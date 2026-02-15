use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::composer::INPUT_PREFIX;

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
        timeline_focused: bool,
    ) -> Vec<Line<'static>> {
        let mut lines = match self {
            TimelineItem::Text {
                role,
                text,
                expanded,
            } => {
                let mut lines = Vec::new();
                let is_system_line = *role == MessageRoleTag::System;
                let content_style = if is_system_line {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default().fg(Color::White)
                };
                let role_label = if *role == MessageRoleTag::Assistant {
                    assistant_label.to_ascii_lowercase()
                } else {
                    "system".to_string()
                };
                let continuation_indent = "  ";

                let full_lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
                let preview_lines = text_preview_lines(text, 3, 220);
                let source = if *expanded || !text_is_collapsible(text) {
                    &full_lines
                } else {
                    &preview_lines
                };
                let mut iter = source.iter();
                let first = iter.next().cloned().unwrap_or_default();
                let first_line = match role {
                    MessageRoleTag::User => Line::from(vec![
                        Span::styled(
                            INPUT_PREFIX,
                            Style::default()
                                .fg(Color::Rgb(140, 235, 255))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(first, content_style),
                    ]),
                    MessageRoleTag::Assistant => Line::from(vec![
                        badge_chip(
                            &role_label,
                            Style::default()
                                .fg(Color::Rgb(205, 228, 255))
                                .bg(Color::Rgb(52, 76, 115))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                        Span::styled(first, content_style),
                    ]),
                    MessageRoleTag::System => Line::from(vec![
                        badge_chip(
                            "system",
                            Style::default()
                                .fg(Color::DarkGray)
                                .bg(Color::Rgb(44, 44, 52)),
                        ),
                        Span::raw(" "),
                        Span::styled(first, content_style),
                    ]),
                };
                lines.push(first_line);
                for line in iter {
                    lines.push(Line::from(vec![
                        Span::raw(continuation_indent),
                        Span::styled(line.clone(), content_style),
                    ]));
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
                if matches!(*role, MessageRoleTag::User | MessageRoleTag::Assistant) {
                    lines.insert(0, Line::raw(""));
                    lines.push(Line::raw(""));
                }
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
                let badge = match tool_id.as_str() {
                    "list_agents" => badge_chip(
                        "agents",
                        Style::default()
                            .fg(Color::Rgb(196, 236, 255))
                            .bg(Color::Rgb(36, 88, 108))
                            .add_modifier(Modifier::BOLD),
                    ),
                    "read_config" => badge_chip(
                        "config",
                        Style::default()
                            .fg(Color::Rgb(252, 212, 255))
                            .bg(Color::Rgb(92, 48, 105))
                            .add_modifier(Modifier::BOLD),
                    ),
                    "modify_skill" => badge_chip(
                        "skill",
                        Style::default()
                            .fg(Color::Rgb(210, 229, 255))
                            .bg(Color::Rgb(48, 76, 122))
                            .add_modifier(Modifier::BOLD),
                    ),
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
                    | "list_audit_records" => badge_chip(
                        "adapt",
                        Style::default()
                            .fg(Color::Rgb(252, 220, 255))
                            .bg(Color::Rgb(95, 43, 110))
                            .add_modifier(Modifier::BOLD),
                    ),
                    "authorize_proposal" | "apply_authorized_proposal" => badge_chip(
                        "auth",
                        Style::default()
                            .fg(Color::Rgb(245, 220, 255))
                            .bg(Color::Rgb(109, 55, 118))
                            .add_modifier(Modifier::BOLD),
                    ),
                    _ => badge_chip(
                        "tool",
                        Style::default()
                            .fg(Color::Rgb(255, 236, 197))
                            .bg(Color::Rgb(95, 77, 28))
                            .add_modifier(Modifier::BOLD),
                    ),
                };
                lines.push(Line::from(vec![
                    badge,
                    Span::raw(" "),
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
                let args_preview = instruction
                    .as_ref()
                    .map(|value| text_preview_lines(value, 1, 72).join(" "))
                    .unwrap_or_else(|| preview_json_value(arguments, 72));
                let output_preview = summary
                    .as_ref()
                    .map(|value| text_preview_lines(value, 1, 72).join(" "))
                    .unwrap_or_else(|| preview_json_value(output, 72));
                let error_preview = error.clone().unwrap_or_else(|| "none".to_string());

                if !*expanded {
                    lines.push(kv_row("Args", args_preview));
                    lines.push(kv_row("Output", output_preview));
                    lines.push(kv_row("Error", error_preview));
                } else {
                    lines.push(section_header("State"));
                    lines.push(kv_row("status", pretty_status(status)));
                    lines.push(kv_row("agent", target.clone()));

                    lines.push(Line::raw(""));
                    lines.push(section_header("Input"));
                    let mut input_fields = key_values_from_json(arguments, 5, &[]);
                    if let Some(instruction) = instruction
                        && !input_fields.iter().any(|(key, _)| key == "task")
                    {
                        input_fields.insert(
                            0,
                            (
                                "task".to_string(),
                                text_preview_lines(instruction, 2, 90).join(" "),
                            ),
                        );
                    }
                    if input_fields.is_empty() {
                        input_fields.push(("task".to_string(), args_preview.clone()));
                    }
                    for (key, value) in input_fields {
                        lines.push(kv_row(&key, value));
                    }

                    lines.push(Line::raw(""));
                    lines.push(section_header("Output"));
                    lines.push(kv_row("summary", output_preview.clone()));
                    let output_fields = key_values_from_json(
                        output,
                        3,
                        &[
                            "summary",
                            "error",
                            "thread_id",
                            "run_id",
                            "timestamp",
                            "tools",
                        ],
                    );
                    for (key, value) in output_fields {
                        lines.push(kv_row(&key, value));
                    }
                    lines.push(kv_row("error", error_preview.clone()));

                    let tool_fields = collect_tool_usage(output);
                    if !tool_fields.is_empty() {
                        lines.push(Line::raw(""));
                        lines.push(section_header("Tools"));
                        for (key, value) in tool_fields {
                            lines.push(kv_row(&key, value));
                        }
                    }

                    lines.push(Line::raw(""));
                    lines.push(section_header("Details"));
                    lines.push(kv_row("thread_id", compact_id(thread_id.as_deref())));
                    lines.push(kv_row("run_id", compact_id(run_id.as_deref())));
                    lines.push(kv_row(
                        "timestamp",
                        meta.as_ref()
                            .and_then(|value| value.ts.clone())
                            .or_else(|| {
                                output
                                    .get("timestamp")
                                    .and_then(|value| value.as_str())
                                    .map(|value| value.to_string())
                            })
                            .unwrap_or_else(|| "n/a".to_string()),
                    ));
                    lines.push(kv_row(
                        "duration",
                        output
                            .get("duration")
                            .and_then(|value| value.as_str())
                            .map(|value| value.to_string())
                            .or_else(|| {
                                output
                                    .get("duration")
                                    .and_then(|value| value.as_u64())
                                    .map(|value| format!("{value}s"))
                            })
                            .unwrap_or_else(|| "n/a".to_string()),
                    ));
                    lines.push(kv_row("call_id", compact_id(Some(call_id.as_str()))));
                }
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    badge_chip(
                        &target,
                        Style::default()
                            .fg(Color::Rgb(255, 237, 183))
                            .bg(Color::Rgb(89, 76, 33))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                    Span::styled(pretty_status(status), status_style(status)),
                ]));
                lines
            }
        };

        let card_style = if selected {
            self.selected_card_style()
        } else {
            self.base_card_style()
        };
        let card_gutter_style = self.card_gutter_style(selected, timeline_focused);
        for line in &mut lines {
            let mut prefixed = Vec::with_capacity(line.spans.len() + 2);
            prefixed.push(Span::styled(
                if selected { "┃" } else { " " },
                card_gutter_style,
            ));
            prefixed.push(Span::styled(" ", card_style));
            prefixed.extend(line.spans.clone());
            *line = Line::from(prefixed).patch_style(card_style);
            if let Some(fill_width) = selected_fill_width {
                let width = line.width();
                if fill_width > width {
                    line.spans
                        .push(Span::styled(" ".repeat(fill_width - width), card_style));
                }
            }
        }

        lines
    }

    fn base_card_style(&self) -> Style {
        let background = match self {
            TimelineItem::Text {
                role: MessageRoleTag::User,
                ..
            } => Color::Rgb(20, 36, 62),
            _ => Color::Rgb(24, 24, 31),
        };
        Style::default().bg(background)
    }

    fn selected_card_style(&self) -> Style {
        let background = match self {
            TimelineItem::Text {
                role: MessageRoleTag::User,
                ..
            } => Color::Rgb(33, 63, 108),
            _ => Color::Rgb(58, 58, 74),
        };
        Style::default().bg(background).add_modifier(Modifier::BOLD)
    }

    fn card_gutter_style(&self, selected: bool, timeline_focused: bool) -> Style {
        let background = if selected {
            self.selected_card_bg()
        } else {
            self.base_card_bg()
        };
        Style::default()
            .bg(background)
            .fg(if selected && timeline_focused {
                Color::Rgb(104, 181, 255)
            } else {
                Color::Rgb(70, 70, 84)
            })
    }

    fn base_card_bg(&self) -> Color {
        match self {
            TimelineItem::Text {
                role: MessageRoleTag::User,
                ..
            } => Color::Rgb(20, 36, 62),
            _ => Color::Rgb(24, 24, 31),
        }
    }

    fn selected_card_bg(&self) -> Color {
        match self {
            TimelineItem::Text {
                role: MessageRoleTag::User,
                ..
            } => Color::Rgb(33, 63, 108),
            _ => Color::Rgb(58, 58, 74),
        }
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

fn badge_chip(label: &str, style: Style) -> Span<'static> {
    Span::styled(format!(" {} ", label), style)
}

fn section_header(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("[{title}]"),
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    ))
}

fn kv_row(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<10}"), Style::default().fg(Color::DarkGray)),
        Span::styled(": ", Style::default().fg(Color::DarkGray)),
        Span::styled(value, Style::default().fg(Color::White)),
    ])
}

fn pretty_status(status: &str) -> String {
    match status {
        "success" | "completed" => "COMPLETED".to_string(),
        "error" | "failed" => "FAILED".to_string(),
        "running" | "pending" => "RUNNING".to_string(),
        _ => status.to_ascii_uppercase(),
    }
}

fn preview_json_value(value: &serde_json::Value, max_chars: usize) -> String {
    let rendered = match value {
        serde_json::Value::Null => "none".to_string(),
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Bool(flag) => flag.to_string(),
        serde_json::Value::Number(number) => number.to_string(),
        serde_json::Value::Array(items) => {
            if items.is_empty() {
                "none".to_string()
            } else {
                let values = items
                    .iter()
                    .take(3)
                    .map(|item| preview_json_value(item, 24))
                    .collect::<Vec<_>>()
                    .join(", ");
                if items.len() > 3 {
                    format!("{values}, ...")
                } else {
                    values
                }
            }
        }
        serde_json::Value::Object(map) => {
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            let values = keys
                .iter()
                .take(3)
                .map(|key| {
                    format!(
                        "{key}={}",
                        preview_json_value(map.get(key).unwrap_or(&serde_json::Value::Null), 24)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            if keys.len() > 3 {
                format!("{values}, ...")
            } else {
                values
            }
        }
    };
    if rendered.chars().count() > max_chars {
        let mut clipped = rendered
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect::<String>();
        clipped.push_str("...");
        clipped
    } else {
        rendered
    }
}

fn key_values_from_json(
    value: &serde_json::Value,
    max_fields: usize,
    skip_keys: &[&str],
) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let Some(map) = value.as_object() else {
        if !value.is_null() {
            rows.push(("value".to_string(), preview_json_value(value, 90)));
        }
        return rows;
    };

    let mut keys = map.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    for key in keys {
        if rows.len() >= max_fields {
            break;
        }
        if skip_keys.iter().any(|skip| *skip == key) {
            continue;
        }
        let Some(field) = map.get(&key) else {
            continue;
        };
        rows.push((key, preview_json_value(field, 90)));
    }
    rows
}

fn collect_tool_usage(output: &serde_json::Value) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    if let Some(tools) = output.get("tools").and_then(|value| value.as_object()) {
        let mut keys = tools.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        for key in keys {
            if let Some(value) = tools.get(&key) {
                rows.push((key, preview_json_value(value, 60)));
            }
        }
    }
    rows
}

fn compact_id(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "n/a".to_string();
    };
    if value.chars().count() <= 12 {
        return value.to_string();
    }
    let prefix = value.chars().take(8).collect::<String>();
    format!("{prefix}...")
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

        let lines = item.lines(false, "System0", None, true);
        let output_line = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("Output"))
            .expect("collapsed output line should exist");
        assert!(
            output_line.len() < 250,
            "output line should be previewed, got {} chars",
            output_line.len()
        );
    }
}
