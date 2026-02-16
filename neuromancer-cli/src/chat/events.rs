use std::collections::HashMap;

use neuromancer_core::rpc::{OrchestratorThreadGetParams, ThreadEvent};

use crate::CliError;
use crate::rpc_client::RpcClient;

use super::app::ThreadKind;
use super::timeline::{MessageRoleTag, TimelineItem, TimelineRenderMeta};

const DEFAULT_THREAD_PAGE_LIMIT: usize = 200;

pub(super) async fn fetch_all_thread_events(
    rpc: &RpcClient,
    thread_id: &str,
) -> Result<Vec<ThreadEvent>, CliError> {
    let mut offset = 0usize;
    let mut all = Vec::new();

    loop {
        let page = rpc
            .orchestrator_thread_get(OrchestratorThreadGetParams {
                thread_id: thread_id.to_string(),
                offset: Some(offset),
                limit: Some(DEFAULT_THREAD_PAGE_LIMIT),
            })
            .await?;

        if page.events.is_empty() {
            break;
        }

        offset += page.events.len();
        all.extend(page.events);
        if all.len() >= page.total {
            break;
        }
    }

    all.sort_by(|a, b| {
        if a.seq == b.seq {
            a.ts.cmp(&b.ts)
        } else {
            a.seq.cmp(&b.seq)
        }
    });
    Ok(all)
}

pub(super) fn timeline_items_from_events(
    kind: &ThreadKind,
    events: &[ThreadEvent],
) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    let mut pending_tool_calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();
    let mut saw_user = false;

    for event in events {
        match event.event_type.as_str() {
            "message_user" => {
                if let Some(content) = event
                    .payload
                    .get("content")
                    .and_then(|value| value.as_str())
                    .or_else(|| event.payload.as_str())
                {
                    items.push(TimelineItem::text(
                        MessageRoleTag::User,
                        content.to_string(),
                    ));
                    saw_user = true;
                }
            }
            "message_assistant" => {
                if let Some(content) = event
                    .payload
                    .get("content")
                    .and_then(|value| value.as_str())
                    .or_else(|| event.payload.as_str())
                {
                    items.push(TimelineItem::text(
                        MessageRoleTag::Assistant,
                        content.to_string(),
                    ));
                }
            }
            "tool_call" => {
                let Some(call_id) = event
                    .payload
                    .get("call_id")
                    .and_then(|value| value.as_str())
                else {
                    continue;
                };
                let tool_id = event
                    .payload
                    .get("tool_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let arguments = event
                    .payload
                    .get("arguments")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                pending_tool_calls.insert(call_id.to_string(), (tool_id, arguments));
            }
            "tool_result" => {
                let Some(call_id) = event
                    .payload
                    .get("call_id")
                    .and_then(|value| value.as_str())
                else {
                    continue;
                };
                let (tool_id, arguments) =
                    pending_tool_calls.remove(call_id).unwrap_or_else(|| {
                        (
                            event
                                .payload
                                .get("tool_id")
                                .and_then(|value| value.as_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            event
                                .payload
                                .get("arguments")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                        )
                    });
                let status = event
                    .payload
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("success")
                    .to_string();
                let output = event
                    .payload
                    .get("output")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                items.push(tool_item_from_parts(
                    call_id.to_string(),
                    tool_id,
                    status,
                    arguments,
                    output,
                    Some(TimelineRenderMeta {
                        seq: Some(event.seq),
                        ts: Some(event.ts.clone()),
                        redaction_applied: Some(event.redaction_applied),
                    }),
                ));
            }
            "thread_created" => {
                let agent_id = event
                    .payload
                    .get("agent_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                items.push(TimelineItem::text(
                    MessageRoleTag::System,
                    format!("Thread created for sub-agent '{agent_id}'"),
                ));
            }
            "thread_resurrected" => {
                items.push(TimelineItem::text(
                    MessageRoleTag::System,
                    "Thread resurrected and ready for continuation.",
                ));
            }
            "run_state_changed" => {
                let state = event
                    .payload
                    .get("state")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                let summary = event
                    .payload
                    .get("summary")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                let error = event
                    .payload
                    .get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");

                let mut message = format!("Run state changed: {state}");
                if !summary.is_empty() {
                    message.push_str(&format!(" | summary: {summary}"));
                }
                if !error.is_empty() {
                    message.push_str(&format!(" | error: {error}"));
                }
                items.push(TimelineItem::text(MessageRoleTag::System, message));
            }
            "subagent_report" => {
                let report_type = event
                    .payload
                    .get("report_type")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                let summary = event
                    .payload
                    .get("report")
                    .and_then(|value| value.get("description"))
                    .and_then(|value| value.as_str())
                    .or_else(|| {
                        event
                            .payload
                            .get("report")
                            .and_then(|value| value.get("summary"))
                            .and_then(|value| value.as_str())
                    })
                    .unwrap_or("report available");
                items.push(TimelineItem::text(
                    MessageRoleTag::System,
                    format!("SUB-AGENT REPORT [{report_type}] {summary}"),
                ));
            }
            "error" => {
                let error = event
                    .payload
                    .get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown error");
                let tool_id = event
                    .payload
                    .get("tool_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("runtime");
                let call_id = event
                    .payload
                    .get("call_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("n/a");
                items.push(TimelineItem::text(
                    MessageRoleTag::System,
                    format!("SYSTEM ERROR [{tool_id}::{call_id}] {error}"),
                ));
            }
            _ => {}
        }
    }

    for (call_id, (tool_id, arguments)) in pending_tool_calls {
        items.push(tool_item_from_parts(
            call_id,
            tool_id,
            "pending".to_string(),
            arguments,
            serde_json::Value::Null,
            None,
        ));
    }

    if matches!(kind, ThreadKind::Subagent) && !saw_user {
        items.insert(
            0,
            TimelineItem::text(
                MessageRoleTag::System,
                "Initial instruction unavailable (legacy run format)",
            ),
        );
    }

    items
}

fn tool_item_from_parts(
    call_id: String,
    tool_id: String,
    status: String,
    arguments: serde_json::Value,
    output: serde_json::Value,
    meta: Option<TimelineRenderMeta>,
) -> TimelineItem {
    if tool_id == "delegate_to_agent" {
        let target_agent = arguments
            .get("agent_id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .or_else(|| {
                output
                    .get("agent_id")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string())
            });
        let thread_id = output
            .get("thread_id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let run_id = output
            .get("run_id")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let instruction = arguments
            .get("instruction")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let summary = output
            .get("summary")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string());
        let error = output
            .get("error")
            .and_then(|value| value.as_str())
            .map(|value| value.to_string())
            .or_else(|| {
                if status == "error" {
                    Some(output.to_string())
                } else {
                    None
                }
            });

        TimelineItem::DelegateInvocation {
            call_id,
            status,
            target_agent,
            thread_id,
            run_id,
            instruction,
            summary,
            error,
            arguments,
            output,
            meta,
            expanded: false,
        }
    } else {
        TimelineItem::ToolInvocation {
            call_id,
            tool_id,
            status,
            arguments,
            output,
            meta,
            expanded: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use neuromancer_core::rpc::ThreadEvent;

    use super::super::app::ThreadKind;
    use super::super::timeline::{MessageRoleTag, TimelineItem};
    use super::timeline_items_from_events;

    #[test]
    fn timeline_adds_legacy_instruction_fallback_for_subagent_threads() {
        let events = vec![ThreadEvent {
            event_id: "e1".to_string(),
            thread_id: "thread-1".to_string(),
            thread_kind: "subagent".to_string(),
            seq: 1,
            ts: "2026-02-14T00:00:00Z".to_string(),
            event_type: "message_assistant".to_string(),
            agent_id: Some("planner".to_string()),
            run_id: Some("run-1".to_string()),
            payload: serde_json::json!({"content":"hello"}),
            redaction_applied: false,
            turn_id: None,
            parent_event_id: None,
            call_id: None,
            attempt: None,
            duration_ms: None,
            meta: None,
        }];

        let items = timeline_items_from_events(&ThreadKind::Subagent, &events);
        assert!(matches!(
            &items[0],
            TimelineItem::Text { role: MessageRoleTag::System, text, .. }
                if text.contains("Initial instruction unavailable")
        ));
    }
}
