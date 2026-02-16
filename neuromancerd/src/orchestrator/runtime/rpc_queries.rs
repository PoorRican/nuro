use std::collections::BTreeMap;

use neuromancer_core::rpc::{
    DelegatedRun, OrchestratorEventsQueryParams, OrchestratorEventsQueryResult,
    OrchestratorOutputsPullResult, OrchestratorRunDiagnoseResult, OrchestratorStatsGetResult,
    OrchestratorThreadGetParams, OrchestratorThreadGetResult, OrchestratorThreadMessage,
    OrchestratorThreadResurrectResult, ThreadSummary,
};

use crate::orchestrator::error::System0Error;
use crate::orchestrator::state::{SYSTEM0_AGENT_ID, SubAgentThreadState};
use crate::orchestrator::tracing::conversation_projection::{
    conversation_to_thread_messages, reconstruct_subagent_conversation,
    thread_events_to_thread_messages,
};
use crate::orchestrator::tracing::event_query::{
    event_error_text, event_matches_query, event_tool_id,
};
use crate::orchestrator::tracing::thread_journal::{make_event, now_rfc3339};

use super::System0Runtime;

const DEFAULT_THREAD_PAGE_LIMIT: usize = 100;
const DEFAULT_EVENTS_QUERY_LIMIT: usize = 200;

impl System0Runtime {
    pub async fn runs_list(
        &self,
    ) -> Result<Vec<DelegatedRun>, System0Error> {
        Ok(self.system0_broker.list_runs().await)
    }

    pub async fn outputs_pull(
        &self,
        limit: Option<usize>,
    ) -> Result<OrchestratorOutputsPullResult, System0Error> {
        let limit = limit.unwrap_or(100).clamp(1, 1_000);
        let (outputs, remaining) = self.system0_broker.pull_outputs(limit).await;
        Ok(OrchestratorOutputsPullResult { outputs, remaining })
    }

    pub async fn run_get(
        &self,
        run_id: String,
    ) -> Result<DelegatedRun, System0Error> {
        if run_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "run_id must not be empty".to_string(),
            ));
        }

        self.system0_broker
            .get_run(&run_id)
            .await
            .ok_or_else(|| System0Error::ResourceNotFound(format!("run '{run_id}'")))
    }

    pub async fn context_get(
        &self,
    ) -> Result<Vec<OrchestratorThreadMessage>, System0Error> {
        let events = self.thread_journal.read_thread_events(SYSTEM0_AGENT_ID)?;
        Ok(thread_events_to_thread_messages(&events))
    }

    pub async fn threads_list(
        &self,
    ) -> Result<Vec<ThreadSummary>, System0Error> {
        let mut summaries = self.thread_journal.load_latest_thread_summaries()?;
        summaries
            .entry(SYSTEM0_AGENT_ID.to_string())
            .or_insert_with(|| ThreadSummary {
                thread_id: SYSTEM0_AGENT_ID.to_string(),
                kind: "system".to_string(),
                agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                latest_run_id: None,
                state: "active".to_string(),
                updated_at: now_rfc3339(),
                resurrected: false,
                active: true,
            });

        let active_threads = self.system0_broker.list_thread_states().await;
        for state in active_threads {
            summaries.insert(
                state.thread_id.clone(),
                ThreadSummary {
                    thread_id: state.thread_id,
                    kind: "subagent".to_string(),
                    agent_id: Some(state.agent_id),
                    latest_run_id: state.latest_run_id,
                    state: state.state,
                    updated_at: state.updated_at,
                    resurrected: state.resurrected,
                    active: state.active,
                },
            );
        }

        let mut out = summaries.into_values().collect::<Vec<_>>();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    }

    pub async fn thread_get(
        &self,
        params: OrchestratorThreadGetParams,
    ) -> Result<OrchestratorThreadGetResult, System0Error> {
        if params.thread_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "thread_id must not be empty".to_string(),
            ));
        }
        let offset = params.offset.unwrap_or(0);
        let limit = params
            .limit
            .unwrap_or(DEFAULT_THREAD_PAGE_LIMIT)
            .clamp(1, 500);

        let events = self.thread_journal.read_thread_events(&params.thread_id)?;
        let total = events.len();
        let page = events
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();

        Ok(OrchestratorThreadGetResult {
            thread_id: params.thread_id,
            events: page,
            total,
            offset,
            limit,
        })
    }

    pub async fn thread_resurrect(
        &self,
        thread_id: String,
    ) -> Result<OrchestratorThreadResurrectResult, System0Error> {
        if thread_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "thread_id must not be empty".to_string(),
            ));
        }
        if thread_id == SYSTEM0_AGENT_ID {
            return Ok(OrchestratorThreadResurrectResult {
                thread: ThreadSummary {
                    thread_id: SYSTEM0_AGENT_ID.to_string(),
                    kind: "system".to_string(),
                    agent_id: Some(SYSTEM0_AGENT_ID.to_string()),
                    latest_run_id: None,
                    state: "active".to_string(),
                    updated_at: now_rfc3339(),
                    resurrected: false,
                    active: true,
                },
            });
        }

        let summaries = self.thread_journal.load_latest_thread_summaries()?;
        let Some(summary) = summaries.get(&thread_id).cloned() else {
            return Err(System0Error::ResourceNotFound(format!(
                "thread '{thread_id}'"
            )));
        };
        let Some(agent_id) = summary.agent_id.clone() else {
            return Err(System0Error::InvalidRequest(format!(
                "thread '{thread_id}' is not a sub-agent thread"
            )));
        };

        let _runtime = self
            .system0_broker
            .runtime_for_agent(&agent_id)
            .await
            .ok_or_else(|| {
                System0Error::ResourceNotFound(format!(
                    "agent '{agent_id}' for thread '{thread_id}'"
                ))
            })?;

        let events = self.thread_journal.read_thread_events(&thread_id)?;
        let session_id = uuid::Uuid::new_v4();
        let reconstructed = reconstruct_subagent_conversation(&events);
        self.system0_broker
            .session_store()
            .await
            .save_conversation(session_id, reconstructed.clone())
            .await;

        let thread_state = SubAgentThreadState {
            thread_id: thread_id.clone(),
            agent_id: agent_id.clone(),
            session_id,
            latest_run_id: summary.latest_run_id.clone(),
            state: "active".to_string(),
            summary: None,
            initial_instruction: None,
            resurrected: true,
            active: true,
            updated_at: now_rfc3339(),
            persisted_message_count: conversation_to_thread_messages(&reconstructed.messages).len(),
        };
        self.system0_broker.upsert_thread_state(thread_state).await;

        let resurrected = ThreadSummary {
            resurrected: true,
            active: true,
            updated_at: now_rfc3339(),
            ..summary
        };
        self.thread_journal
            .append_index_snapshot(&resurrected)
            .await?;
        self.thread_journal
            .append_event(make_event(
                thread_id.clone(),
                "subagent",
                "thread_resurrected",
                Some(agent_id),
                resurrected.latest_run_id.clone(),
                serde_json::json!({"resurrected": true}),
            ))
            .await?;

        Ok(OrchestratorThreadResurrectResult {
            thread: resurrected,
        })
    }

    pub async fn events_query(
        &self,
        params: OrchestratorEventsQueryParams,
    ) -> Result<OrchestratorEventsQueryResult, System0Error> {
        let offset = params.offset.unwrap_or(0);
        let limit = params
            .limit
            .unwrap_or(DEFAULT_EVENTS_QUERY_LIMIT)
            .clamp(1, 1000);

        let mut events = self.thread_journal.read_all_thread_events()?;
        events.retain(|event| event_matches_query(event, &params));

        let total = events.len();
        let events = events
            .into_iter()
            .skip(offset)
            .take(limit)
            .collect::<Vec<_>>();

        Ok(OrchestratorEventsQueryResult {
            events,
            total,
            offset,
            limit,
        })
    }

    pub async fn run_diagnose(
        &self,
        run_id: String,
    ) -> Result<OrchestratorRunDiagnoseResult, System0Error> {
        if run_id.trim().is_empty() {
            return Err(System0Error::InvalidRequest(
                "run_id must not be empty".to_string(),
            ));
        }

        let run =
            self.system0_broker.get_run(&run_id).await.ok_or_else(|| {
                System0Error::ResourceNotFound(format!("run '{run_id}'"))
            })?;

        let all_events = self.thread_journal.read_all_thread_events()?;
        let mut events = all_events
            .iter()
            .filter(|event| event.run_id.as_deref() == Some(run_id.as_str()))
            .cloned()
            .collect::<Vec<_>>();

        if events.is_empty()
            && let Some(thread_id) = run.thread_id.as_deref()
        {
            events = self
                .thread_journal
                .read_thread_events(thread_id)?
                .into_iter()
                .filter(|event| event.run_id.as_deref() == Some(run_id.as_str()))
                .collect();
        }

        let thread = if let Some(thread_id) = run.thread_id.as_deref() {
            let summaries = self.threads_list().await?;
            summaries
                .into_iter()
                .find(|summary| summary.thread_id == thread_id)
        } else {
            None
        };

        let inferred_failure_cause = events.iter().rev().find_map(event_error_text);

        Ok(OrchestratorRunDiagnoseResult {
            run,
            thread,
            events,
            inferred_failure_cause,
        })
    }

    pub async fn stats_get(
        &self,
    ) -> Result<OrchestratorStatsGetResult, System0Error> {
        let events = self.thread_journal.read_all_thread_events()?;
        let runs = self.system0_broker.list_runs().await;
        let threads_total = self.threads_list().await?.len();

        let mut event_counts = BTreeMap::<String, usize>::new();
        let mut tool_counts = BTreeMap::<String, usize>::new();
        let mut agent_counts = BTreeMap::<String, usize>::new();
        for event in &events {
            *event_counts.entry(event.event_type.clone()).or_default() += 1;
            if let Some(tool_id) = event_tool_id(event) {
                *tool_counts.entry(tool_id).or_default() += 1;
            }
            if let Some(agent_id) = event.agent_id.clone() {
                *agent_counts.entry(agent_id).or_default() += 1;
            }
        }

        let failed_runs = runs
            .iter()
            .filter(|run| run.state == "failed" || run.state == "error")
            .count();

        Ok(OrchestratorStatsGetResult {
            threads_total,
            events_total: events.len(),
            runs_total: runs.len(),
            failed_runs,
            event_counts,
            tool_counts,
            agent_counts,
        })
    }
}
