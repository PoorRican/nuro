use std::collections::{BTreeMap, HashMap};

use neuromancer_core::rpc::{
    DelegatedRun, OrchestratorEventsQueryParams, OrchestratorEventsQueryResult,
    OrchestratorOutputsPullResult, OrchestratorReportRecord,
    OrchestratorReportsQueryParams, OrchestratorReportsQueryResult, OrchestratorRunDiagnoseResult,
    OrchestratorStatsGetResult,
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
const DEFAULT_REPORTS_QUERY_LIMIT: usize = 200;

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

    pub async fn reports_query(
        &self,
        params: OrchestratorReportsQueryParams,
    ) -> Result<OrchestratorReportsQueryResult, System0Error> {
        let offset = params.offset.unwrap_or(0);
        let limit = params
            .limit
            .unwrap_or(DEFAULT_REPORTS_QUERY_LIMIT)
            .clamp(1, 1000);
        let include_remediation = params.include_remediation.unwrap_or(true);
        let report_type_filter = params
            .report_type
            .as_deref()
            .map(|value| value.to_ascii_lowercase());

        let events = self.thread_journal.read_all_thread_events()?;
        let mut remediation_by_key =
            HashMap::<(String, Option<String>, String), serde_json::Value>::new();
        if include_remediation {
            for event in &events {
                if event.event_type != "remediation_action" {
                    continue;
                }
                let Some(source_report_type) = event
                    .payload
                    .get("source_report_type")
                    .and_then(|value| value.as_str())
                else {
                    continue;
                };
                remediation_by_key.insert(
                    (
                        event.thread_id.clone(),
                        event.run_id.clone(),
                        source_report_type.to_ascii_lowercase(),
                    ),
                    event.payload.clone(),
                );
            }
        }

        let mut reports = Vec::<OrchestratorReportRecord>::new();
        for event in events {
            if event.event_type != "subagent_report" {
                continue;
            }

            if let Some(thread_id) = params.thread_id.as_deref()
                && event.thread_id != thread_id
            {
                continue;
            }
            if let Some(run_id) = params.run_id.as_deref()
                && event.run_id.as_deref() != Some(run_id)
            {
                continue;
            }

            let source_agent_id = event
                .payload
                .get("source_agent_id")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);
            if let Some(agent_id) = params.agent_id.as_deref()
                && event.agent_id.as_deref() != Some(agent_id)
                && source_agent_id.as_deref() != Some(agent_id)
            {
                continue;
            }

            let report_type = event
                .payload
                .get("report_type")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown")
                .to_ascii_lowercase();
            if let Some(filter) = report_type_filter.as_deref()
                && report_type != filter
            {
                continue;
            }

            let source_thread_id = event
                .payload
                .get("source_thread_id")
                .and_then(|value| value.as_str())
                .map(ToString::to_string);
            let report = event
                .payload
                .get("report")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let remediation_action = if include_remediation {
                remediation_by_key
                    .get(&(event.thread_id.clone(), event.run_id.clone(), report_type.clone()))
                    .cloned()
                    .or_else(|| {
                        remediation_by_key
                            .iter()
                            .find_map(|((_, run_id, kind), payload)| {
                                if *run_id == event.run_id && *kind == report_type {
                                    Some(payload.clone())
                                } else {
                                    None
                                }
                            })
                    })
            } else {
                None
            };

            reports.push(OrchestratorReportRecord {
                event_id: event.event_id,
                ts: event.ts,
                thread_id: event.thread_id,
                run_id: event.run_id,
                agent_id: event.agent_id,
                source_thread_id,
                source_agent_id,
                report_type,
                report,
                remediation_action,
            });
        }

        reports.sort_by(|a, b| {
            if a.ts == b.ts {
                a.event_id.cmp(&b.event_id)
            } else {
                a.ts.cmp(&b.ts)
            }
        });

        let total = reports.len();
        let reports = reports.into_iter().skip(offset).take(limit).collect::<Vec<_>>();

        Ok(OrchestratorReportsQueryResult {
            reports,
            total,
            offset,
            limit,
        })
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

        let snapshot = self.system0_broker.last_report_for_run(&run_id).await;
        let latest_report_type = snapshot
            .as_ref()
            .map(|value| value.report_type.clone())
            .or_else(|| {
                events.iter().rev().find_map(|event| {
                    if event.event_type != "subagent_report" {
                        return None;
                    }
                    event.payload
                        .get("report_type")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string)
                })
            });
        let latest_report = snapshot.as_ref().map(|value| value.report.clone()).or_else(|| {
            events.iter().rev().find_map(|event| {
                if event.event_type != "subagent_report" {
                    return None;
                }
                event.payload.get("report").cloned()
            })
        });
        let recommended_remediation = snapshot
            .as_ref()
            .and_then(|value| value.recommended_remediation.clone())
            .or_else(|| {
                events.iter().rev().find_map(|event| {
                    if event.event_type != "remediation_action" {
                        return None;
                    }
                    event
                        .payload
                        .get("recommended_remediation")
                        .cloned()
                        .or_else(|| event.payload.get("action_payload").cloned())
                })
            });

        let inferred_failure_cause = latest_report_type
            .as_deref()
            .and_then(|report_type| {
                latest_report
                    .as_ref()
                    .and_then(|report| infer_failure_from_subagent_report(report_type, report))
            })
            .or_else(|| events.iter().rev().find_map(event_error_text));

        Ok(OrchestratorRunDiagnoseResult {
            run,
            thread,
            events,
            inferred_failure_cause,
            latest_report_type,
            latest_report,
            recommended_remediation,
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
        let mut subagent_report_counts = BTreeMap::<String, usize>::new();
        let mut remediation_action_counts = BTreeMap::<String, usize>::new();
        for event in &events {
            *event_counts.entry(event.event_type.clone()).or_default() += 1;
            if let Some(tool_id) = event_tool_id(event) {
                *tool_counts.entry(tool_id).or_default() += 1;
            }
            if let Some(agent_id) = event.agent_id.clone() {
                *agent_counts.entry(agent_id).or_default() += 1;
            }
            if event.event_type == "subagent_report"
                && let Some(report_type) =
                    event.payload.get("report_type").and_then(|value| value.as_str())
            {
                *subagent_report_counts
                    .entry(report_type.to_ascii_lowercase())
                    .or_default() += 1;
            }
            if event.event_type == "remediation_action"
                && let Some(action) = event.payload.get("action").and_then(|value| value.as_str())
            {
                *remediation_action_counts
                    .entry(action.to_ascii_lowercase())
                    .or_default() += 1;
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
            subagent_report_counts,
            remediation_action_counts,
        })
    }
}

fn infer_failure_from_subagent_report(report_type: &str, report: &serde_json::Value) -> Option<String> {
    match report_type {
        "stuck" => report
            .get("reason")
            .and_then(|value| value.as_str())
            .map(|value| format!("sub-agent stuck: {value}")),
        "tool_failure" => {
            let tool_id = report.get("tool_id").and_then(|value| value.as_str());
            let error = report.get("error").and_then(|value| value.as_str());
            match (tool_id, error) {
                (Some(tool), Some(error)) => Some(format!("tool failure ({tool}): {error}")),
                (None, Some(error)) => Some(error.to_string()),
                _ => None,
            }
        }
        "policy_denied" => {
            let code = report.get("policy_code").and_then(|value| value.as_str());
            let capability = report
                .get("capability_needed")
                .and_then(|value| value.as_str());
            match (code, capability) {
                (Some(code), Some(capability)) => {
                    Some(format!("policy denied ({code}): {capability}"))
                }
                (_, Some(capability)) => Some(format!("policy denied: {capability}")),
                _ => None,
            }
        }
        "input_required" => report
            .get("question")
            .and_then(|value| value.as_str())
            .map(|value| format!("input required: {value}")),
        "failed" => report
            .get("error")
            .and_then(|value| value.as_str())
            .map(ToString::to_string),
        _ => None,
    }
}
