use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use neuromancer_core::rpc::ThreadSummary;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Padding, Paragraph, Wrap};

use crate::CliError;
use crate::rpc_client::RpcClient;

use super::clipboard::ClipboardProvider;
use super::composer::{ComposerState, DEFAULT_MAX_VISIBLE_LINES};
use super::events::{fetch_all_thread_events, timeline_items_from_events};
use super::timeline::{MessageRoleTag, TimelineItem, short_thread_id, status_style};
use super::{PendingOutcome, SYSTEM_THREAD_ID};

const ESC_NAVIGATION_WINDOW: Duration = Duration::from_millis(850);
const AUTO_COLLAPSE_SIDEBAR_WIDTH: u16 = 92;
const MIN_RIGHT_COLUMN_WIDTH: u16 = 58;
const PANE_GUTTER_WIDTH: u16 = 1;
const INPUT_FOCUS_COLOR: Color = Color::Rgb(120, 210, 176);
const INPUT_BACKGROUND: Color = Color::Rgb(58, 58, 68);
const TIMELINE_BACKGROUND: Color = Color::Rgb(16, 16, 22);
const SIDEBAR_BACKGROUND: Color = Color::Rgb(40, 44, 58);

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractionMode {
    Compose,
    Navigate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AppAction {
    None,
    Quit,
    SubmitSystemTurn(String),
    SubmitSubagentTurn { thread_id: String, message: String },
    OpenThread { thread_id: String, resurrect: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ThreadKind {
    System,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainFilter {
    All,
    DelegatesOnly,
    FailuresOnly,
    SystemToolsOnly,
}

impl MainFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::DelegatesOnly => "delegates",
            Self::FailuresOnly => "failures",
            Self::SystemToolsOnly => "system-tools",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ThreadView {
    pub(super) id: String,
    pub(super) title: String,
    pub(super) kind: ThreadKind,
    pub(super) agent_id: Option<String>,
    pub(super) state: String,
    pub(super) active: bool,
    pub(super) resurrected: bool,
    pub(super) read_only: bool,
    pub(super) items: Vec<TimelineItem>,
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
pub(super) struct ChatApp {
    threads: Vec<ThreadView>,
    sidebar_selected: usize,
    active_thread: usize,
    focus: FocusPane,
    main_filter: MainFilter,
    selected_item_by_thread: HashMap<String, usize>,
    vertical_scroll_by_thread: HashMap<String, usize>,
    horizontal_scroll_by_thread: HashMap<String, usize>,
    composer: ComposerState,
    pub(super) status: Option<String>,
    last_input_cursor_rel: Option<(u16, u16)>,
    last_main_viewport_rows: usize,
    sidebar_collapsed: bool,
    sidebar_auto_collapsed: bool,
    mode: InteractionMode,
    esc_armed_at: Option<Instant>,
    pending_thread_id: Option<String>,
    pending_placeholder_index_by_thread: HashMap<String, usize>,
}

impl ChatApp {
    pub(super) fn new() -> Self {
        Self {
            threads: vec![ThreadView::system()],
            sidebar_selected: 0,
            active_thread: 0,
            focus: FocusPane::Input,
            main_filter: MainFilter::All,
            selected_item_by_thread: HashMap::new(),
            vertical_scroll_by_thread: HashMap::new(),
            horizontal_scroll_by_thread: HashMap::new(),
            composer: ComposerState::new(),
            status: Some("System0 thread ready".to_string()),
            last_input_cursor_rel: None,
            last_main_viewport_rows: 1,
            sidebar_collapsed: false,
            sidebar_auto_collapsed: false,
            mode: InteractionMode::Compose,
            esc_armed_at: None,
            pending_thread_id: None,
            pending_placeholder_index_by_thread: HashMap::new(),
        }
    }

    pub(super) async fn bootstrap(&mut self, rpc: &RpcClient) -> Result<(), CliError> {
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

    pub(super) fn bootstrap_demo(&mut self) {
        let (threads, active) = super::demo::build_demo_threads();
        self.threads = threads;
        self.active_thread = active;
        self.sidebar_selected = active;
        let thread_id = self.threads[active].id.clone();
        self.selected_item_by_thread.insert(
            thread_id,
            self.threads[active].items.len().saturating_sub(1),
        );
        self.status = Some("DEMO MODE".to_string());
    }

    pub(super) async fn handle_pending_outcome(
        &mut self,
        rpc: &RpcClient,
        outcome: PendingOutcome,
    ) {
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

    pub(super) async fn refresh_thread_summaries(
        &mut self,
        rpc: &RpcClient,
    ) -> Result<(), CliError> {
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

    pub(super) async fn load_thread(
        &mut self,
        rpc: &RpcClient,
        thread_id: &str,
    ) -> Result<(), CliError> {
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

    pub(super) fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.sidebar_auto_collapsed = area.width < AUTO_COLLAPSE_SIDEBAR_WIDTH;
        if self.focus == FocusPane::Sidebar && !self.sidebar_visible() {
            self.focus = FocusPane::Main;
        }

        self.composer.max_visible_lines = if area.height < 22 {
            4
        } else if area.height < 34 {
            6
        } else {
            DEFAULT_MAX_VISIBLE_LINES
        };
        self.composer.ensure_cursor_visible();

        let max_input_height = area.height.saturating_sub(8).max(3);
        let input_box_height = (self.composer.input_height() as u16)
            .saturating_add(2)
            .clamp(3, max_input_height);

        let right_column = if self.sidebar_visible() {
            let max_sidebar = area.width.saturating_sub(MIN_RIGHT_COLUMN_WIDTH);
            let sidebar_width = (((area.width as f32) * 0.28) as u16)
                .clamp(20, 38)
                .min(max_sidebar);
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(sidebar_width),
                    Constraint::Min(MIN_RIGHT_COLUMN_WIDTH),
                ])
                .split(area);
            self.render_sidebar(frame, columns[0]);
            columns[1]
        } else {
            area
        };

        let right_content = if right_column.width > 2 {
            right_column.inner(Margin {
                horizontal: 1,
                vertical: 0,
            })
        } else {
            right_column
        };

        let right_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(input_box_height),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(right_content);

        self.render_thread(frame, right_chunks[0]);
        self.render_input(frame, right_chunks[1]);
        frame.render_widget(Paragraph::new(" "), right_chunks[2]);
        self.render_status(frame, right_chunks[3]);
    }

    fn render_sidebar(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let focused = self.focus == FocusPane::Sidebar;
        let primary_text = if focused { Color::White } else { Color::Gray };
        let secondary_text = if focused {
            Color::DarkGray
        } else {
            Color::Gray
        };
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(1)])
            .split(area);
        frame.render_widget(
            Paragraph::new("threads").style(
                Style::default()
                    .fg(secondary_text)
                    .bg(SIDEBAR_BACKGROUND)
                    .add_modifier(Modifier::BOLD),
            ),
            split[0],
        );
        let items: Vec<ListItem<'static>> = self
            .threads
            .iter()
            .map(|thread| {
                if thread.kind == ThreadKind::System {
                    ListItem::new(Line::from(vec![Span::styled(
                        "system0",
                        Style::default().fg(primary_text),
                    )]))
                } else {
                    ListItem::new(Line::from(vec![
                        Span::styled(thread.title.clone(), Style::default().fg(primary_text)),
                        Span::raw(" "),
                        Span::styled(
                            thread.state.clone(),
                            if focused {
                                status_style(&thread.state)
                            } else {
                                Style::default().fg(Color::Gray)
                            },
                        ),
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
            .bg(Color::Rgb(54, 58, 76))
            .add_modifier(Modifier::BOLD);
        if !focused {
            highlight = highlight.remove_modifier(Modifier::BOLD);
        }

        let list = List::new(items)
            .style(Style::default().bg(SIDEBAR_BACKGROUND))
            .highlight_symbol("» ")
            .highlight_style(highlight)
            .highlight_spacing(ratatui::widgets::HighlightSpacing::Always);

        frame.render_stateful_widget(list, split[1], &mut state);
    }

    fn render_thread(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let thread = self.current_thread().clone();
        let selected = self
            .current_thread_selected_item()
            .min(thread.items.len().saturating_sub(1));
        let horizontal_offset = self
            .horizontal_scroll_by_thread
            .get(&thread.id)
            .copied()
            .unwrap_or(0);
        let visible_width = area.width as usize;
        let selected_fill_width = horizontal_offset.saturating_add(visible_width.max(1));
        let timeline_focused =
            self.mode == InteractionMode::Navigate || self.focus == FocusPane::Main;
        let (lines, ranges, selected_visible_idx) = build_thread_lines(
            &thread,
            selected,
            selected_fill_width,
            self.main_filter,
            timeline_focused,
        );
        let viewport_rows = area.height as usize;
        self.last_main_viewport_rows = viewport_rows.max(1);

        self.ensure_selected_visible_for(
            &thread.id,
            &ranges,
            viewport_rows.max(1),
            selected_visible_idx,
        );

        let max_vertical = lines.len().saturating_sub(viewport_rows.max(1));
        let mut vertical_offset = self
            .vertical_scroll_by_thread
            .get(&thread.id)
            .copied()
            .unwrap_or(0)
            .min(max_vertical);
        self.vertical_scroll_by_thread
            .insert(thread.id.clone(), vertical_offset);

        let paragraph = Paragraph::new(Text::from(lines))
            .style(Style::default().bg(TIMELINE_BACKGROUND))
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
        if area.width <= PANE_GUTTER_WIDTH {
            return;
        }
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(PANE_GUTTER_WIDTH), Constraint::Min(1)])
            .split(area);
        let gutter_area = split[0];
        let body_area = split[1];

        let read_only = self.current_thread().read_only;
        let input_focused =
            self.mode != InteractionMode::Navigate && self.focus == FocusPane::Input;
        self.render_focus_gutter(frame, gutter_area, input_focused);

        let block = Block::default()
            .padding(Padding::new(1, 1, 1, 1))
            .style(Style::default().bg(INPUT_BACKGROUND));
        let inner = block.inner(body_area);

        let lines = self.composer.prefixed_lines();
        let paragraph = Paragraph::new(Text::from(lines))
            .block(block)
            .style(Style::default().bg(INPUT_BACKGROUND))
            .wrap(Wrap { trim: false })
            .scroll((self.composer.scroll_line_offset as u16, 0));

        frame.render_widget(paragraph, body_area);
        self.last_input_cursor_rel = None;

        if self.mode == InteractionMode::Navigate || self.focus != FocusPane::Input || read_only {
            return;
        }

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
        let mode_label = match self.mode {
            InteractionMode::Compose => "input",
            InteractionMode::Navigate => "navigation",
        };
        let sidebar_label = if self.sidebar_visible() {
            "open"
        } else {
            "hidden"
        };
        let status = self.status.clone().unwrap_or_else(|| {
            format!(
                "Mode:{mode_label} Sidebar:{sidebar_label} Filter:{} | Esc Esc: navigation mode | i: input mode | Ctrl+B: toggle sidebar | Tab/Shift+Tab: focus | Enter: send/open | Space: expand | Left/Right: h-scroll | 1-4: filter | q: quit",
                self.main_filter.label()
            )
        });
        let text = Text::from(Line::from(vec![Span::styled(
            status,
            Style::default().fg(Color::DarkGray),
        )]));
        frame.render_widget(Paragraph::new(text), area);
    }

    pub(super) fn handle_event(
        &mut self,
        event: Event,
        clipboard: &mut dyn ClipboardProvider,
    ) -> AppAction {
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

        self.expire_esc_arm();
        let read_only_input = self.current_thread().read_only;

        if key.code == KeyCode::Char('q') && key.modifiers.is_empty() {
            return AppAction::Quit;
        }

        if key.code == KeyCode::Esc {
            return self.handle_escape_press();
        }
        self.esc_armed_at = None;

        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('b') | KeyCode::Char('B'))
        {
            self.toggle_sidebar();
            return AppAction::None;
        }

        if self.mode == InteractionMode::Navigate {
            return self.handle_navigation_key(key);
        }

        if key.code == KeyCode::BackTab
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT))
        {
            self.cycle_focus_backward();
            return AppAction::None;
        }

        if key.code == KeyCode::Tab {
            self.cycle_focus_forward();
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
            KeyCode::Char('1') if self.focus == FocusPane::Main && key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::All);
                AppAction::None
            }
            KeyCode::Char('2') if self.focus == FocusPane::Main && key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::DelegatesOnly);
                AppAction::None
            }
            KeyCode::Char('3') if self.focus == FocusPane::Main && key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::FailuresOnly);
                AppAction::None
            }
            KeyCode::Char('4') if self.focus == FocusPane::Main && key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::SystemToolsOnly);
                AppAction::None
            }
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

    fn sidebar_visible(&self) -> bool {
        !self.sidebar_collapsed && !self.sidebar_auto_collapsed
    }

    fn render_focus_gutter(&self, frame: &mut Frame<'_>, area: Rect, active: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let lines = (0..area.height)
            .map(|_| {
                Line::from(Span::styled(
                    if active { "┃" } else { " " },
                    Style::default().fg(INPUT_FOCUS_COLOR).bg(INPUT_BACKGROUND),
                ))
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(Text::from(lines)).style(Style::default().bg(INPUT_BACKGROUND)),
            area,
        );
    }

    fn cycle_focus_forward(&mut self) {
        let mut next = self.focus.next();
        if next == FocusPane::Sidebar && !self.sidebar_visible() {
            next = next.next();
        }
        self.focus = next;
    }

    fn cycle_focus_backward(&mut self) {
        let mut prev = self.focus.prev();
        if prev == FocusPane::Sidebar && !self.sidebar_visible() {
            prev = prev.prev();
        }
        self.focus = prev;
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        if !self.sidebar_visible() && self.focus == FocusPane::Sidebar {
            self.focus = FocusPane::Main;
        }
        self.status = Some(if self.sidebar_visible() {
            "Thread sidebar shown".to_string()
        } else {
            "Thread sidebar hidden".to_string()
        });
    }

    fn expire_esc_arm(&mut self) {
        if let Some(armed_at) = self.esc_armed_at
            && armed_at.elapsed() > ESC_NAVIGATION_WINDOW
        {
            self.esc_armed_at = None;
        }
    }

    fn handle_escape_press(&mut self) -> AppAction {
        if self.mode == InteractionMode::Navigate {
            self.mode = InteractionMode::Compose;
            self.focus = FocusPane::Input;
            self.esc_armed_at = None;
            self.status = Some("Input mode".to_string());
            return AppAction::None;
        }

        if self
            .esc_armed_at
            .is_some_and(|armed| armed.elapsed() <= ESC_NAVIGATION_WINDOW)
        {
            self.mode = InteractionMode::Navigate;
            self.focus = FocusPane::Main;
            self.esc_armed_at = None;
            self.status = Some(
                "Navigation mode: Up/Down to move, Enter to open, Space to expand, i to edit"
                    .to_string(),
            );
            return AppAction::None;
        }

        self.esc_armed_at = Some(Instant::now());
        self.status = Some("Press Esc again quickly to enter navigation mode".to_string());
        AppAction::None
    }

    fn handle_navigation_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Char('i') | KeyCode::Char('I') => {
                self.mode = InteractionMode::Compose;
                self.focus = FocusPane::Input;
                self.status = Some("Input mode".to_string());
                AppAction::None
            }
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() => {
                self.select_main_prev();
                AppAction::None
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() => {
                self.select_main_next();
                AppAction::None
            }
            KeyCode::Left => {
                let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    8
                } else {
                    1
                };
                let current = self.current_thread_horizontal_offset();
                self.set_current_thread_horizontal_offset(current.saturating_sub(step));
                AppAction::None
            }
            KeyCode::Right => {
                let step = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    8
                } else {
                    1
                };
                let current = self.current_thread_horizontal_offset();
                self.set_current_thread_horizontal_offset(current.saturating_add(step));
                AppAction::None
            }
            KeyCode::Home => {
                self.set_current_thread_horizontal_offset(0);
                AppAction::None
            }
            KeyCode::Char(' ') => {
                self.toggle_selected_item();
                AppAction::None
            }
            KeyCode::Enter => self.open_selected_main_link(),
            KeyCode::Char('1') if key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::All);
                AppAction::None
            }
            KeyCode::Char('2') if key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::DelegatesOnly);
                AppAction::None
            }
            KeyCode::Char('3') if key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::FailuresOnly);
                AppAction::None
            }
            KeyCode::Char('4') if key.modifiers.is_empty() => {
                self.set_main_filter(MainFilter::SystemToolsOnly);
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
        let visible = self.current_visible_item_indices();
        if visible.is_empty() {
            return;
        }
        let selected = self.current_thread_selected_item();
        let current_pos = selected_visible_position(&visible, selected);
        let new_pos = current_pos.saturating_sub(1);
        self.set_current_thread_selected_item(visible[new_pos]);
        self.ensure_selected_visible_current();
    }

    fn select_main_next(&mut self) {
        let visible = self.current_visible_item_indices();
        if visible.is_empty() {
            return;
        }
        let selected = self.current_thread_selected_item();
        let current_pos = selected_visible_position(&visible, selected);
        let new_pos = (current_pos + 1).min(visible.len().saturating_sub(1));
        self.set_current_thread_selected_item(visible[new_pos]);
        self.ensure_selected_visible_current();
    }

    fn toggle_selected_item(&mut self) {
        self.align_selection_to_filter_current();
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
        self.focus = FocusPane::Main;

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
        self.align_selection_to_filter_current();
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
        self.focus = FocusPane::Main;
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

    fn set_main_filter(&mut self, filter: MainFilter) {
        self.main_filter = filter;
        self.align_selection_to_filter_current();
        self.ensure_selected_visible_current();
        self.status = Some(format!("Main filter: {}", self.main_filter.label()));
    }

    fn current_visible_item_indices(&self) -> Vec<usize> {
        self.current_thread()
            .items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| {
                if item_matches_filter(item, self.main_filter) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect()
    }

    fn align_selection_to_filter_current(&mut self) {
        let visible = self.current_visible_item_indices();
        if visible.is_empty() {
            return;
        }

        let selected = self.current_thread_selected_item();
        if visible.iter().any(|idx| *idx == selected) {
            return;
        }

        let fallback = visible
            .iter()
            .rfind(|idx| **idx < selected)
            .copied()
            .unwrap_or_else(|| visible[0]);
        self.set_current_thread_selected_item(fallback);
    }

    fn ensure_selected_visible_current(&mut self) {
        let thread = self.current_thread().clone();
        let selected = self
            .current_thread_selected_item()
            .min(thread.items.len().saturating_sub(1));
        let (_, ranges, selected_visible_idx) =
            build_thread_lines(&thread, selected, 1, self.main_filter, false);
        self.ensure_selected_visible_for(
            &thread.id,
            &ranges,
            self.last_main_viewport_rows.max(1),
            selected_visible_idx,
        );
    }

    fn ensure_selected_visible_for(
        &mut self,
        thread_id: &str,
        ranges: &[(usize, usize)],
        viewport_rows: usize,
        selected_idx: usize,
    ) {
        if ranges.is_empty() {
            self.vertical_scroll_by_thread
                .insert(thread_id.to_string(), 0);
            return;
        }

        let selected = selected_idx.min(ranges.len().saturating_sub(1));
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

    pub(super) fn system_thread_snapshot(&self) -> serde_json::Value {
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
    selected_fill_width: usize,
    filter: MainFilter,
    timeline_focused: bool,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>, usize) {
    if thread.items.is_empty() {
        return (
            vec![Line::from(Span::styled(
                "No messages yet",
                Style::default().fg(Color::DarkGray),
            ))],
            vec![],
            0,
        );
    }

    let visible_indices = thread
        .items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            if item_matches_filter(item, filter) {
                Some(idx)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    if visible_indices.is_empty() {
        return (
            vec![Line::from(Span::styled(
                "No messages for current filter",
                Style::default().fg(Color::DarkGray),
            ))],
            vec![],
            0,
        );
    }

    let selected_visible_idx = selected_visible_position(&visible_indices, selected);
    let mut lines = Vec::new();
    let mut ranges = Vec::new();
    let assistant_label = thread.assistant_badge_label();
    for (visible_pos, idx) in visible_indices.iter().enumerate() {
        let item = &thread.items[*idx];
        let start = lines.len();
        let item_lines = item.lines(
            visible_pos == selected_visible_idx,
            &assistant_label,
            Some(selected_fill_width),
            timeline_focused,
        );
        lines.extend(item_lines);
        let end = lines.len().saturating_sub(1);
        ranges.push((start, end));
        if visible_pos + 1 < visible_indices.len() {
            lines.push(Line::from(Span::styled(
                " ",
                Style::default().bg(TIMELINE_BACKGROUND),
            )));
        }
    }

    (lines, ranges, selected_visible_idx)
}

fn selected_visible_position(visible: &[usize], selected: usize) -> usize {
    if visible.is_empty() {
        return 0;
    }

    if let Some(pos) = visible.iter().position(|idx| *idx == selected) {
        return pos;
    }

    visible.iter().rposition(|idx| *idx < selected).unwrap_or(0)
}

fn item_matches_filter(item: &TimelineItem, filter: MainFilter) -> bool {
    match filter {
        MainFilter::All => true,
        MainFilter::DelegatesOnly => match item {
            TimelineItem::DelegateInvocation { .. } => true,
            TimelineItem::ToolInvocation { tool_id, .. } => tool_id == "delegate_to_agent",
            _ => false,
        },
        MainFilter::FailuresOnly => match item {
            TimelineItem::ToolInvocation { status, .. } => status == "error" || status == "failed",
            TimelineItem::DelegateInvocation { status, error, .. } => {
                status == "error" || status == "failed" || error.is_some()
            }
            TimelineItem::Text { role, text, .. } => {
                *role == MessageRoleTag::System && text.to_ascii_lowercase().contains("error")
            }
        },
        MainFilter::SystemToolsOnly => matches!(
            item,
            TimelineItem::DelegateInvocation { .. } | TimelineItem::ToolInvocation { .. }
        ),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventState, KeyModifiers};

    use super::*;

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
    fn double_escape_enters_navigation_mode() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Input;
        let mut clipboard = MockClipboard { value: None };
        let _ = app.handle_event(Event::Key(key(KeyCode::Esc)), &mut clipboard);
        assert_eq!(app.mode, InteractionMode::Compose);
        let _ = app.handle_event(Event::Key(key(KeyCode::Esc)), &mut clipboard);
        assert_eq!(app.mode, InteractionMode::Navigate);
        assert_eq!(app.focus, FocusPane::Main);
    }

    #[test]
    fn navigation_mode_scrolls_and_exits_with_i() {
        let mut app = ChatApp::new();
        if let Some(system) = app
            .threads
            .iter_mut()
            .find(|thread| thread.id == SYSTEM_THREAD_ID)
        {
            system
                .items
                .push(TimelineItem::text(MessageRoleTag::User, "first"));
            system
                .items
                .push(TimelineItem::text(MessageRoleTag::Assistant, "second"));
        }
        app.selected_item_by_thread
            .insert(SYSTEM_THREAD_ID.to_string(), 0);
        app.mode = InteractionMode::Navigate;
        app.focus = FocusPane::Main;

        let mut clipboard = MockClipboard { value: None };
        let _ = app.handle_event(Event::Key(key(KeyCode::Down)), &mut clipboard);
        assert_eq!(app.current_thread_selected_item(), 1);

        let _ = app.handle_event(Event::Key(key(KeyCode::Char('i'))), &mut clipboard);
        assert_eq!(app.mode, InteractionMode::Compose);
        assert_eq!(app.focus, FocusPane::Input);
    }

    #[test]
    fn ctrl_b_collapses_sidebar_and_skips_sidebar_focus() {
        let mut app = ChatApp::new();
        app.focus = FocusPane::Sidebar;
        let mut clipboard = MockClipboard { value: None };
        let _ = app.handle_event(
            Event::Key(key_with_modifiers(
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
            )),
            &mut clipboard,
        );
        assert!(app.sidebar_collapsed);
        assert_eq!(app.focus, FocusPane::Main);

        app.focus = FocusPane::Input;
        let _ = app.handle_event(Event::Key(key(KeyCode::Tab)), &mut clipboard);
        assert_eq!(app.focus, FocusPane::Main);
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
                meta: None,
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
    fn assistant_badge_uses_system_and_agent_names() {
        let mut system = ThreadView::system();
        system.items.push(TimelineItem::text(
            MessageRoleTag::Assistant,
            "system reply",
        ));
        let (system_lines, _, _) = build_thread_lines(&system, 0, 1, MainFilter::All, true);
        assert!(
            line_text(&system_lines[1]).contains("system0"),
            "system assistant label should be System0"
        );

        let mut subagent = ThreadView::from_summary(&subagent_summary("thread-2", true, false));
        subagent
            .items
            .push(TimelineItem::text(MessageRoleTag::Assistant, "agent reply"));
        let (subagent_lines, _, _) = build_thread_lines(&subagent, 0, 1, MainFilter::All, true);
        assert!(
            line_text(&subagent_lines[1]).contains("planner"),
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
}
