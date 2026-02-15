use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const TEXT_COLLAPSE_LINE_THRESHOLD: usize = 4;
const TEXT_COLLAPSE_CHAR_THRESHOLD: usize = 280;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageRoleTag {
    System,
    User,
    Assistant,
}

impl MessageRoleTag {
    fn label(self) -> &'static str {
        match self {
            Self::System => "SYSTEM",
            Self::User => "USER",
            Self::Assistant => "ASSISTANT",
        }
    }

    fn badge_style(self) -> Style {
        match self {
            Self::System => Style::default().fg(Color::Cyan),
            Self::User => Style::default().fg(Color::Green),
            Self::Assistant => Style::default().fg(Color::Blue),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct TimelineRenderMeta {
    pub(super) seq: Option<u64>,
    pub(super) ts: Option<String>,
    pub(super) redaction_applied: Option<bool>,
}

#[derive(Debug, Clone)]
pub(super) enum TimelineItem {
    Text {
        role: MessageRoleTag,
        text: String,
        expanded: bool,
    },
    ToolInvocation {
        call_id: String,
        tool_id: String,
        status: String,
        arguments: serde_json::Value,
        output: serde_json::Value,
        meta: Option<TimelineRenderMeta>,
        expanded: bool,
    },
    DelegateInvocation {
        call_id: String,
        status: String,
        target_agent: Option<String>,
        thread_id: Option<String>,
        run_id: Option<String>,
        instruction: Option<String>,
        summary: Option<String>,
        error: Option<String>,
        arguments: serde_json::Value,
        output: serde_json::Value,
        meta: Option<TimelineRenderMeta>,
        expanded: bool,
    },
}

impl TimelineItem {
    pub(super) fn text(role: MessageRoleTag, text: impl Into<String>) -> Self {
        let text = text.into();
        let expanded = !text_is_collapsible(&text);
        Self::Text {
            role,
            text,
            expanded,
        }
    }

    pub(super) fn lines(
        &self,
        selected: bool,
        assistant_label: &str,
        selected_fill_width: Option<usize>,
    ) -> Vec<Line<'static>> {
        let mut lines = match self {
            TimelineItem::Text {
                role,
                text,
                expanded,
            } => {
                let mut lines = Vec::new();
                let role_label = if *role == MessageRoleTag::Assistant {
                    assistant_label
                } else {
                    role.label()
                };
                let role_prefix_text = format!("[{role_label}] ");
                let continuation_indent = " ".repeat(role_prefix_text.chars().count());
                let role_prefix = Span::styled(
                    role_prefix_text,
                    role.badge_style().add_modifier(Modifier::BOLD),
                );

                let full_lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
                let preview_lines = text_preview_lines(text, 3, 220);
                let source = if *expanded || !text_is_collapsible(text) {
                    &full_lines
                } else {
                    &preview_lines
                };
                let mut iter = source.iter();
                let first = iter.next().cloned().unwrap_or_default();
                lines.push(Line::from(vec![role_prefix, Span::raw(first)]));
                for line in iter {
                    lines.push(Line::from(format!("{continuation_indent}{line}")));
                }

                if text_is_collapsible(text) {
                    let hint = if *expanded {
                        "[expanded] Space to collapse"
                    } else {
                        "[collapsed] Space to expand"
                    };
                    lines.push(Line::from(Span::styled(
                        format!("{continuation_indent}{hint}"),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::raw(""));
                lines
            }
            TimelineItem::ToolInvocation {
                call_id,
                tool_id,
                status,
                arguments,
                output,
                meta,
                expanded,
            } => {
                let mut lines = Vec::new();
                let (badge, badge_color) = match tool_id.as_str() {
                    "list_agents" => ("[AGENTS] ", Color::Cyan),
                    "read_config" => ("[CONFIG] ", Color::Magenta),
                    "modify_skill" => ("[SKILL] ", Color::LightBlue),
                    "propose_config_change"
                    | "propose_skill_add"
                    | "propose_skill_update"
                    | "propose_agent_add"
                    | "propose_agent_update"
                    | "list_proposals"
                    | "get_proposal"
                    | "analyze_failures"
                    | "score_skills"
                    | "adapt_routing"
                    | "record_lesson"
                    | "run_redteam_eval"
                    | "list_audit_records" => ("[ADAPT] ", Color::LightMagenta),
                    "authorize_proposal" | "apply_authorized_proposal" => {
                        ("[AUTH-ADAPT] ", Color::Magenta)
                    }
                    _ => ("[TOOL] ", Color::Yellow),
                };
                lines.push(Line::from(vec![
                    Span::styled(
                        badge,
                        Style::default()
                            .fg(badge_color)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(tool_id.clone(), Style::default().fg(Color::Gray)),
                    Span::raw(" status="),
                    Span::styled(status.clone(), status_style(status)),
                    Span::styled(
                        if *expanded {
                            " [expanded]"
                        } else {
                            " [collapsed]"
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));

                match tool_id.as_str() {
                    "list_agents" => {
                        let agents = output
                            .get("agents")
                            .and_then(|value| value.as_array())
                            .cloned()
                            .unwrap_or_default();
                        let visible = if *expanded {
                            agents.len()
                        } else {
                            agents.len().min(4)
                        };
                        lines.push(Line::from(vec![
                            Span::styled("  agents: ", Style::default().fg(Color::DarkGray)),
                            Span::raw(format!("{} found", agents.len())),
                        ]));
                        for agent in agents.iter().take(visible) {
                            let agent_id = json_string_field(agent, "agent_id")
                                .or_else(|| json_string_field(agent, "id"))
                                .unwrap_or_else(|| "unknown".to_string());
                            let state = json_string_field(agent, "state")
                                .or_else(|| json_string_field(agent, "status"))
                                .unwrap_or_else(|| "unknown".to_string());
                            let thread = json_string_field(agent, "thread_id")
                                .or_else(|| json_string_field(agent, "latest_thread_id"))
                                .map(|id| short_id(&id))
                                .unwrap_or_else(|| "-".to_string());
                            let run = json_string_field(agent, "run_id")
                                .or_else(|| json_string_field(agent, "latest_run_id"))
                                .map(|id| short_id(&id))
                                .unwrap_or_else(|| "-".to_string());
                            lines.push(Line::from(vec![
                                Span::styled("  - ", Style::default().fg(Color::DarkGray)),
                                Span::styled(agent_id, Style::default().fg(Color::Cyan)),
                                Span::raw(" ["),
                                Span::styled(state, status_style(status)),
                                Span::raw("] "),
                                Span::styled("thread=", Style::default().fg(Color::DarkGray)),
                                Span::raw(thread),
                                Span::styled(" run=", Style::default().fg(Color::DarkGray)),
                                Span::raw(run),
                            ]));
                        }
                        if !*expanded && agents.len() > visible {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "  ... {} more (expand for full list)",
                                    agents.len() - visible
                                ),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                    "read_config" => {
                        let mut sections = output
                            .as_object()
                            .map(|map| map.keys().cloned().collect::<Vec<_>>())
                            .unwrap_or_default();
                        sections.sort();
                        let section_count = sections.len();
                        let preview_len = if *expanded {
                            section_count
                        } else {
                            section_count.min(6)
                        };
                        let preview = sections
                            .iter()
                            .take(preview_len)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ");
                        lines.push(Line::from(vec![
                            Span::styled("  snapshot: ", Style::default().fg(Color::DarkGray)),
                            Span::raw(format!("{section_count} top-level sections")),
                        ]));
                        if !preview.is_empty() {
                            lines.push(Line::from(vec![
                                Span::styled("  sections: ", Style::default().fg(Color::DarkGray)),
                                Span::raw(preview),
                            ]));
                        }
                        lines.push(Line::from(vec![
                            Span::styled("  redaction: ", Style::default().fg(Color::DarkGray)),
                            Span::styled(
                                if contains_redaction_marker(output) {
                                    "yes"
                                } else {
                                    "no"
                                },
                                Style::default().fg(if contains_redaction_marker(output) {
                                    Color::Yellow
                                } else {
                                    Color::Green
                                }),
                            ),
                        ]));
                        if !*expanded && section_count > preview_len {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "  ... {} more sections (expand for full snapshot)",
                                    section_count - preview_len
                                ),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                    "modify_skill" => {
                        let status_preview = output
                            .get("status")
                            .and_then(|value| value.as_str())
                            .or_else(|| output.get("result").and_then(|value| value.as_str()))
                            .unwrap_or("unknown");
                        let reason_preview = output
                            .get("reason")
                            .and_then(|value| value.as_str())
                            .or_else(|| output.get("error").and_then(|value| value.as_str()))
                            .unwrap_or("n/a");
                        lines.push(Line::from(vec![
                            Span::styled("  action: ", Style::default().fg(Color::DarkGray)),
                            Span::raw(status_preview.to_string()),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("  reason: ", Style::default().fg(Color::DarkGray)),
                            Span::raw(text_preview_lines(reason_preview, 1, 180).join(" ")),
                        ]));
                    }
                    "propose_config_change"
                    | "propose_skill_add"
                    | "propose_skill_update"
                    | "propose_agent_add"
                    | "propose_agent_update"
                    | "authorize_proposal"
                    | "apply_authorized_proposal" => {
                        let state_preview = output
                            .get("state")
                            .and_then(|value| value.as_str())
                            .or_else(|| {
                                output
                                    .get("proposal")
                                    .and_then(|proposal| proposal.get("state"))
                                    .and_then(|value| value.as_str())
                            })
                            .unwrap_or("n/a");
                        let proposal_id = output
                            .get("proposal_id")
                            .and_then(|value| value.as_str())
                            .or_else(|| {
                                output
                                    .get("proposal")
                                    .and_then(|proposal| proposal.get("proposal_id"))
                                    .and_then(|value| value.as_str())
                            })
                            .unwrap_or("n/a");
                        lines.push(Line::from(vec![
                            Span::styled("  proposal: ", Style::default().fg(Color::DarkGray)),
                            Span::raw(proposal_id.to_string()),
                        ]));
                        lines.push(Line::from(vec![
                            Span::styled("  state: ", Style::default().fg(Color::DarkGray)),
                            Span::raw(state_preview.to_string()),
                        ]));
                    }
                    _ => {}
                }

                if *expanded {
                    lines.push(Line::from(vec![
                        Span::styled("  call id: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(call_id.clone(), Style::default().fg(Color::DarkGray)),
                    ]));
                    let seq = meta
                        .as_ref()
                        .and_then(|value| value.seq)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "n/a".to_string());
                    let ts = meta
                        .as_ref()
                        .and_then(|value| value.ts.clone())
                        .unwrap_or_else(|| "n/a".to_string());
                    let redaction = meta
                        .as_ref()
                        .and_then(|value| value.redaction_applied)
                        .map(|flag| if flag { "yes" } else { "no" })
                        .unwrap_or("n/a");
                    let linked_thread = json_string_field(output, "thread_id")
                        .map(|id| short_id(&id))
                        .unwrap_or_else(|| "-".to_string());
                    let linked_run = json_string_field(output, "run_id")
                        .map(|id| short_id(&id))
                        .unwrap_or_else(|| "-".to_string());
                    lines.push(Line::from(vec![
                        Span::styled("  meta: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(format!(
                            "status={status} thread={linked_thread} run={linked_run} seq={seq} ts={ts} redacted={redaction}"
                        )),
                    ]));
                    lines.push(Line::from(Span::styled(
                        "  arguments:",
                        Style::default().fg(Color::Gray),
                    )));
                    for line in pretty_json_lines(arguments) {
                        lines.push(Line::from(format!("    {line}")));
                    }
                    lines.push(Line::from(Span::styled(
                        "  output:",
                        Style::default().fg(Color::Gray),
                    )));
                    for line in pretty_json_lines(output) {
                        lines.push(Line::from(format!("    {line}")));
                    }
                }
                lines.push(Line::raw(""));
                lines
            }
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
                expanded,
            } => {
                let mut lines = Vec::new();
                let target = target_agent
                    .clone()
                    .unwrap_or_else(|| "unknown-agent".to_string());
                lines.push(Line::from(vec![
                    Span::styled(
                        "[DELEGATE] ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("-> {target}"), Style::default().fg(Color::Cyan)),
                    Span::raw(" state="),
                    Span::styled(status.clone(), status_style(status)),
                    Span::styled(
                        if *expanded {
                            " [expanded]"
                        } else {
                            " [collapsed]"
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("  thread: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        thread_id
                            .clone()
                            .unwrap_or_else(|| "unavailable".to_string()),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled("  run: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        run_id.clone().unwrap_or_else(|| "unavailable".to_string()),
                        Style::default().fg(Color::White),
                    ),
                ]));

                if let Some(instruction) = instruction {
                    let preview = text_preview_lines(instruction, 2, 160).join(" ");
                    lines.push(Line::from(vec![
                        Span::styled("  instruction: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(preview),
                    ]));
                }

                if let Some(error) = error {
                    lines.push(Line::from(vec![
                        Span::styled("  error: ", Style::default().fg(Color::Red)),
                        Span::styled(error.clone(), Style::default().fg(Color::Red)),
                    ]));
                } else if let Some(summary) = summary {
                    let preview = text_preview_lines(summary, 2, 180).join(" ");
                    lines.push(Line::from(vec![
                        Span::styled("  summary: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(preview),
                    ]));
                }

                lines.push(Line::from(Span::styled(
                    "  Enter: open sub-agent thread",
                    Style::default().fg(Color::DarkGray),
                )));

                if *expanded {
                    lines.push(Line::from(vec![
                        Span::styled("  call id: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(call_id.clone(), Style::default().fg(Color::DarkGray)),
                    ]));
                    let seq = meta
                        .as_ref()
                        .and_then(|value| value.seq)
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "n/a".to_string());
                    let ts = meta
                        .as_ref()
                        .and_then(|value| value.ts.clone())
                        .unwrap_or_else(|| "n/a".to_string());
                    let redaction = meta
                        .as_ref()
                        .and_then(|value| value.redaction_applied)
                        .map(|flag| if flag { "yes" } else { "no" })
                        .unwrap_or("n/a");
                    let linked_thread = thread_id
                        .as_ref()
                        .map(|id| short_id(id))
                        .unwrap_or_else(|| "-".to_string());
                    let linked_run = run_id
                        .as_ref()
                        .map(|id| short_id(id))
                        .unwrap_or_else(|| "-".to_string());
                    lines.push(Line::from(vec![
                        Span::styled("  meta: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(format!(
                            "status={status} thread={linked_thread} run={linked_run} seq={seq} ts={ts} redacted={redaction}"
                        )),
                    ]));
                    lines.push(Line::from(Span::styled(
                        "  arguments:",
                        Style::default().fg(Color::Gray),
                    )));
                    for line in pretty_json_lines(arguments) {
                        lines.push(Line::from(format!("    {line}")));
                    }
                    lines.push(Line::from(Span::styled(
                        "  output:",
                        Style::default().fg(Color::Gray),
                    )));
                    for line in pretty_json_lines(output) {
                        lines.push(Line::from(format!("    {line}")));
                    }
                }
                lines.push(Line::raw(""));
                lines
            }
        };

        if selected {
            let selected_style = Style::default().bg(Color::DarkGray).fg(Color::White);
            for line in &mut lines {
                *line = line.clone().patch_style(selected_style);
                if let Some(fill_width) = selected_fill_width {
                    let width = line.width();
                    if fill_width > width {
                        line.spans
                            .push(Span::styled(" ".repeat(fill_width - width), selected_style));
                    }
                }
            }
        }

        lines
    }

    pub(super) fn to_snapshot_value(&self) -> serde_json::Value {
        match self {
            TimelineItem::Text { role, text, .. } => serde_json::json!({
                "role": role.label().to_lowercase(),
                "type": "text",
                "content": text,
            }),
            TimelineItem::ToolInvocation {
                call_id,
                tool_id,
                status,
                arguments,
                output,
                meta,
                ..
            } => serde_json::json!({
                "role": "tool",
                "type": "tool_invocation",
                "call_id": call_id,
                "tool_id": tool_id,
                "status": status,
                "arguments": arguments,
                "output": output,
                "meta": meta.as_ref().map(|meta| serde_json::json!({
                    "seq": meta.seq,
                    "ts": meta.ts,
                    "redaction_applied": meta.redaction_applied,
                })),
            }),
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
                ..
            } => serde_json::json!({
                "role": "tool",
                "type": "delegate_to_agent",
                "call_id": call_id,
                "status": status,
                "target_agent": target_agent,
                "thread_id": thread_id,
                "run_id": run_id,
                "instruction": instruction,
                "summary": summary,
                "error": error,
                "arguments": arguments,
                "output": output,
                "meta": meta.as_ref().map(|meta| serde_json::json!({
                    "seq": meta.seq,
                    "ts": meta.ts,
                    "redaction_applied": meta.redaction_applied,
                })),
            }),
        }
    }

    pub(super) fn toggle(&mut self) {
        match self {
            TimelineItem::Text { text, expanded, .. } => {
                if text_is_collapsible(text) {
                    *expanded = !*expanded;
                }
            }
            TimelineItem::ToolInvocation { expanded, .. }
            | TimelineItem::DelegateInvocation { expanded, .. } => *expanded = !*expanded,
        }
    }

    pub(super) fn is_toggleable(&self) -> bool {
        match self {
            TimelineItem::Text { text, .. } => text_is_collapsible(text),
            TimelineItem::ToolInvocation { .. } | TimelineItem::DelegateInvocation { .. } => true,
        }
    }

    pub(super) fn linked_thread_id(&self) -> Option<String> {
        match self {
            TimelineItem::DelegateInvocation {
                thread_id: Some(thread_id),
                ..
            } => Some(thread_id.clone()),
            TimelineItem::ToolInvocation {
                output, tool_id, ..
            } => {
                if tool_id != "delegate_to_agent" {
                    return None;
                }
                output
                    .get("thread_id")
                    .and_then(|value| value.as_str())
                    .map(|thread_id| thread_id.to_string())
            }
            TimelineItem::Text { .. } => None,
            TimelineItem::DelegateInvocation {
                thread_id: None, ..
            } => None,
        }
    }
}

fn pretty_json_lines(value: &serde_json::Value) -> Vec<String> {
    let rendered = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    rendered.lines().map(|line| line.to_string()).collect()
}

fn text_is_collapsible(text: &str) -> bool {
    text.lines().count() > TEXT_COLLAPSE_LINE_THRESHOLD
        || text.chars().count() > TEXT_COLLAPSE_CHAR_THRESHOLD
}

fn text_preview_lines(text: &str, max_lines: usize, max_chars: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut consumed = 0usize;
    for line in text.lines() {
        if out.len() >= max_lines {
            break;
        }

        let remaining = max_chars.saturating_sub(consumed);
        if remaining == 0 {
            break;
        }

        let mut clipped = line.chars().take(remaining).collect::<String>();
        let clipped_len = clipped.chars().count();
        consumed += clipped_len;
        if clipped_len < line.chars().count() {
            clipped.push_str("...");
            out.push(clipped);
            break;
        }

        out.push(clipped);
    }

    if out.is_empty() {
        out.push(String::new());
    }
    out
}

pub(super) fn status_style(status: &str) -> Style {
    match status {
        "success" | "completed" => Style::default().fg(Color::Green),
        "error" | "failed" => Style::default().fg(Color::Red),
        "running" | "pending" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Gray),
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

pub(super) fn short_thread_id(thread_id: &str) -> String {
    short_id(thread_id)
}

fn json_string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .map(|field| field.to_string())
}

fn contains_redaction_marker(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => {
            let normalized = text.to_ascii_lowercase();
            normalized.contains("redacted") || normalized.contains("sensitive_payload")
        }
        serde_json::Value::Array(items) => items.iter().any(contains_redaction_marker),
        serde_json::Value::Object(map) => map.iter().any(|(key, value)| {
            let key_lower = key.to_ascii_lowercase();
            key_lower.contains("redacted")
                || key_lower.contains("sensitive")
                || key_lower.contains("secret")
                || contains_redaction_marker(value)
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::TimelineItem;

    fn line_text(line: &ratatui::text::Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn delegate_summary_is_previewed_when_collapsed() {
        let item = TimelineItem::DelegateInvocation {
            call_id: "call-1".to_string(),
            status: "success".to_string(),
            target_agent: Some("planner".to_string()),
            thread_id: Some("thread-abc".to_string()),
            run_id: Some("run-1".to_string()),
            instruction: Some("Summarize the request".to_string()),
            summary: Some("x".repeat(500)),
            error: None,
            arguments: serde_json::json!({}),
            output: serde_json::json!({}),
            meta: None,
            expanded: false,
        };

        let lines = item.lines(false, "System0", None);
        let summary_line = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("  summary: "))
            .expect("summary line should exist");
        assert!(
            summary_line.len() < 250,
            "summary line should be previewed, got {} chars",
            summary_line.len()
        );
    }
}
