use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use neuromancer_core::agent::SubAgentReport;
use neuromancer_core::rpc::{OrchestratorThreadMessage, ThreadEvent, ThreadSummary};
use tokio::sync::Mutex as AsyncMutex;

use crate::orchestrator::error::System0Error;
use crate::orchestrator::security::redaction::{collect_secret_values, sanitize_event_payload};
use crate::orchestrator::tracing::conversation_projection::infer_thread_state_from_events;
use crate::orchestrator::tracing::jsonl_io::{
    append_jsonl_line, read_jsonl_lines, read_thread_events_from_path,
};

const SYSTEM0_AGENT_ID: &str = "system0";

#[derive(Clone)]
pub(crate) struct ThreadJournal {
    base_dir: PathBuf,
    index_file: PathBuf,
    lock: Arc<AsyncMutex<()>>,
    seq_cache: Arc<AsyncMutex<HashMap<String, u64>>>,
    /// Cached secret values used for redaction when writing events.
    secret_values: Arc<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ThreadIndexEntry {
    event_id: String,
    ts: String,
    thread: ThreadSummary,
}

impl ThreadJournal {
    pub(crate) fn new(base_dir: PathBuf) -> Result<Self, System0Error> {
        let system_dir = base_dir.join("system0");
        let subagents_dir = base_dir.join("subagents");
        fs::create_dir_all(&system_dir).map_err(|err| {
            System0Error::Internal(format!(
                "failed to create thread journal system dir '{}': {err}",
                system_dir.display()
            ))
        })?;
        fs::create_dir_all(&subagents_dir).map_err(|err| {
            System0Error::Internal(format!(
                "failed to create thread journal subagent dir '{}': {err}",
                subagents_dir.display()
            ))
        })?;

        let index_file = base_dir.join("index.jsonl");
        if !index_file.exists() {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&index_file)
                .map_err(|err| {
                    System0Error::Internal(format!(
                        "failed to create thread index '{}': {err}",
                        index_file.display()
                    ))
                })?;
        }

        Ok(Self {
            base_dir,
            index_file,
            lock: Arc::new(AsyncMutex::new(())),
            seq_cache: Arc::new(AsyncMutex::new(HashMap::new())),
            secret_values: Arc::new(collect_secret_values()),
        })
    }

    fn system_thread_file(&self) -> PathBuf {
        self.base_dir.join("system0").join("system0.jsonl")
    }

    fn subagent_thread_file(&self, thread_id: &str) -> PathBuf {
        let safe = sanitize_thread_file_component(thread_id);
        self.base_dir
            .join("subagents")
            .join(format!("{safe}.jsonl"))
    }

    fn thread_file_for_id(&self, thread_id: &str) -> PathBuf {
        if thread_id == SYSTEM0_AGENT_ID {
            self.system_thread_file()
        } else {
            self.subagent_thread_file(thread_id)
        }
    }

    fn thread_file_for_event(&self, event: &ThreadEvent) -> PathBuf {
        if event.thread_kind == "system" || event.thread_id == SYSTEM0_AGENT_ID {
            self.system_thread_file()
        } else {
            self.subagent_thread_file(&event.thread_id)
        }
    }

    pub(crate) async fn append_event(&self, mut event: ThreadEvent) -> Result<(), System0Error> {
        let _guard = self.lock.lock().await;
        let path = self.thread_file_for_event(&event);
        let current_seq = self.current_seq_for_locked(&event.thread_id, &path)?;
        event.seq = if event.seq == 0 {
            current_seq + 1
        } else {
            event.seq.max(current_seq + 1)
        };

        let (payload, redacted, safe) = sanitize_event_payload(
            &event.event_type,
            event.payload,
            self.secret_values.as_ref(),
        );
        if safe {
            event.payload = payload;
            event.redaction_applied = event.redaction_applied || redacted;
        } else {
            event.payload = serde_json::json!({ "omitted": "sensitive_payload" });
            event.redaction_applied = true;
        }

        append_jsonl_line(&path, &event)?;
        let mut cache = self.seq_cache.lock().await;
        cache.insert(event.thread_id, event.seq);
        Ok(())
    }

    pub(crate) async fn append_index_snapshot(
        &self,
        summary: &ThreadSummary,
    ) -> Result<(), System0Error> {
        let _guard = self.lock.lock().await;
        let entry = ThreadIndexEntry {
            event_id: uuid::Uuid::new_v4().to_string(),
            ts: now_rfc3339(),
            thread: summary.clone(),
        };
        append_jsonl_line(&self.index_file, &entry)
    }

    pub(crate) fn load_latest_thread_summaries(
        &self,
    ) -> Result<HashMap<String, ThreadSummary>, System0Error> {
        let mut by_thread = HashMap::<String, ThreadSummary>::new();
        if self.index_file.exists() {
            for line in read_jsonl_lines(&self.index_file)? {
                if let Ok(entry) = serde_json::from_str::<ThreadIndexEntry>(&line) {
                    by_thread.insert(entry.thread.thread_id.clone(), entry.thread);
                    continue;
                }
                if let Ok(summary) = serde_json::from_str::<ThreadSummary>(&line) {
                    by_thread.insert(summary.thread_id.clone(), summary);
                }
            }
        }

        let subagents_dir = self.base_dir.join("subagents");
        if subagents_dir.exists() {
            for dir_entry in fs::read_dir(&subagents_dir).map_err(|err| {
                System0Error::Internal(format!(
                    "failed to read subagent thread dir '{}': {err}",
                    subagents_dir.display()
                ))
            })? {
                let Ok(dir_entry) = dir_entry else { continue };
                let path = dir_entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                let events = read_thread_events_from_path(&path)?;
                let Some(last) = events.last() else { continue };
                let thread_id = events
                    .first()
                    .map(|event| event.thread_id.clone())
                    .unwrap_or_else(|| {
                        path.file_stem()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or_default()
                            .to_string()
                    });
                by_thread
                    .entry(thread_id.clone())
                    .or_insert_with(|| ThreadSummary {
                        thread_id,
                        kind: "subagent".to_string(),
                        agent_id: last.agent_id.clone(),
                        latest_run_id: last.run_id.clone(),
                        state: infer_thread_state_from_events(&events),
                        updated_at: last.ts.clone(),
                        resurrected: false,
                        active: false,
                    });
            }
        }

        Ok(by_thread)
    }

    pub(crate) fn read_thread_events(
        &self,
        thread_id: &str,
    ) -> Result<Vec<ThreadEvent>, System0Error> {
        let path = self.thread_file_for_id(thread_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        read_thread_events_from_path(&path)
    }

    pub(crate) fn read_all_thread_events(&self) -> Result<Vec<ThreadEvent>, System0Error> {
        let mut events = Vec::new();

        let system_file = self.system_thread_file();
        if system_file.exists() {
            events.extend(read_thread_events_from_path(&system_file)?);
        }

        let subagents_dir = self.base_dir.join("subagents");
        if subagents_dir.exists() {
            for dir_entry in fs::read_dir(&subagents_dir).map_err(|err| {
                System0Error::Internal(format!(
                    "failed to read subagent thread dir '{}': {err}",
                    subagents_dir.display()
                ))
            })? {
                let Ok(dir_entry) = dir_entry else { continue };
                let path = dir_entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                events.extend(read_thread_events_from_path(&path)?);
            }
        }

        events.sort_by(|a, b| {
            if a.ts == b.ts {
                a.seq.cmp(&b.seq)
            } else {
                a.ts.cmp(&b.ts)
            }
        });
        Ok(events)
    }

    pub(crate) async fn append_messages(
        &self,
        thread_id: &str,
        thread_kind: &str,
        agent_id: Option<&str>,
        run_id: Option<&str>,
        messages: &[OrchestratorThreadMessage],
    ) -> Result<(), System0Error> {
        for message in messages {
            let (event_type, payload) = match message {
                OrchestratorThreadMessage::Text { role, content } => {
                    let event_type = if role == "user" {
                        "message_user"
                    } else if role == "assistant" {
                        "message_assistant"
                    } else {
                        "message_system"
                    };
                    (
                        event_type.to_string(),
                        serde_json::json!({ "role": role, "content": content }),
                    )
                }
                OrchestratorThreadMessage::ToolInvocation {
                    call_id,
                    tool_id,
                    arguments,
                    status,
                    output,
                } => (
                    "tool_result".to_string(),
                    serde_json::json!({
                        "call_id": call_id,
                        "tool_id": tool_id,
                        "arguments": arguments,
                        "status": status,
                        "output": output,
                    }),
                ),
            };

            self.append_event(make_event(
                thread_id,
                thread_kind,
                event_type,
                agent_id.map(str::to_string),
                run_id.map(str::to_string),
                payload,
            ))
            .await?;
        }
        Ok(())
    }

    fn current_seq_for_locked(&self, thread_id: &str, path: &Path) -> Result<u64, System0Error> {
        if let Some(seq) = self
            .seq_cache
            .try_lock()
            .ok()
            .and_then(|cache| cache.get(thread_id).copied())
        {
            return Ok(seq);
        }

        if !path.exists() {
            return Ok(0);
        }
        let mut max_seq = 0_u64;
        for line in read_jsonl_lines(path)? {
            let Ok(event) = serde_json::from_str::<ThreadEvent>(&line) else {
                continue;
            };
            max_seq = max_seq.max(event.seq);
        }
        Ok(max_seq)
    }
}

pub(crate) fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Build a `ThreadEvent` with sensible defaults for the common fields.
///
/// Callers can override `turn_id`, `call_id`, `duration_ms`, etc. on the
/// returned struct before passing it to `ThreadJournal::append_event`.
pub(crate) fn make_event(
    thread_id: impl Into<String>,
    thread_kind: impl Into<String>,
    event_type: impl Into<String>,
    agent_id: Option<String>,
    run_id: Option<String>,
    payload: serde_json::Value,
) -> ThreadEvent {
    ThreadEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        thread_id: thread_id.into(),
        thread_kind: thread_kind.into(),
        seq: 0,
        ts: now_rfc3339(),
        event_type: event_type.into(),
        agent_id,
        run_id,
        payload,
        redaction_applied: false,
        turn_id: None,
        parent_event_id: None,
        call_id: None,
        attempt: None,
        duration_ms: None,
        meta: None,
    }
}

pub(crate) fn sanitize_thread_file_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "thread".to_string()
    } else {
        out
    }
}

pub(crate) fn subagent_report_type(report: &SubAgentReport) -> &'static str {
    match report {
        SubAgentReport::Progress { .. } => "progress",
        SubAgentReport::InputRequired { .. } => "input_required",
        SubAgentReport::ToolFailure { .. } => "tool_failure",
        SubAgentReport::PolicyDenied { .. } => "policy_denied",
        SubAgentReport::Stuck { .. } => "stuck",
        SubAgentReport::Completed { .. } => "completed",
        SubAgentReport::Failed { .. } => "failed",
    }
}

pub(crate) fn subagent_report_task_id(report: &SubAgentReport) -> String {
    match report {
        SubAgentReport::Progress { task_id, .. }
        | SubAgentReport::InputRequired { task_id, .. }
        | SubAgentReport::ToolFailure { task_id, .. }
        | SubAgentReport::PolicyDenied { task_id, .. }
        | SubAgentReport::Stuck { task_id, .. }
        | SubAgentReport::Completed { task_id, .. }
        | SubAgentReport::Failed { task_id, .. } => task_id.to_string(),
    }
}
