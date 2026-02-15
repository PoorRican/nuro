use std::collections::HashMap;

use neuromancer_agent::conversation::{ChatMessage, ConversationContext, TruncationStrategy};
use neuromancer_core::rpc::{OrchestratorThreadMessage, ThreadEvent};
use neuromancer_core::tool::{ToolOutput, ToolResult};

pub(crate) fn conversation_to_thread_messages(
    messages: &[neuromancer_agent::conversation::ChatMessage],
) -> Vec<OrchestratorThreadMessage> {
    use neuromancer_agent::conversation::{MessageContent, MessageRole};

    let mut result = Vec::new();
    let mut pending_tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();

    for msg in messages {
        match (&msg.role, &msg.content) {
            (MessageRole::System, _) => {}
            (MessageRole::User, MessageContent::Text(text)) => {
                flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
                result.push(OrchestratorThreadMessage::Text {
                    role: "user".to_string(),
                    content: text.clone(),
                });
            }
            (MessageRole::Assistant, MessageContent::Text(text)) => {
                flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
                result.push(OrchestratorThreadMessage::Text {
                    role: "assistant".to_string(),
                    content: text.clone(),
                });
            }
            (MessageRole::Assistant, MessageContent::ToolCalls(calls)) => {
                flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
                for call in calls {
                    pending_tool_calls.push((
                        call.id.clone(),
                        call.tool_id.clone(),
                        call.arguments.clone(),
                    ));
                }
            }
            (MessageRole::Tool, MessageContent::ToolResult(tool_result)) => {
                if let Some(pos) = pending_tool_calls
                    .iter()
                    .position(|(id, _, _)| *id == tool_result.call_id)
                {
                    let (call_id, tool_id, arguments) = pending_tool_calls.remove(pos);
                    let (status, output) = match &tool_result.output {
                        ToolOutput::Success(v) => ("success".to_string(), v.clone()),
                        ToolOutput::Error(e) => {
                            ("error".to_string(), serde_json::Value::String(e.clone()))
                        }
                    };
                    result.push(OrchestratorThreadMessage::ToolInvocation {
                        call_id,
                        tool_id,
                        arguments,
                        status,
                        output,
                    });
                }
            }
            _ => {}
        }
    }

    flush_pending_tool_calls(&mut pending_tool_calls, &mut result);
    result
}

fn flush_pending_tool_calls(
    pending: &mut Vec<(String, String, serde_json::Value)>,
    result: &mut Vec<OrchestratorThreadMessage>,
) {
    for (call_id, tool_id, arguments) in pending.drain(..) {
        result.push(OrchestratorThreadMessage::ToolInvocation {
            call_id,
            tool_id,
            arguments,
            status: "pending".to_string(),
            output: serde_json::Value::Null,
        });
    }
}

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

pub(crate) fn reconstruct_subagent_conversation(events: &[ThreadEvent]) -> ConversationContext {
    let mut conversation = ConversationContext::new(
        u32::MAX,
        TruncationStrategy::SlidingWindow { keep_last: 50 },
    );

    for message in thread_events_to_thread_messages(events) {
        match message {
            OrchestratorThreadMessage::Text { role, content } => {
                if role == "user" {
                    conversation.add_message(ChatMessage::user(content));
                } else if role == "assistant" {
                    conversation.add_message(ChatMessage::assistant_text(content));
                }
            }
            OrchestratorThreadMessage::ToolInvocation {
                call_id,
                tool_id: _,
                arguments: _,
                status,
                output,
            } => {
                let tool_output = if status == "error" {
                    let text = output
                        .get("error")
                        .and_then(|value| value.as_str())
                        .map(ToString::to_string)
                        .unwrap_or_else(|| output.to_string());
                    ToolOutput::Error(text)
                } else {
                    ToolOutput::Success(output)
                };
                conversation.add_message(ChatMessage::tool_result(ToolResult {
                    call_id,
                    output: tool_output,
                }));
            }
        }
    }

    conversation
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
