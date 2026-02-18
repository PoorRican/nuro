use std::collections::HashMap;

use neuromancer_core::rpc::{OrchestratorThreadMessage, ThreadEvent};

pub(crate) fn infer_thread_state_from_events(events: &[ThreadEvent]) -> String {
    for event in events.iter().rev() {
        if event.event_type == "error" {
            return "failed".to_string();
        }
        if event.event_type == "run_state_changed"
            && let Some(state) = event.payload.get("state").and_then(|value| value.as_str())
        {
            return state.to_string();
        }
    }
    "completed".to_string()
}

pub(crate) fn normalize_error_message(message: impl Into<String>) -> String {
    let mut text = message.into().replace('\n', " ");
    if text.len() > 512 {
        text.truncate(512);
        text.push_str("...");
    }
    text
}

pub(crate) fn thread_events_to_thread_messages(
    events: &[ThreadEvent],
) -> Vec<OrchestratorThreadMessage> {
    let mut out = Vec::new();
    let mut pending_tool_calls: HashMap<String, (String, serde_json::Value)> = HashMap::new();

    for event in events {
        match event.event_type.as_str() {
            "message_user" => {
                if let Some(content) = event
                    .payload
                    .get("content")
                    .and_then(|value| value.as_str())
                {
                    out.push(OrchestratorThreadMessage::Text {
                        role: "user".to_string(),
                        content: content.to_string(),
                    });
                }
            }
            "message_assistant" => {
                if let Some(content) = event
                    .payload
                    .get("content")
                    .and_then(|value| value.as_str())
                {
                    out.push(OrchestratorThreadMessage::Text {
                        role: "assistant".to_string(),
                        content: content.to_string(),
                    });
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
                out.push(OrchestratorThreadMessage::ToolInvocation {
                    call_id: call_id.to_string(),
                    tool_id,
                    arguments,
                    status,
                    output,
                });
            }
            "error" => {
                let err = event
                    .payload
                    .get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown runtime error");
                out.push(OrchestratorThreadMessage::Text {
                    role: "system".to_string(),
                    content: format!("SYSTEM ERROR: {err}"),
                });
            }
            _ => {}
        }
    }

    for (call_id, (tool_id, arguments)) in pending_tool_calls {
        out.push(OrchestratorThreadMessage::ToolInvocation {
            call_id,
            tool_id,
            arguments,
            status: "pending".to_string(),
            output: serde_json::Value::Null,
        });
    }

    out
}
