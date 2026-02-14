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
    DelegatedRun, OrchestratorRunGetParams, OrchestratorThreadMessage, OrchestratorToolInvocation,
    OrchestratorTurnResult,
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

    loop {
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
            AppAction::SendTurn(message) => {
                if let Err(err) = app.send_system_turn(rpc, message).await {
                    app.status = Some(err.to_string());
                }
            }
            AppAction::LoadRun(run_id) => {
                if let Err(err) = app.refresh_run_thread(rpc, &run_id).await {
                    app.status = Some(err.to_string());
                }
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
            "runs": app.delegated_runs,
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

        match app.send_system_turn(rpc, message.clone()).await {
            Ok(turn_result) => {
                emit_ndjson(
                    output,
                    serde_json::json!({
                        "event": "turn_complete",
                        "input": message,
                        "turn_result": turn_result,
                        "system_thread": app.system_thread_snapshot(),
                        "runs": app.delegated_runs,
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
                            "message": err.to_string(),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppAction {
    None,
    Quit,
    SendTurn(String),
    LoadRun(String),
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

    fn from_wire_role(role: &str) -> Self {
        match role {
            "system" => Self::System,
            "user" => Self::User,
            _ => Self::Assistant,
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

    fn to_list_item(&self, selected: bool) -> ListItem<'static> {
        let style = if selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        match self {
            TimelineItem::Text {
                role,
                text,
                expanded,
            } => {
                let mut lines = Vec::new();
                let role_prefix = Span::styled(
                    format!("[{}] ", role.label()),
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
                    lines.push(Line::from(format!("           {line}")));
                }

                if text_is_collapsible(text) {
                    let hint = if *expanded {
                        "           [expanded] Space to collapse"
                    } else {
                        "           [collapsed] Space to expand"
                    };
                    lines.push(Line::from(Span::styled(
                        hint,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::raw(""));

                ListItem::new(Text::from(lines)).style(style)
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
                let mut header = vec![
                    Span::styled(
                        "[TOOL] ",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(tool_id.clone(), Style::default().fg(Color::LightYellow)),
                    Span::raw(" "),
                    Span::raw("status="),
                    Span::styled(status.clone(), status_style(status)),
                ];

                if *expanded {
                    header.push(Span::styled(
                        " [expanded]",
                        Style::default().fg(Color::DarkGray),
                    ));
                    lines.push(Line::from(header));
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
                } else {
                    header.push(Span::styled(
                        " [collapsed]",
                        Style::default().fg(Color::DarkGray),
                    ));
                    lines.push(Line::from(header));
                }
                lines.push(Line::raw(""));

                ListItem::new(Text::from(lines)).style(style)
            }
        }
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
        }
    }

    fn toggle(&mut self) {
        match self {
            TimelineItem::Text { text, expanded, .. } => {
                if text_is_collapsible(text) {
                    *expanded = !*expanded;
                }
            }
            TimelineItem::ToolInvocation { expanded, .. } => *expanded = !*expanded,
        }
    }

    fn is_toggleable(&self) -> bool {
        match self {
            TimelineItem::Text { text, .. } => text_is_collapsible(text),
            TimelineItem::ToolInvocation { .. } => true,
        }
    }

    fn linked_run_id(&self) -> Option<String> {
        match self {
            TimelineItem::ToolInvocation { output, .. } => output
                .get("run_id")
                .and_then(|value| value.as_str())
                .map(|run_id| run_id.to_string()),
            TimelineItem::Text { text, .. } => parse_run_id_from_text(text),
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
        "running" => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::Gray),
    }
}

fn parse_run_id_from_text(text: &str) -> Option<String> {
    text.strip_prefix("delegated run ")
        .and_then(|rest| rest.split_whitespace().next())
        .map(|run_id| run_id.to_string())
}

#[derive(Debug, Clone)]
struct ThreadView {
    id: String,
    title: String,
    run_id: Option<String>,
    state: Option<String>,
    read_only: bool,
    items: Vec<TimelineItem>,
}

impl ThreadView {
    fn system() -> Self {
        Self {
            id: SYSTEM_THREAD_ID.to_string(),
            title: "System0".to_string(),
            run_id: None,
            state: None,
            read_only: false,
            items: vec![],
        }
    }

    fn delegated_run(run: &DelegatedRun) -> Self {
        Self {
            id: run.run_id.clone(),
            title: format!("{} ({})", run.agent_id, short_run_id(&run.run_id)),
            run_id: Some(run.run_id.clone()),
            state: Some(run.state.clone()),
            read_only: true,
            items: run_thread_items(run),
        }
    }
}

#[derive(Debug, Clone)]
struct ChatApp {
    threads: Vec<ThreadView>,
    delegated_runs: Vec<DelegatedRun>,
    sidebar_selected: usize,
    active_thread: usize,
    focus: FocusPane,
    selected_item_by_thread: HashMap<String, usize>,
    composer: ComposerState,
    status: Option<String>,
    last_input_cursor_rel: Option<(u16, u16)>,
}

impl ChatApp {
    fn new() -> Self {
        Self {
            threads: vec![ThreadView::system()],
            delegated_runs: vec![],
            sidebar_selected: 0,
            active_thread: 0,
            focus: FocusPane::Input,
            selected_item_by_thread: HashMap::new(),
            composer: ComposerState::new(),
            status: Some("System0 thread ready".to_string()),
            last_input_cursor_rel: None,
        }
    }

    async fn bootstrap(&mut self, rpc: &RpcClient) -> Result<(), CliError> {
        let context = rpc.orchestrator_context_get().await?.messages;
        self.load_system_context(context);

        let runs = rpc.orchestrator_runs_list().await?.runs;
        self.sync_runs(runs);

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

    async fn send_system_turn(
        &mut self,
        rpc: &RpcClient,
        message: String,
    ) -> Result<OrchestratorTurnResult, CliError> {
        self.active_thread = 0;
        self.sidebar_selected = 0;
        self.append_system_item(TimelineItem::text(MessageRoleTag::User, message.clone()));

        let result = rpc
            .orchestrator_turn(neuromancer_core::rpc::OrchestratorTurnParams { message })
            .await?;

        self.append_system_item(TimelineItem::text(
            MessageRoleTag::Assistant,
            result.response.clone(),
        ));

        for invocation in &result.tool_invocations {
            self.append_system_item(tool_item_from_invocation(invocation));
        }

        for run in &result.delegated_runs {
            self.append_system_item(TimelineItem::text(
                MessageRoleTag::System,
                format!(
                    "delegated run {} agent={} state={}",
                    run.run_id, run.agent_id, run.state
                ),
            ));
        }

        let runs = rpc.orchestrator_runs_list().await?.runs;
        self.sync_runs(runs);
        self.status = Some("Turn completed".to_string());

        Ok(result)
    }

    async fn refresh_run_thread(&mut self, rpc: &RpcClient, run_id: &str) -> Result<(), CliError> {
        let run = rpc
            .orchestrator_run_get(OrchestratorRunGetParams {
                run_id: run_id.to_string(),
            })
            .await?
            .run;

        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == run.run_id)
        {
            thread.items = run_thread_items(&run);
            thread.state = Some(run.state);
            self.status = Some(format!("Loaded run {}", short_run_id(run_id)));
            return Ok(());
        }

        Err(CliError::Usage(format!(
            "delegated run thread '{}' not found",
            run_id
        )))
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
                if thread.id == SYSTEM_THREAD_ID {
                    ListItem::new(Line::from(vec![
                        Span::styled("System0", Style::default().fg(Color::Cyan)),
                        Span::styled(" [active]", Style::default().fg(Color::DarkGray)),
                    ]))
                } else {
                    let state = thread
                        .state
                        .clone()
                        .unwrap_or_else(|| "unknown".to_string());
                    ListItem::new(Line::from(vec![
                        Span::raw(thread.title.clone()),
                        Span::raw(" ["),
                        Span::styled(state.clone(), status_style(&state)),
                        Span::raw("]"),
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
        let thread = self.current_thread();
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
        let selected = self.current_thread_selected_item();
        let items: Vec<ListItem<'static>> = if thread.items.is_empty() {
            vec![ListItem::new(Text::from(vec![Line::from(Span::styled(
                "No messages yet",
                Style::default().fg(Color::DarkGray),
            ))]))]
        } else {
            thread
                .items
                .iter()
                .enumerate()
                .map(|(idx, item)| item.to_list_item(selected == idx))
                .collect()
        };

        let mut state = ListState::default();
        if !thread.items.is_empty() {
            state.select(Some(selected.min(thread.items.len().saturating_sub(1))));
        }

        let mut highlight = Style::default()
            .fg(Color::White)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD);
        if self.focus != FocusPane::Main {
            highlight = highlight.remove_modifier(Modifier::BOLD);
        }

        let list = List::new(items)
            .block(block)
            .highlight_symbol("▶ ")
            .highlight_style(highlight);

        frame.render_stateful_widget(list, area, &mut state);
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
            "Tab: switch focus | Enter: send/open | Space: expand/collapse | Shift+Enter/Ctrl+J: newline"
                .to_string()
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
                if self.focus == FocusPane::Input && !read_only_input {
                    self.composer.cursor_left();
                }
                AppAction::None
            }
            KeyCode::Right => {
                if self.focus == FocusPane::Input && !read_only_input {
                    self.composer.cursor_right();
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
                FocusPane::Sidebar => {
                    self.active_thread = self
                        .sidebar_selected
                        .min(self.threads.len().saturating_sub(1));
                    if let Some(run_id) = self.current_thread().run_id.clone() {
                        AppAction::LoadRun(run_id)
                    } else {
                        AppAction::None
                    }
                }
                FocusPane::Input => {
                    if read_only_input {
                        return AppAction::None;
                    }
                    if let Some(message) = self.composer.submit() {
                        AppAction::SendTurn(message)
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

    fn select_main_prev(&mut self) {
        let selected = self.current_thread_selected_item();
        self.set_current_thread_selected_item(selected.saturating_sub(1));
    }

    fn select_main_next(&mut self) {
        let max = self.current_thread().items.len().saturating_sub(1);
        let selected = self.current_thread_selected_item();
        self.set_current_thread_selected_item((selected + 1).min(max));
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
    }

    fn open_selected_main_link(&mut self) -> AppAction {
        let selected = self.current_thread_selected_item();
        let Some(run_id) = self
            .current_thread()
            .items
            .get(selected)
            .and_then(TimelineItem::linked_run_id)
        else {
            return AppAction::None;
        };

        if self.select_thread_by_run_id(&run_id) {
            self.status = Some(format!("Opening run {}", short_run_id(&run_id)));
            return AppAction::LoadRun(run_id);
        }

        self.status = Some(format!(
            "Run {} not found in sidebar",
            short_run_id(&run_id)
        ));
        AppAction::None
    }

    fn select_thread_by_run_id(&mut self, run_id: &str) -> bool {
        let Some(idx) = self
            .threads
            .iter()
            .position(|thread| thread.run_id.as_deref() == Some(run_id) || thread.id == run_id)
        else {
            return false;
        };

        self.active_thread = idx;
        self.sidebar_selected = idx;
        true
    }

    fn sync_runs(&mut self, runs: Vec<DelegatedRun>) {
        let current_active_id = self.current_thread().id.clone();
        let mut existing = std::mem::take(&mut self.threads)
            .into_iter()
            .map(|thread| (thread.id.clone(), thread))
            .collect::<HashMap<_, _>>();

        let mut threads = Vec::new();
        threads.push(
            existing
                .remove(SYSTEM_THREAD_ID)
                .unwrap_or_else(ThreadView::system),
        );

        for run in runs.iter().rev() {
            let mut thread = existing
                .remove(&run.run_id)
                .unwrap_or_else(|| ThreadView::delegated_run(run));
            thread.title = format!("{} ({})", run.agent_id, short_run_id(&run.run_id));
            thread.run_id = Some(run.run_id.clone());
            thread.state = Some(run.state.clone());
            thread.read_only = true;
            if thread.items.is_empty() {
                thread.items = run_thread_items(run);
            }
            threads.push(thread);
        }

        self.delegated_runs = runs;
        self.threads = threads;

        self.active_thread = self
            .threads
            .iter()
            .position(|thread| thread.id == current_active_id)
            .unwrap_or(0);
        self.sidebar_selected = self
            .sidebar_selected
            .min(self.threads.len().saturating_sub(1));
    }

    fn append_system_item(&mut self, item: TimelineItem) {
        if let Some(system_thread) = self.threads.get_mut(0) {
            system_thread.items.push(item);
            let idx = system_thread.items.len().saturating_sub(1);
            self.selected_item_by_thread
                .insert(system_thread.id.clone(), idx);
        }
    }

    fn load_system_context(&mut self, messages: Vec<OrchestratorThreadMessage>) {
        let items = messages
            .into_iter()
            .map(timeline_item_from_thread_message)
            .collect::<Vec<_>>();

        if let Some(system_thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.id == SYSTEM_THREAD_ID)
        {
            system_thread.items = items;
            if !system_thread.items.is_empty() {
                self.selected_item_by_thread.insert(
                    system_thread.id.clone(),
                    system_thread.items.len().saturating_sub(1),
                );
            }
        }
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

fn run_thread_items(run: &DelegatedRun) -> Vec<TimelineItem> {
    let mut items = Vec::new();
    items.push(TimelineItem::text(
        MessageRoleTag::System,
        format!(
            "Sub-agent trace for run {} (agent={}, state={})",
            run.run_id, run.agent_id, run.state
        ),
    ));
    if let Some(summary) = &run.summary {
        items.push(TimelineItem::text(
            MessageRoleTag::Assistant,
            summary.clone(),
        ));
    }
    items
}

fn timeline_item_from_thread_message(message: OrchestratorThreadMessage) -> TimelineItem {
    match message {
        OrchestratorThreadMessage::Text { role, content } => {
            TimelineItem::text(MessageRoleTag::from_wire_role(&role), content)
        }
        OrchestratorThreadMessage::ToolInvocation {
            call_id,
            tool_id,
            arguments,
            status,
            output,
        } => TimelineItem::ToolInvocation {
            call_id,
            tool_id,
            status,
            arguments,
            output,
            expanded: false,
        },
    }
}

fn tool_item_from_invocation(invocation: &OrchestratorToolInvocation) -> TimelineItem {
    TimelineItem::ToolInvocation {
        call_id: invocation.call_id.clone(),
        tool_id: invocation.tool_id.clone(),
        status: invocation.status.clone(),
        arguments: invocation.arguments.clone(),
        output: invocation.output.clone(),
        expanded: false,
    }
}

fn short_run_id(run_id: &str) -> String {
    run_id.chars().take(8).collect()
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
    use ratatui::backend::TestBackend;

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

    #[test]
    fn composer_normalizes_line_endings_on_insert_str() {
        let mut composer = ComposerState::new();
        composer.insert_str("a\r\nb\rc");
        assert_eq!(composer.buffer, "a\nb\nc");
    }

    #[test]
    fn composer_backspace_handles_utf8_scalars() {
        let mut composer = ComposerState::new();
        composer.insert_str("hi🙂");
        composer.backspace();
        assert_eq!(composer.buffer, "hi");
        assert_eq!(composer.cursor_byte, composer.buffer.len());
    }

    #[test]
    fn cursor_up_down_preserves_column_when_possible() {
        let mut composer = ComposerState::new();
        composer.insert_str("abcd\nef\nxyz123");
        composer.set_cursor_line_col(0, 3);

        composer.cursor_down();
        let (line, col) = composer.cursor_line_col();
        assert_eq!((line, col), (1, 2));

        composer.cursor_down();
        let (line, col) = composer.cursor_line_col();
        assert_eq!((line, col), (2, 3));
    }

    #[test]
    fn input_height_clamps_to_max_visible_lines() {
        let mut composer = ComposerState::new();
        composer.insert_str("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12");
        assert_eq!(composer.input_height(), DEFAULT_MAX_VISIBLE_LINES);
    }

    #[test]
    fn cursor_visibility_updates_scroll_offset() {
        let mut composer = ComposerState::new();
        composer.insert_str("0\n1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11");
        let (line, _) = composer.cursor_line_col();
        assert_eq!(line, 11);
        assert!(composer.scroll_line_offset > 0);
        let (_, rel_y) = composer.cursor_screen_position();
        assert_eq!(rel_y as usize, DEFAULT_MAX_VISIBLE_LINES - 1);
    }

    #[test]
    fn testbackend_renders_multiline_cursor_position() {
        let backend = TestBackend::new(80, 16);
        let mut terminal = Terminal::new(backend).expect("terminal should initialize");

        let mut app = ChatApp::new();
        app.focus = FocusPane::Input;
        app.composer.insert_str("alpha\nbeta\ngamma");
        app.composer.set_cursor_line_col(2, 2);

        terminal
            .draw(|frame| app.render(frame))
            .expect("draw should succeed");

        assert_eq!(app.last_input_cursor_rel, Some((4, 2)));
    }

    #[test]
    fn enter_submits_and_shift_enter_inserts_newline() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Input;
        app.composer.insert_str("hello");
        let mut clipboard = MockClipboard { value: None };

        let action = app.handle_event(Event::Key(key(KeyCode::Enter)), &mut clipboard);
        assert!(matches!(action, AppAction::SendTurn(_)));

        app.composer.insert_str("hello");
        let action = app.handle_event(
            Event::Key(key_with_modifiers(KeyCode::Enter, KeyModifiers::SHIFT)),
            &mut clipboard,
        );
        assert_eq!(action, AppAction::None);
        assert_eq!(app.composer.buffer, "hello\n");
    }

    #[test]
    fn sub_agent_threads_keep_input_read_only() {
        let run = DelegatedRun {
            run_id: "run-123".to_string(),
            agent_id: "planner".to_string(),
            state: "completed".to_string(),
            summary: Some("done".to_string()),
        };

        let mut app = ChatApp::new();
        app.sync_runs(vec![run]);
        app.sidebar_selected = 1;
        app.active_thread = 1;
        app.focus = FocusPane::Input;

        let mut clipboard = MockClipboard { value: None };
        let _ = app.handle_event(Event::Key(key(KeyCode::Char('x'))), &mut clipboard);

        assert!(app.composer.buffer.is_empty());
    }

    #[test]
    fn ctrl_v_uses_paste_path_with_normalized_line_endings() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Input;

        let mut clipboard = MockClipboard {
            value: Some("line1\r\nline2".to_string()),
        };

        let _ = app.handle_event(
            Event::Key(key_with_modifiers(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL,
            )),
            &mut clipboard,
        );

        assert_eq!(app.composer.buffer, "line1\nline2");
    }

    #[test]
    fn main_enter_on_delegate_invocation_opens_sub_agent_thread() {
        let run = DelegatedRun {
            run_id: "run-123".to_string(),
            agent_id: "planner".to_string(),
            state: "completed".to_string(),
            summary: Some("done".to_string()),
        };

        let mut app = ChatApp::new();
        app.sync_runs(vec![run.clone()]);
        app.append_system_item(TimelineItem::ToolInvocation {
            call_id: "call-1".to_string(),
            tool_id: "delegate_to_agent".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({"agent_id": "planner"}),
            output: serde_json::json!({"run_id": run.run_id}),
            expanded: false,
        });
        app.focus = FocusPane::Main;
        app.active_thread = 0;
        app.sidebar_selected = 0;

        let mut clipboard = MockClipboard { value: None };
        let action = app.handle_event(Event::Key(key(KeyCode::Enter)), &mut clipboard);
        assert_eq!(action, AppAction::LoadRun("run-123".to_string()));
        assert_eq!(app.active_thread, 1);
        assert_eq!(app.sidebar_selected, 1);
    }

    #[test]
    fn sync_runs_keeps_completed_and_failed_threads_visible() {
        let completed = DelegatedRun {
            run_id: "run-1".to_string(),
            agent_id: "planner".to_string(),
            state: "completed".to_string(),
            summary: None,
        };
        let failed = DelegatedRun {
            run_id: "run-2".to_string(),
            agent_id: "browser".to_string(),
            state: "failed".to_string(),
            summary: None,
        };

        let mut app = ChatApp::new();
        app.sync_runs(vec![completed, failed]);

        assert_eq!(app.threads.len(), 3);
        assert!(
            app.threads
                .iter()
                .any(|thread| thread.state.as_deref() == Some("completed")),
            "completed thread should be visible"
        );
        assert!(
            app.threads
                .iter()
                .any(|thread| thread.state.as_deref() == Some("failed")),
            "failed thread should be visible"
        );
    }

    #[test]
    fn load_system_context_maps_messages_into_timeline_items() {
        let mut app = ChatApp::new();
        app.load_system_context(vec![
            OrchestratorThreadMessage::Text {
                role: "user".to_string(),
                content: "hello".to_string(),
            },
            OrchestratorThreadMessage::ToolInvocation {
                call_id: "call-1".to_string(),
                tool_id: "read_config".to_string(),
                arguments: serde_json::json!({}),
                status: "success".to_string(),
                output: serde_json::json!({"ok": true}),
            },
        ]);

        let system_thread = app
            .threads
            .iter()
            .find(|thread| thread.id == SYSTEM_THREAD_ID)
            .expect("system thread should exist");
        assert_eq!(system_thread.items.len(), 2);
        assert!(matches!(
            &system_thread.items[0],
            TimelineItem::Text { role, .. } if *role == MessageRoleTag::User
        ));
        assert!(matches!(
            &system_thread.items[1],
            TimelineItem::ToolInvocation { tool_id, .. } if tool_id == "read_config"
        ));
    }

    #[test]
    fn long_text_messages_start_collapsed_and_toggle() {
        let long_text = "line1\nline2\nline3\nline4\nline5";
        let mut item = TimelineItem::text(MessageRoleTag::Assistant, long_text);

        assert!(item.is_toggleable());
        assert!(matches!(
            &item,
            TimelineItem::Text { expanded, .. } if !expanded
        ));

        item.toggle();
        assert!(matches!(
            &item,
            TimelineItem::Text { expanded, .. } if *expanded
        ));
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
