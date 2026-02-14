use std::collections::HashMap;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::Command;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use neuromancer_core::rpc::{
    OrchestratorSubagentTurnParams, OrchestratorSubagentTurnResult, OrchestratorThreadGetParams,
    OrchestratorTurnParams, OrchestratorTurnResult, ThreadEvent, ThreadSummary,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::CliError;
use crate::rpc_client::RpcClient;

const SYSTEM_THREAD_ID: &str = "system0";
const DEFAULT_MAX_VISIBLE_LINES: usize = 10;
const DEFAULT_THREAD_PAGE_LIMIT: usize = 200;
const INPUT_PREFIX: &str = "> ";
const TEXT_COLLAPSE_LINE_THRESHOLD: usize = 4;
const TEXT_COLLAPSE_CHAR_THRESHOLD: usize = 280;

pub async fn run_orchestrator_chat(rpc: &RpcClient, json_mode: bool) -> Result<(), CliError> {
    if json_mode {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        run_ndjson_chat(rpc, stdin.lock(), &mut stdout).await
    } else {
        run_interactive_chat(rpc).await
    }
}

#[derive(Debug)]
enum PendingOutcome {
    SystemTurn(Result<OrchestratorTurnResult, CliError>),
    SubagentTurn {
        thread_id: String,
        result: Result<OrchestratorSubagentTurnResult, CliError>,
    },
    OpenThread {
        thread_id: String,
        result: Result<(), CliError>,
    },
    WorkerFailed(String),
}

async fn run_interactive_chat(rpc: &RpcClient) -> Result<(), CliError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CliError::Usage(
            "orchestrator chat interactive mode requires a TTY; use --json for NDJSON mode"
                .to_string(),
        ));
    }

    enable_raw_mode().map_err(map_terminal_err)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste).map_err(map_terminal_err)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(map_terminal_err)?;
    let mut cleanup = TerminalCleanup { enabled: true };

    let mut app = ChatApp::new();
    app.bootstrap(rpc).await?;
    let mut clipboard = CommandClipboard;
    let mut pending: Option<tokio::task::JoinHandle<PendingOutcome>> = None;

    loop {
        if pending.as_ref().is_some_and(|handle| handle.is_finished()) {
            let finished = pending.take().expect("pending task must exist");
            let outcome = match finished.await {
                Ok(outcome) => outcome,
                Err(err) => PendingOutcome::WorkerFailed(err.to_string()),
            };
            app.handle_pending_outcome(rpc, outcome).await;
        }

        terminal
            .draw(|frame| app.render(frame))
            .map_err(map_terminal_err)?;

        if !event::poll(Duration::from_millis(100)).map_err(map_terminal_err)? {
            continue;
        }

        let next = event::read().map_err(map_terminal_err)?;
        match app.handle_event(next, &mut clipboard) {
            AppAction::None => {}
            AppAction::Quit => break,
            AppAction::SubmitSystemTurn(message) => {
                if pending.is_some() {
                    app.status = Some("Request already in progress".to_string());
                    continue;
                }
                let rpc = rpc.clone();
                pending = Some(tokio::spawn(async move {
                    let result = rpc
                        .orchestrator_turn(OrchestratorTurnParams { message })
                        .await
                        .map_err(CliError::from);
                    PendingOutcome::SystemTurn(result)
                }));
            }
            AppAction::SubmitSubagentTurn { thread_id, message } => {
                if pending.is_some() {
                    app.status = Some("Request already in progress".to_string());
                    continue;
                }
                let rpc = rpc.clone();
                pending = Some(tokio::spawn(async move {
                    let result = rpc
                        .orchestrator_subagent_turn(OrchestratorSubagentTurnParams {
                            thread_id: thread_id.clone(),
                            message,
                        })
                        .await
                        .map_err(CliError::from);
                    PendingOutcome::SubagentTurn { thread_id, result }
                }));
            }
            AppAction::OpenThread {
                thread_id,
                resurrect,
            } => {
                if pending.is_some() {
                    app.status = Some("Request already in progress".to_string());
                    continue;
                }
                let rpc = rpc.clone();
                pending = Some(tokio::spawn(async move {
                    let result = async {
                        if resurrect {
                            rpc.orchestrator_thread_resurrect(
                                neuromancer_core::rpc::OrchestratorThreadResurrectParams {
                                    thread_id: thread_id.clone(),
                                },
                            )
                            .await
                            .map_err(CliError::from)?;
                        }
                        Ok(())
                    }
                    .await;
                    PendingOutcome::OpenThread { thread_id, result }
                }));
            }
        }
    }

    terminal.show_cursor().map_err(map_terminal_err)?;
    drop(terminal);
    cleanup.disable();
    Ok(())
}

async fn run_ndjson_chat<R: BufRead, W: Write>(
    rpc: &RpcClient,
    mut input: R,
    output: &mut W,
) -> Result<(), CliError> {
    let mut app = ChatApp::new();
    app.bootstrap(rpc).await?;

    emit_ndjson(
        output,
        serde_json::json!({
            "event": "snapshot",
            "system_thread": app.system_thread_snapshot(),
            "runs": [],
        }),
    )?;

    let mut line = String::new();
    loop {
        line.clear();
        let read = input.read_line(&mut line).map_err(map_io_err)?;
        if read == 0 {
            break;
        }

        let Some(message) = parse_ndjson_message_line(&line) else {
            continue;
        };

        match rpc
            .orchestrator_turn(OrchestratorTurnParams {
                message: message.clone(),
            })
            .await
        {
            Ok(turn_result) => {
                app.refresh_thread_summaries(rpc).await?;
                app.load_thread(rpc, SYSTEM_THREAD_ID).await?;
                emit_ndjson(
                    output,
                    serde_json::json!({
                        "event": "turn_complete",
                        "input": message,
                        "turn_result": turn_result,
                        "system_thread": app.system_thread_snapshot(),
                        "runs": [],
                    }),
                )?;
            }
            Err(err) => {
                emit_ndjson(
                    output,
                    serde_json::json!({
                        "event": "turn_error",
                        "input": message,
                        "error": {
                            "message": CliError::from(err).to_string(),
                        },
                    }),
                )?;
            }
        }
    }

    Ok(())
}

fn emit_ndjson<W: Write>(output: &mut W, value: serde_json::Value) -> Result<(), CliError> {
    serde_json::to_writer(&mut *output, &value)
        .map_err(|err| CliError::Lifecycle(format!("failed to encode NDJSON output: {err}")))?;
    output.write_all(b"\n").map_err(map_io_err)?;
    output.flush().map_err(map_io_err)?;
    Ok(())
}

fn parse_ndjson_message_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(message) = value.as_str() {
            return Some(message.to_string());
        }
        if let Some(message) = value.get("message").and_then(|inner| inner.as_str()) {
            return Some(message.to_string());
        }
        return None;
    }

    Some(trimmed.to_string())
}

fn map_terminal_err(err: impl std::fmt::Display) -> CliError {
    CliError::Lifecycle(format!("chat terminal error: {err}"))
}

fn map_io_err(err: io::Error) -> CliError {
    CliError::Lifecycle(format!("chat I/O error: {err}"))
}

struct TerminalCleanup {
    enabled: bool,
}

impl TerminalCleanup {
    fn disable(&mut self) {
        if !self.enabled {
            return;
        }

        self.enabled = false;
        let _ = disable_raw_mode();
        let mut out = io::stdout();
        let _ = execute!(out, DisableBracketedPaste, LeaveAlternateScreen);
    }
}

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        self.disable();
    }
}

trait ClipboardProvider {
    fn read_text(&mut self) -> Result<Option<String>, String>;
}

struct CommandClipboard;

impl ClipboardProvider for CommandClipboard {
    fn read_text(&mut self) -> Result<Option<String>, String> {
        #[cfg(target_os = "macos")]
        {
            return run_clipboard_command("pbpaste", &[]);
        }

        #[cfg(target_os = "linux")]
        {
            for (cmd, args) in [
                ("wl-paste", vec!["-n"]),
                ("xclip", vec!["-selection", "clipboard", "-o"]),
                ("xsel", vec!["--clipboard", "--output"]),
            ] {
                if let Some(value) = run_clipboard_command(cmd, &args)? {
                    return Ok(Some(value));
                }
            }
            return Ok(None);
        }

        #[cfg(target_os = "windows")]
        {
            return run_clipboard_command(
                "powershell",
                &["-NoProfile", "-Command", "Get-Clipboard"],
            );
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Ok(None)
        }
    }
}

fn run_clipboard_command(command: &str, args: &[&str]) -> Result<Option<String>, String> {
    let output = match Command::new(command).args(args).output() {
        Ok(output) => output,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.to_string()),
    };

    if !output.status.success() {
        return Ok(None);
    }

    let content = String::from_utf8_lossy(&output.stdout).to_string();
    if content.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(normalize_line_endings(content.trim_end_matches('\n'))))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Sidebar,
    Main,
    Input,
}

impl FocusPane {
    fn next(self) -> Self {
        match self {
            Self::Sidebar => Self::Main,
            Self::Main => Self::Input,
            Self::Input => Self::Sidebar,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::Sidebar => Self::Input,
            Self::Main => Self::Sidebar,
            Self::Input => Self::Main,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppAction {
    None,
    Quit,
    SubmitSystemTurn(String),
    SubmitSubagentTurn { thread_id: String, message: String },
    OpenThread { thread_id: String, resurrect: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageRoleTag {
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ThreadKind {
    System,
    Subagent,
}

#[derive(Debug, Clone)]
enum TimelineItem {
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
        expanded: bool,
    },
}

impl TimelineItem {
    fn text(role: MessageRoleTag, text: impl Into<String>) -> Self {
        let text = text.into();
        let expanded = !text_is_collapsible(&text);
        Self::Text {
            role,
            text,
            expanded,
        }
    }

    fn lines(&self, selected: bool, assistant_label: &str) -> Vec<Line<'static>> {
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
                expanded,
            } => {
                let mut lines = Vec::new();
                lines.push(Line::from(vec![
                    Span::styled(
                        "[TOOL] ",
                        Style::default()
                            .fg(Color::Yellow)
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
                if *expanded {
                    lines.push(Line::from(vec![
                        Span::styled("  call id: ", Style::default().fg(Color::DarkGray)),
                        Span::styled(call_id.clone(), Style::default().fg(Color::DarkGray)),
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
                    lines.push(Line::from(vec![
                        Span::styled("  summary: ", Style::default().fg(Color::DarkGray)),
                        Span::raw(summary.clone()),
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
            for line in &mut lines {
                *line = line
                    .clone()
                    .patch_style(Style::default().bg(Color::DarkGray).fg(Color::White));
            }
        }

        lines
    }

    fn to_snapshot_value(&self) -> serde_json::Value {
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
                ..
            } => serde_json::json!({
                "role": "tool",
                "type": "tool_invocation",
                "call_id": call_id,
                "tool_id": tool_id,
                "status": status,
                "arguments": arguments,
                "output": output,
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
            }),
        }
    }

    fn toggle(&mut self) {
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

    fn is_toggleable(&self) -> bool {
        match self {
            TimelineItem::Text { text, .. } => text_is_collapsible(text),
            TimelineItem::ToolInvocation { .. } | TimelineItem::DelegateInvocation { .. } => true,
        }
    }

    fn linked_thread_id(&self) -> Option<String> {
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

fn status_style(status: &str) -> Style {
    match status {
        "success" | "completed" => Style::default().fg(Color::Green),
        "error" | "failed" => Style::default().fg(Color::Red),
        "running" | "pending" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Gray),
    }
}

#[derive(Debug, Clone)]
struct ThreadView {
    id: String,
    title: String,
    kind: ThreadKind,
    agent_id: Option<String>,
    state: String,
    active: bool,
    resurrected: bool,
    read_only: bool,
    items: Vec<TimelineItem>,
}

impl ThreadView {
    fn system() -> Self {
        Self {
            id: SYSTEM_THREAD_ID.to_string(),
            title: "System0".to_string(),
            kind: ThreadKind::System,
            agent_id: Some(SYSTEM_THREAD_ID.to_string()),
            state: "active".to_string(),
            active: true,
            resurrected: false,
            read_only: false,
            items: vec![],
        }
    }

    fn from_summary(summary: &ThreadSummary) -> Self {
        let kind = if summary.kind == "system" {
            ThreadKind::System
        } else {
            ThreadKind::Subagent
        };
        let title = if kind == ThreadKind::System {
            "System0".to_string()
        } else {
            format!(
                "{} ({})",
                summary
                    .agent_id
                    .clone()
                    .unwrap_or_else(|| "sub-agent".to_string()),
                short_thread_id(&summary.thread_id),
            )
        };
        let read_only = kind == ThreadKind::Subagent && !(summary.active || summary.resurrected);
        Self {
            id: summary.thread_id.clone(),
            title,
            kind,
            agent_id: summary.agent_id.clone(),
            state: summary.state.clone(),
            active: summary.active,
            resurrected: summary.resurrected,
            read_only,
            items: vec![],
        }
    }

    fn assistant_badge_label(&self) -> String {
        match self.kind {
            ThreadKind::System => "System0".to_string(),
            ThreadKind::Subagent => self
                .agent_id
                .clone()
                .unwrap_or_else(|| "Assistant".to_string()),
        }
    }

    fn apply_summary(&mut self, summary: &ThreadSummary) {
        self.agent_id = summary.agent_id.clone();
        self.state = summary.state.clone();
        self.active = summary.active;
        self.resurrected = summary.resurrected;
        self.read_only =
            self.kind == ThreadKind::Subagent && !(summary.active || summary.resurrected);
        if self.kind == ThreadKind::Subagent {
            self.title = format!(
                "{} ({})",
                summary
                    .agent_id
                    .clone()
                    .unwrap_or_else(|| "sub-agent".to_string()),
                short_thread_id(&summary.thread_id),
            );
        }
    }
}

#[derive(Debug, Clone)]
struct ChatApp {
    threads: Vec<ThreadView>,
    sidebar_selected: usize,
    active_thread: usize,
    focus: FocusPane,
    selected_item_by_thread: HashMap<String, usize>,
    vertical_scroll_by_thread: HashMap<String, usize>,
    horizontal_scroll_by_thread: HashMap<String, usize>,
    composer: ComposerState,
    status: Option<String>,
    last_input_cursor_rel: Option<(u16, u16)>,
    last_main_viewport_rows: usize,
    pending_thread_id: Option<String>,
    pending_placeholder_index_by_thread: HashMap<String, usize>,
}

impl ChatApp {
    fn new() -> Self {
        Self {
            threads: vec![ThreadView::system()],
            sidebar_selected: 0,
            active_thread: 0,
            focus: FocusPane::Input,
            selected_item_by_thread: HashMap::new(),
            vertical_scroll_by_thread: HashMap::new(),
            horizontal_scroll_by_thread: HashMap::new(),
            composer: ComposerState::new(),
            status: Some("System0 thread ready".to_string()),
            last_input_cursor_rel: None,
            last_main_viewport_rows: 1,
            pending_thread_id: None,
            pending_placeholder_index_by_thread: HashMap::new(),
        }
    }

    async fn bootstrap(&mut self, rpc: &RpcClient) -> Result<(), CliError> {
        self.refresh_thread_summaries(rpc).await?;
        self.load_thread(rpc, SYSTEM_THREAD_ID).await?;
        if let Some(system_thread) = self
            .threads
            .iter()
            .find(|thread| thread.id == SYSTEM_THREAD_ID)
            && !system_thread.items.is_empty()
        {
            self.status = Some("Loaded existing System0 context".to_string());
        }
        Ok(())
    }

    async fn handle_pending_outcome(&mut self, rpc: &RpcClient, outcome: PendingOutcome) {
        match outcome {
            PendingOutcome::SystemTurn(result) => match result {
                Ok(_) => {
                    if let Err(err) = self.refresh_thread_summaries(rpc).await {
                        self.mark_pending_failed(err.to_string());
                        return;
                    }
                    if let Err(err) = self.load_thread(rpc, SYSTEM_THREAD_ID).await {
                        self.mark_pending_failed(err.to_string());
                        return;
                    }
                    self.clear_pending_marker(SYSTEM_THREAD_ID);
                    self.status = Some("Turn completed".to_string());
                }
                Err(err) => {
                    self.mark_pending_failed(err.to_string());
                }
            },
            PendingOutcome::SubagentTurn { thread_id, result } => match result {
                Ok(_) => {
                    if let Err(err) = self.refresh_thread_summaries(rpc).await {
                        self.mark_pending_failed(err.to_string());
                        return;
                    }
                    if let Err(err) = self.load_thread(rpc, &thread_id).await {
                        self.mark_pending_failed(err.to_string());
                        return;
                    }
                    self.clear_pending_marker(&thread_id);
                    self.status = Some(format!("Thread {} updated", short_thread_id(&thread_id)));
                }
                Err(err) => {
                    self.mark_pending_failed(err.to_string());
                }
            },
            PendingOutcome::OpenThread { thread_id, result } => match result {
                Ok(()) => {
                    if let Err(err) = self.refresh_thread_summaries(rpc).await {
                        self.status = Some(err.to_string());
                        return;
                    }
                    if let Err(err) = self.load_thread(rpc, &thread_id).await {
                        self.status = Some(err.to_string());
                        return;
                    }
                    self.select_thread_by_id(&thread_id);
                    self.status = Some(format!("Loaded thread {}", short_thread_id(&thread_id)));
                }
                Err(err) => {
                    self.status = Some(err.to_string());
                }
            },
            PendingOutcome::WorkerFailed(message) => {
                self.mark_pending_failed(format!("request worker failed: {message}"));
            }
        }
    }

    async fn refresh_thread_summaries(&mut self, rpc: &RpcClient) -> Result<(), CliError> {
        let mut summaries = rpc.orchestrator_threads_list().await?.threads;
        if !summaries
            .iter()
            .any(|thread| thread.thread_id == SYSTEM_THREAD_ID)
        {
            summaries.push(ThreadSummary {
                thread_id: SYSTEM_THREAD_ID.to_string(),
                kind: "system".to_string(),
                agent_id: Some(SYSTEM_THREAD_ID.to_string()),
                latest_run_id: None,
                state: "active".to_string(),
                updated_at: "".to_string(),
                resurrected: false,
                active: true,
            });
        }
        self.sync_thread_summaries(summaries);
        Ok(())
    }

    fn sync_thread_summaries(&mut self, mut summaries: Vec<ThreadSummary>) {
        let current_active_id = self.current_thread().id.clone();
        let current_sidebar_id = self
            .threads
            .get(self.sidebar_selected)
            .map(|thread| thread.id.clone())
            .unwrap_or_else(|| SYSTEM_THREAD_ID.to_string());

        let mut existing = std::mem::take(&mut self.threads)
            .into_iter()
            .map(|thread| (thread.id.clone(), thread))
            .collect::<HashMap<_, _>>();

        summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        let mut system_summary = None;
        let mut subagent_summaries = Vec::new();
        for summary in summaries {
            if summary.thread_id == SYSTEM_THREAD_ID || summary.kind == "system" {
                system_summary = Some(summary);
            } else {
                subagent_summaries.push(summary);
            }
        }

        let mut threads = Vec::new();
        let system_summary = system_summary.unwrap_or(ThreadSummary {
            thread_id: SYSTEM_THREAD_ID.to_string(),
            kind: "system".to_string(),
            agent_id: Some(SYSTEM_THREAD_ID.to_string()),
            latest_run_id: None,
            state: "active".to_string(),
            updated_at: "".to_string(),
            resurrected: false,
            active: true,
        });

        let mut system_thread = existing
            .remove(SYSTEM_THREAD_ID)
            .unwrap_or_else(|| ThreadView::from_summary(&system_summary));
        system_thread.apply_summary(&system_summary);
        threads.push(system_thread);

        for summary in subagent_summaries {
            let mut thread = existing
                .remove(&summary.thread_id)
                .unwrap_or_else(|| ThreadView::from_summary(&summary));
            thread.apply_summary(&summary);
            if thread.items.is_empty() && thread.read_only {
                thread.items.push(TimelineItem::text(
                    MessageRoleTag::System,
                    "Saved sub-agent thread. Press Enter to resurrect and continue.",
                ));
            }
            threads.push(thread);
        }

        self.threads = threads;
        self.active_thread = self
            .threads
            .iter()
            .position(|thread| thread.id == current_active_id)
            .unwrap_or(0);
        self.sidebar_selected = self
            .threads
            .iter()
            .position(|thread| thread.id == current_sidebar_id)
            .unwrap_or(self.active_thread)
            .min(self.threads.len().saturating_sub(1));
    }

    async fn load_thread(&mut self, rpc: &RpcClient, thread_id: &str) -> Result<(), CliError> {
        let events = fetch_all_thread_events(rpc, thread_id).await?;
        let kind = self
            .threads
            .iter()
            .find(|thread| thread.id == thread_id)
            .map(|thread| thread.kind.clone())
            .unwrap_or_else(|| {
                if thread_id == SYSTEM_THREAD_ID {
                    ThreadKind::System
                } else {
                    ThreadKind::Subagent
                }
            });

        let mut items = timeline_items_from_events(&kind, &events);
        if matches!(kind, ThreadKind::Subagent)
            && !items.iter().any(|item| {
                matches!(
                    item,
                    TimelineItem::Text {
                        role: MessageRoleTag::User,
                        ..
                    }
                )
            })
        {
            items.insert(
                0,
                TimelineItem::text(
                    MessageRoleTag::System,
                    "Initial instruction unavailable (legacy run format)",
                ),
            );
        }

        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        {
            thread.items = items;
            let selected = self
                .selected_item_by_thread
                .get(thread_id)
                .copied()
                .unwrap_or_else(|| thread.items.len().saturating_sub(1));
            self.selected_item_by_thread.insert(
                thread_id.to_string(),
                selected.min(thread.items.len().saturating_sub(1)),
            );
        }

        self.clear_pending_marker(thread_id);
        Ok(())
    }

    fn clear_pending_marker(&mut self, thread_id: &str) {
        self.pending_thread_id = None;
        self.pending_placeholder_index_by_thread.remove(thread_id);
    }

    fn mark_pending_failed(&mut self, message: String) {
        if let Some(thread_id) = self.pending_thread_id.take() {
            if let Some(thread) = self
                .threads
                .iter_mut()
                .find(|thread| thread.id == thread_id)
            {
                if let Some(placeholder_idx) =
                    self.pending_placeholder_index_by_thread.remove(&thread_id)
                    && placeholder_idx < thread.items.len()
                {
                    thread.items.remove(placeholder_idx);
                }
                thread.items.push(TimelineItem::text(
                    MessageRoleTag::System,
                    format!("SYSTEM ERROR: {message}"),
                ));
                let selected = thread.items.len().saturating_sub(1);
                self.selected_item_by_thread
                    .insert(thread_id.clone(), selected);
            }
        }
        self.status = Some(message);
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.composer.max_visible_lines = if area.height < 22 {
            4
        } else if area.height < 34 {
            6
        } else {
            DEFAULT_MAX_VISIBLE_LINES
        };
        self.composer.ensure_cursor_visible();

        let max_input_height = area.height.saturating_sub(6).max(3);
        let input_box_height = (self.composer.input_height() as u16)
            .saturating_add(2)
            .clamp(3, max_input_height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(input_box_height),
                Constraint::Length(1),
            ])
            .split(area);

        let body = if chunks[0].width < 84 {
            let sidebar_height = (chunks[0].height / 3).clamp(5, 10);
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(sidebar_height), Constraint::Min(6)])
                .split(chunks[0])
        } else {
            let max_sidebar = chunks[0].width.saturating_sub(20).max(18);
            let sidebar_width = (((chunks[0].width as f32) * 0.30) as u16)
                .clamp(18, 38)
                .min(max_sidebar);
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(sidebar_width), Constraint::Min(20)])
                .split(chunks[0])
        };

        self.render_sidebar(frame, body[0]);
        self.render_thread(frame, body[1]);
        self.render_input(frame, chunks[1]);
        self.render_status(frame, chunks[2]);
    }

    fn render_sidebar(&self, frame: &mut Frame<'_>, area: Rect) {
        let block = if self.focus == FocusPane::Sidebar {
            Block::default()
                .title("Threads")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
        } else {
            Block::default().title("Threads").borders(Borders::ALL)
        };
        let items: Vec<ListItem<'static>> = self
            .threads
            .iter()
            .map(|thread| {
                if thread.kind == ThreadKind::System {
                    ListItem::new(Line::from(vec![
                        Span::styled("System0", Style::default().fg(Color::Cyan)),
                        Span::styled(" [active]", Style::default().fg(Color::DarkGray)),
                    ]))
                } else {
                    let mode_label = if thread.read_only { " saved" } else { " live" };
                    ListItem::new(Line::from(vec![
                        Span::raw(thread.title.clone()),
                        Span::raw(" ["),
                        Span::styled(thread.state.clone(), status_style(&thread.state)),
                        Span::raw("]"),
                        Span::styled(mode_label, Style::default().fg(Color::DarkGray)),
                    ]))
                }
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(
            self.sidebar_selected
                .min(self.threads.len().saturating_sub(1)),
        ));

        let mut highlight = Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        if self.focus != FocusPane::Sidebar {
            highlight = highlight.remove_modifier(Modifier::BOLD);
        }

        let list = List::new(items)
            .block(block)
            .highlight_symbol("▶ ")
            .highlight_style(highlight);

        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_thread(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let thread = self.current_thread().clone();
        let title = if thread.read_only {
            format!("Conversation: {} (read-only)", thread.title)
        } else {
            format!("Conversation: {}", thread.title)
        };

        let block = if self.focus == FocusPane::Main {
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue))
        } else {
            Block::default().title(title).borders(Borders::ALL)
        };

        let selected = self
            .current_thread_selected_item()
            .min(thread.items.len().saturating_sub(1));
        let (lines, ranges) = build_thread_lines(&thread, selected);
        let viewport_rows = area.height.saturating_sub(2) as usize;
        self.last_main_viewport_rows = viewport_rows.max(1);

        self.ensure_selected_visible_for(&thread.id, &ranges, viewport_rows.max(1));

        let max_vertical = lines.len().saturating_sub(viewport_rows.max(1));
        let mut vertical_offset = self
            .vertical_scroll_by_thread
            .get(&thread.id)
            .copied()
            .unwrap_or(0)
            .min(max_vertical);
        self.vertical_scroll_by_thread
            .insert(thread.id.clone(), vertical_offset);

        let horizontal_offset = self
            .horizontal_scroll_by_thread
            .get(&thread.id)
            .copied()
            .unwrap_or(0);

        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .scroll((vertical_offset as u16, horizontal_offset as u16));

        frame.render_widget(paragraph, area);

        vertical_offset = self
            .vertical_scroll_by_thread
            .get(&thread.id)
            .copied()
            .unwrap_or(0)
            .min(max_vertical);
        self.vertical_scroll_by_thread
            .insert(thread.id, vertical_offset);
    }

    fn render_input(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let read_only = self.current_thread().read_only;
        let title = if read_only {
            "Input (read-only sub-agent thread)"
        } else {
            "Input"
        };

        let mut block = Block::default().title(title).borders(Borders::ALL);
        if self.focus == FocusPane::Input && !read_only {
            block = block.border_style(Style::default().fg(Color::Green));
        } else if read_only {
            block = block.border_style(Style::default().fg(Color::DarkGray));
        }

        let lines = self.composer.prefixed_lines();
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((self.composer.scroll_line_offset as u16, 0));

        frame.render_widget(paragraph, area);
        self.last_input_cursor_rel = None;

        if self.focus != FocusPane::Input || read_only {
            return;
        }

        let inner = area.inner(ratatui::layout::Margin {
            horizontal: 1,
            vertical: 1,
        });
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let (cursor_x_rel, cursor_y_rel) = self.composer.cursor_screen_position();
        self.last_input_cursor_rel = Some((cursor_x_rel, cursor_y_rel));

        let clamped_x = cursor_x_rel.min(inner.width.saturating_sub(1));
        let clamped_y = cursor_y_rel.min(inner.height.saturating_sub(1));
        frame.set_cursor_position((inner.x + clamped_x, inner.y + clamped_y));
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let status = self.status.clone().unwrap_or_else(|| {
            "Tab/Shift+Tab: focus | Enter: send/open | Space: expand | Left/Right: h-scroll | Ctrl+J/Ctrl+N/Shift+Enter: newline | q: quit".to_string()
        });
        let text = Text::from(Line::from(vec![Span::styled(
            status,
            Style::default().fg(Color::DarkGray),
        )]));
        frame.render_widget(Paragraph::new(text), area);
    }

    fn handle_event(&mut self, event: Event, clipboard: &mut dyn ClipboardProvider) -> AppAction {
        match event {
            Event::Key(key) => self.handle_key_event(key, clipboard),
            Event::Paste(text) => {
                if self.focus == FocusPane::Input && !self.current_thread().read_only {
                    self.composer.insert_str(&text);
                }
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    fn handle_key_event(
        &mut self,
        key: KeyEvent,
        clipboard: &mut dyn ClipboardProvider,
    ) -> AppAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return AppAction::None;
        }

        let read_only_input = self.current_thread().read_only;

        if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
            return AppAction::Quit;
        }

        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.focus = self.focus.prev();
            return AppAction::None;
        }

        if key.code == KeyCode::Tab {
            self.focus = self.focus.next();
            return AppAction::None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J'))
        {
            if self.focus == FocusPane::Input && !read_only_input {
                self.composer.newline();
            }
            return AppAction::None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
        {
            if self.focus == FocusPane::Input && !read_only_input {
                self.composer.newline();
            }
            return AppAction::None;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
        {
            if self.focus == FocusPane::Input && !read_only_input {
                match clipboard.read_text() {
                    Ok(Some(text)) => {
                        self.composer.insert_str(&text);
                    }
                    Ok(None) => {
                        self.status = Some("Clipboard unavailable".to_string());
                    }
                    Err(err) => {
                        self.status = Some(format!("Clipboard read failed: {err}"));
                    }
                }
            }
            return AppAction::None;
        }

        if key.code == KeyCode::Enter
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && self.focus == FocusPane::Input
            && !read_only_input
        {
            self.composer.newline();
            return AppAction::None;
        }

        match key.code {
            KeyCode::Up => {
                match self.focus {
                    FocusPane::Sidebar => {
                        self.sidebar_selected = self.sidebar_selected.saturating_sub(1);
                    }
                    FocusPane::Main => self.select_main_prev(),
                    FocusPane::Input => {
                        if !read_only_input {
                            self.composer.cursor_up();
                        }
                    }
                }
                AppAction::None
            }
            KeyCode::Down => {
                match self.focus {
                    FocusPane::Sidebar => {
                        let max = self.threads.len().saturating_sub(1);
                        self.sidebar_selected = (self.sidebar_selected + 1).min(max);
                    }
                    FocusPane::Main => self.select_main_next(),
                    FocusPane::Input => {
                        if !read_only_input {
                            self.composer.cursor_down();
                        }
                    }
                }
                AppAction::None
            }
            KeyCode::Left => {
                match self.focus {
                    FocusPane::Main => {
                        let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                            8
                        } else {
                            1
                        };
                        let current = self.current_thread_horizontal_offset();
                        self.set_current_thread_horizontal_offset(current.saturating_sub(step));
                    }
                    FocusPane::Input if !read_only_input => {
                        self.composer.cursor_left();
                    }
                    _ => {}
                }
                AppAction::None
            }
            KeyCode::Right => {
                match self.focus {
                    FocusPane::Main => {
                        let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                            8
                        } else {
                            1
                        };
                        let current = self.current_thread_horizontal_offset();
                        self.set_current_thread_horizontal_offset(current.saturating_add(step));
                    }
                    FocusPane::Input if !read_only_input => {
                        self.composer.cursor_right();
                    }
                    _ => {}
                }
                AppAction::None
            }
            KeyCode::Home => {
                if self.focus == FocusPane::Main {
                    self.set_current_thread_horizontal_offset(0);
                }
                AppAction::None
            }
            KeyCode::Backspace => {
                if self.focus == FocusPane::Input && !read_only_input {
                    self.composer.backspace();
                }
                AppAction::None
            }
            KeyCode::Char(' ') => {
                if self.focus == FocusPane::Main {
                    self.toggle_selected_item();
                } else if self.focus == FocusPane::Input && !read_only_input {
                    self.composer.insert_char(' ');
                }
                AppAction::None
            }
            KeyCode::Enter => match self.focus {
                FocusPane::Sidebar => self.open_selected_sidebar_thread(),
                FocusPane::Input => {
                    if read_only_input {
                        return AppAction::None;
                    }
                    if let Some(message) = self.composer.submit() {
                        self.start_local_send(message)
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Main => self.open_selected_main_link(),
            },
            KeyCode::Char(ch) => {
                if self.focus == FocusPane::Input
                    && !read_only_input
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                {
                    self.composer.insert_char(ch);
                }
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    fn start_local_send(&mut self, message: String) -> AppAction {
        let thread_id = self.current_thread().id.clone();
        let is_system = thread_id == SYSTEM_THREAD_ID;

        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
        {
            thread
                .items
                .push(TimelineItem::text(MessageRoleTag::User, message.clone()));
            thread
                .items
                .push(TimelineItem::text(MessageRoleTag::Assistant, "Thinking..."));
            let placeholder_idx = thread.items.len().saturating_sub(1);
            self.pending_placeholder_index_by_thread
                .insert(thread.id.clone(), placeholder_idx);
            self.selected_item_by_thread
                .insert(thread.id.clone(), placeholder_idx);
        }

        self.pending_thread_id = Some(thread_id.clone());
        self.status = Some("Sending...".to_string());

        if is_system {
            AppAction::SubmitSystemTurn(message)
        } else {
            AppAction::SubmitSubagentTurn { thread_id, message }
        }
    }

    fn select_main_prev(&mut self) {
        let selected = self.current_thread_selected_item();
        self.set_current_thread_selected_item(selected.saturating_sub(1));
        self.ensure_selected_visible_current();
    }

    fn select_main_next(&mut self) {
        let max = self.current_thread().items.len().saturating_sub(1);
        let selected = self.current_thread_selected_item();
        self.set_current_thread_selected_item((selected + 1).min(max));
        self.ensure_selected_visible_current();
    }

    fn toggle_selected_item(&mut self) {
        let selected = self.current_thread_selected_item();
        let thread_id = self.current_thread().id.clone();
        if let Some(item) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.items.get_mut(selected))
            && item.is_toggleable()
        {
            item.toggle();
        }
        self.ensure_selected_visible_current();
    }

    fn open_selected_sidebar_thread(&mut self) -> AppAction {
        self.active_thread = self
            .sidebar_selected
            .min(self.threads.len().saturating_sub(1));

        let thread = self.current_thread().clone();
        if thread.id == SYSTEM_THREAD_ID {
            return AppAction::OpenThread {
                thread_id: thread.id,
                resurrect: false,
            };
        }

        AppAction::OpenThread {
            thread_id: thread.id,
            resurrect: thread.read_only,
        }
    }

    fn open_selected_main_link(&mut self) -> AppAction {
        let selected = self.current_thread_selected_item();
        let Some(thread_id) = self
            .current_thread()
            .items
            .get(selected)
            .and_then(TimelineItem::linked_thread_id)
        else {
            return AppAction::None;
        };

        if self.select_thread_by_id(&thread_id) {
            let resurrect = self.current_thread().read_only;
            self.status = Some(format!("Opening thread {}", short_thread_id(&thread_id)));
            return AppAction::OpenThread {
                thread_id,
                resurrect,
            };
        }

        self.status = Some(format!(
            "Thread {} not found in sidebar",
            short_thread_id(&thread_id)
        ));
        AppAction::None
    }

    fn select_thread_by_id(&mut self, thread_id: &str) -> bool {
        let Some(idx) = self
            .threads
            .iter()
            .position(|thread| thread.id == thread_id)
        else {
            return false;
        };

        self.active_thread = idx;
        self.sidebar_selected = idx;
        true
    }

    fn current_thread(&self) -> &ThreadView {
        &self.threads[self.active_thread.min(self.threads.len().saturating_sub(1))]
    }

    fn current_thread_selected_item(&self) -> usize {
        let thread_id = &self.current_thread().id;
        self.selected_item_by_thread
            .get(thread_id)
            .copied()
            .unwrap_or(0)
    }

    fn set_current_thread_selected_item(&mut self, idx: usize) {
        let thread_id = self.current_thread().id.clone();
        self.selected_item_by_thread.insert(thread_id, idx);
    }

    fn current_thread_horizontal_offset(&self) -> usize {
        let thread_id = &self.current_thread().id;
        self.horizontal_scroll_by_thread
            .get(thread_id)
            .copied()
            .unwrap_or(0)
    }

    fn set_current_thread_horizontal_offset(&mut self, offset: usize) {
        let thread_id = self.current_thread().id.clone();
        self.horizontal_scroll_by_thread.insert(thread_id, offset);
    }

    fn ensure_selected_visible_current(&mut self) {
        let thread = self.current_thread().clone();
        let selected = self
            .current_thread_selected_item()
            .min(thread.items.len().saturating_sub(1));
        let (_, ranges) = build_thread_lines(&thread, selected);
        self.ensure_selected_visible_for(&thread.id, &ranges, self.last_main_viewport_rows.max(1));
    }

    fn ensure_selected_visible_for(
        &mut self,
        thread_id: &str,
        ranges: &[(usize, usize)],
        viewport_rows: usize,
    ) {
        if ranges.is_empty() {
            self.vertical_scroll_by_thread
                .insert(thread_id.to_string(), 0);
            return;
        }

        let selected = self
            .selected_item_by_thread
            .get(thread_id)
            .copied()
            .unwrap_or(0)
            .min(ranges.len().saturating_sub(1));
        let (start, end) = ranges[selected];

        let mut offset = self
            .vertical_scroll_by_thread
            .get(thread_id)
            .copied()
            .unwrap_or(0);

        if start < offset {
            offset = start;
        } else if end >= offset + viewport_rows {
            offset = end + 1 - viewport_rows;
        }

        self.vertical_scroll_by_thread
            .insert(thread_id.to_string(), offset);
    }

    fn system_thread_snapshot(&self) -> serde_json::Value {
        let messages = self
            .threads
            .iter()
            .find(|thread| thread.id == SYSTEM_THREAD_ID)
            .map(|thread| {
                thread
                    .items
                    .iter()
                    .map(TimelineItem::to_snapshot_value)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        serde_json::json!({
            "thread_id": SYSTEM_THREAD_ID,
            "messages": messages,
        })
    }
}

fn build_thread_lines(
    thread: &ThreadView,
    selected: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    if thread.items.is_empty() {
        return (
            vec![Line::from(Span::styled(
                "No messages yet",
                Style::default().fg(Color::DarkGray),
            ))],
            vec![],
        );
    }

    let mut lines = Vec::new();
    let mut ranges = Vec::new();
    let assistant_label = thread.assistant_badge_label();
    for (idx, item) in thread.items.iter().enumerate() {
        let start = lines.len();
        let item_lines = item.lines(selected == idx, &assistant_label);
        lines.extend(item_lines);
        let end = lines.len().saturating_sub(1);
        ranges.push((start, end));
    }

    (lines, ranges)
}

async fn fetch_all_thread_events(
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

fn timeline_items_from_events(kind: &ThreadKind, events: &[ThreadEvent]) -> Vec<TimelineItem> {
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
                ));
            }
            "thread_resurrected" => {
                items.push(TimelineItem::text(
                    MessageRoleTag::System,
                    "Thread resurrected and ready for continuation.",
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
            expanded: false,
        }
    } else {
        TimelineItem::ToolInvocation {
            call_id,
            tool_id,
            status,
            arguments,
            output,
            expanded: false,
        }
    }
}

fn short_thread_id(thread_id: &str) -> String {
    thread_id.chars().take(8).collect()
}

#[derive(Debug, Clone)]
struct ComposerState {
    buffer: String,
    cursor_byte: usize,
    scroll_line_offset: usize,
    max_visible_lines: usize,
    preferred_column: Option<usize>,
}

impl ComposerState {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            cursor_byte: 0,
            scroll_line_offset: 0,
            max_visible_lines: DEFAULT_MAX_VISIBLE_LINES,
            preferred_column: None,
        }
    }

    fn input_height(&self) -> usize {
        self.logical_line_count()
            .max(1)
            .min(self.max_visible_lines.max(1))
    }

    fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor_byte, ch);
        self.cursor_byte += ch.len_utf8();
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    fn insert_str(&mut self, input: &str) {
        let normalized = normalize_line_endings(input);
        if normalized.is_empty() {
            return;
        }

        self.buffer.insert_str(self.cursor_byte, &normalized);
        self.cursor_byte += normalized.len();
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    fn newline(&mut self) {
        self.insert_char('\n');
    }

    fn backspace(&mut self) {
        if self.cursor_byte == 0 {
            return;
        }

        let previous_byte = self.buffer[..self.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);

        self.buffer.drain(previous_byte..self.cursor_byte);
        self.cursor_byte = previous_byte;
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    fn cursor_left(&mut self) {
        if self.cursor_byte == 0 {
            return;
        }

        self.cursor_byte = self.buffer[..self.cursor_byte]
            .char_indices()
            .next_back()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    fn cursor_right(&mut self) {
        if self.cursor_byte >= self.buffer.len() {
            return;
        }

        let mut iter = self.buffer[self.cursor_byte..].chars();
        if let Some(next) = iter.next() {
            self.cursor_byte += next.len_utf8();
        }
        self.preferred_column = None;
        self.ensure_cursor_visible();
    }

    fn cursor_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }

        let desired_col = self.preferred_column.unwrap_or(col);
        self.set_cursor_line_col(line - 1, desired_col);
        self.preferred_column = Some(desired_col);
        self.ensure_cursor_visible();
    }

    fn cursor_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        let max_line = self.logical_line_count().saturating_sub(1);
        if line >= max_line {
            return;
        }

        let desired_col = self.preferred_column.unwrap_or(col);
        self.set_cursor_line_col(line + 1, desired_col);
        self.preferred_column = Some(desired_col);
        self.ensure_cursor_visible();
    }

    fn submit(&mut self) -> Option<String> {
        if self.buffer.trim().is_empty() {
            return None;
        }

        let output = std::mem::take(&mut self.buffer);
        self.cursor_byte = 0;
        self.scroll_line_offset = 0;
        self.preferred_column = None;
        Some(output)
    }

    fn prefixed_lines(&self) -> Vec<Line<'static>> {
        self.logical_lines()
            .into_iter()
            .map(|line| Line::from(format!("{INPUT_PREFIX}{line}")))
            .collect()
    }

    fn cursor_screen_position(&self) -> (u16, u16) {
        let (line, col) = self.cursor_line_col();
        let rel_y = line.saturating_sub(self.scroll_line_offset);
        ((INPUT_PREFIX.chars().count() + col) as u16, rel_y as u16)
    }

    fn ensure_cursor_visible(&mut self) {
        let visible = self.max_visible_lines.max(1);
        let (line, _) = self.cursor_line_col();

        if line < self.scroll_line_offset {
            self.scroll_line_offset = line;
            return;
        }

        if line >= self.scroll_line_offset + visible {
            self.scroll_line_offset = line + 1 - visible;
        }
    }

    fn logical_lines(&self) -> Vec<&str> {
        self.buffer.split('\n').collect()
    }

    fn logical_line_count(&self) -> usize {
        self.logical_lines().len()
    }

    fn cursor_line_col(&self) -> (usize, usize) {
        let starts = self.line_starts();
        let mut line_idx = 0;
        for (idx, start) in starts.iter().enumerate() {
            if *start > self.cursor_byte {
                break;
            }
            line_idx = idx;
        }

        let line_start = starts[line_idx];
        let col = self.buffer[line_start..self.cursor_byte].chars().count();
        (line_idx, col)
    }

    fn set_cursor_line_col(&mut self, line_idx: usize, desired_col: usize) {
        let starts = self.line_starts();
        let line_idx = line_idx.min(starts.len().saturating_sub(1));
        let line_start = starts[line_idx];
        let line_end = if line_idx + 1 < starts.len() {
            starts[line_idx + 1].saturating_sub(1)
        } else {
            self.buffer.len()
        };

        let line = &self.buffer[line_start..line_end];
        let char_len = line.chars().count();
        let target_col = desired_col.min(char_len);
        let target_offset = if target_col == char_len {
            line.len()
        } else {
            line.char_indices()
                .nth(target_col)
                .map(|(idx, _)| idx)
                .unwrap_or(line.len())
        };

        self.cursor_byte = line_start + target_offset;
    }

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0];
        for (idx, ch) in self.buffer.char_indices() {
            if ch == '\n' {
                starts.push(idx + 1);
            }
        }
        starts
    }
}

fn normalize_line_endings(input: &str) -> String {
    input.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventState, KeyModifiers};

    struct MockClipboard {
        value: Option<String>,
    }

    impl ClipboardProvider for MockClipboard {
        fn read_text(&mut self) -> Result<Option<String>, String> {
            Ok(self.value.take())
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn subagent_summary(id: &str, active: bool, resurrected: bool) -> ThreadSummary {
        ThreadSummary {
            thread_id: id.to_string(),
            kind: "subagent".to_string(),
            agent_id: Some("planner".to_string()),
            latest_run_id: Some("run-1".to_string()),
            state: if active {
                "running".to_string()
            } else {
                "completed".to_string()
            },
            updated_at: "2026-02-14T00:00:00Z".to_string(),
            resurrected,
            active,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn composer_normalizes_line_endings_on_insert_str() {
        let mut composer = ComposerState::new();
        composer.insert_str("a\r\nb\rc");
        assert_eq!(composer.buffer, "a\nb\nc");
    }

    #[test]
    fn shift_tab_cycles_focus_backwards() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Input;
        let mut clipboard = MockClipboard { value: None };
        let _ = app.handle_event(Event::Key(key(KeyCode::BackTab)), &mut clipboard);
        assert_eq!(app.focus, FocusPane::Main);
    }

    #[test]
    fn ctrl_n_inserts_newline() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Input;
        let mut clipboard = MockClipboard { value: None };
        app.composer.insert_str("hello");
        let _ = app.handle_event(
            Event::Key(key_with_modifiers(
                KeyCode::Char('n'),
                KeyModifiers::CONTROL,
            )),
            &mut clipboard,
        );
        assert_eq!(app.composer.buffer, "hello\n");
    }

    #[test]
    fn saved_subagent_threads_are_read_only_until_resurrected() {
        let mut app = ChatApp::new();
        app.sync_thread_summaries(vec![subagent_summary("thread-1", false, false)]);
        let thread = app
            .threads
            .iter()
            .find(|thread| thread.id == "thread-1")
            .expect("thread should exist");
        assert!(thread.read_only);

        app.sync_thread_summaries(vec![subagent_summary("thread-1", true, true)]);
        let thread = app
            .threads
            .iter()
            .find(|thread| thread.id == "thread-1")
            .expect("thread should exist");
        assert!(!thread.read_only);
    }

    #[test]
    fn delegate_card_enter_opens_subagent_thread() {
        let mut app = ChatApp::new();
        app.sync_thread_summaries(vec![subagent_summary("thread-abc", false, false)]);

        if let Some(system) = app
            .threads
            .iter_mut()
            .find(|thread| thread.id == SYSTEM_THREAD_ID)
        {
            system.items.push(TimelineItem::DelegateInvocation {
                call_id: "call-1".to_string(),
                status: "success".to_string(),
                target_agent: Some("planner".to_string()),
                thread_id: Some("thread-abc".to_string()),
                run_id: Some("run-1".to_string()),
                instruction: Some("do it".to_string()),
                summary: Some("done".to_string()),
                error: None,
                arguments: serde_json::json!({}),
                output: serde_json::json!({}),
                expanded: false,
            });
        }

        app.focus = FocusPane::Main;
        app.active_thread = 0;
        app.sidebar_selected = 0;
        app.selected_item_by_thread
            .insert(SYSTEM_THREAD_ID.to_string(), 0);

        let mut clipboard = MockClipboard { value: None };
        let action = app.handle_event(Event::Key(key(KeyCode::Enter)), &mut clipboard);
        assert_eq!(
            action,
            AppAction::OpenThread {
                thread_id: "thread-abc".to_string(),
                resurrect: true,
            }
        );
    }

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
        }];

        let items = timeline_items_from_events(&ThreadKind::Subagent, &events);
        assert!(matches!(
            &items[0],
            TimelineItem::Text { role: MessageRoleTag::System, text, .. }
                if text.contains("Initial instruction unavailable")
        ));
    }

    #[test]
    fn assistant_badge_uses_system_and_agent_names() {
        let mut system = ThreadView::system();
        system.items.push(TimelineItem::text(
            MessageRoleTag::Assistant,
            "system reply",
        ));
        let (system_lines, _) = build_thread_lines(&system, 0);
        assert!(
            line_text(&system_lines[0]).starts_with("[System0] "),
            "system assistant label should be System0"
        );

        let mut subagent = ThreadView::from_summary(&subagent_summary("thread-2", true, false));
        subagent
            .items
            .push(TimelineItem::text(MessageRoleTag::Assistant, "agent reply"));
        let (subagent_lines, _) = build_thread_lines(&subagent, 0);
        assert!(
            line_text(&subagent_lines[0]).starts_with("[planner] "),
            "sub-agent assistant label should use the agent id"
        );
    }

    #[test]
    fn horizontal_scroll_updates_for_main_focus() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Main;
        let mut clipboard = MockClipboard { value: None };
        let _ = app.handle_event(Event::Key(key(KeyCode::Right)), &mut clipboard);
        assert_eq!(app.current_thread_horizontal_offset(), 1);
        let _ = app.handle_event(
            Event::Key(key_with_modifiers(KeyCode::Left, KeyModifiers::SHIFT)),
            &mut clipboard,
        );
        assert_eq!(app.current_thread_horizontal_offset(), 0);
    }

    #[test]
    fn start_local_send_appends_user_and_placeholder() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Input;
        let action = app.start_local_send("hello".to_string());
        assert_eq!(action, AppAction::SubmitSystemTurn("hello".to_string()));
        let system = app
            .threads
            .iter()
            .find(|thread| thread.id == SYSTEM_THREAD_ID)
            .expect("system thread should exist");
        assert_eq!(system.items.len(), 2);
    }

    #[test]
    fn parse_ndjson_supports_json_message_payloads() {
        assert_eq!(
            parse_ndjson_message_line(r#"{"message":"line1\nline2"}"#),
            Some("line1\nline2".to_string())
        );
        assert_eq!(
            parse_ndjson_message_line(r#""plain""#),
            Some("plain".to_string())
        );
    }
}
