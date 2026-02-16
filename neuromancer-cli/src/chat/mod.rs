mod app;
mod clipboard;
mod composer;
mod demo;
mod events;
mod terminal;
mod timeline;

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::{Duration, Instant};

use crossterm::event::{self, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use neuromancer_core::rpc::{
    OrchestratorSubagentTurnParams, OrchestratorSubagentTurnResult,
    OrchestratorThreadResurrectParams, OrchestratorTurnParams, OrchestratorTurnResult,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::CliError;
use crate::rpc_client::RpcClient;

use self::app::{AppAction, ChatApp};
use self::clipboard::CommandClipboard;
use self::terminal::TerminalCleanup;

pub(super) const SYSTEM_THREAD_ID: &str = "system0";
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PENDING_POLL_INTERVAL: Duration = Duration::from_millis(50);
const ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(33);

fn poll_timeout_with_animation(
    fallback: Duration,
    animation_active: bool,
    next_animation_tick: Option<Instant>,
) -> Duration {
    if !animation_active {
        return fallback;
    }

    let Some(next_tick) = next_animation_tick else {
        return fallback;
    };
    let now = Instant::now();
    let until_tick = next_tick.saturating_duration_since(now);
    std::cmp::min(fallback, until_tick)
}

fn animation_tick_due(animation_active: bool, next_animation_tick: Option<Instant>) -> bool {
    animation_active
        && next_animation_tick
            .map(|next_tick| next_tick <= Instant::now())
            .unwrap_or(false)
}

pub async fn run_demo_chat() -> Result<(), CliError> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(CliError::Usage("demo mode requires a TTY".to_string()));
    }

    enable_raw_mode().map_err(map_terminal_err)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste).map_err(map_terminal_err)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(map_terminal_err)?;
    let mut cleanup = TerminalCleanup { enabled: true };

    let mut app = ChatApp::new();
    app.bootstrap_demo();
    let mut clipboard = CommandClipboard;
    let mut needs_redraw = true;
    let mut animation_active = false;
    let mut next_animation_tick: Option<Instant> = None;

    loop {
        let should_animate = app.animation_active();
        if should_animate != animation_active {
            animation_active = should_animate;
            next_animation_tick =
                animation_active.then(|| Instant::now() + ANIMATION_FRAME_INTERVAL);
            needs_redraw = true;
        }

        if needs_redraw {
            terminal
                .draw(|frame| app.render(frame))
                .map_err(map_terminal_err)?;
            needs_redraw = false;
            if animation_active {
                next_animation_tick = Some(Instant::now() + ANIMATION_FRAME_INTERVAL);
            }
        }

        let poll_timeout =
            poll_timeout_with_animation(IDLE_POLL_INTERVAL, animation_active, next_animation_tick);
        if !event::poll(poll_timeout).map_err(map_terminal_err)? {
            if animation_tick_due(animation_active, next_animation_tick) {
                needs_redraw = true;
            }
            continue;
        }

        let next = event::read().map_err(map_terminal_err)?;
        needs_redraw = true;
        match app.handle_event(next, &mut clipboard) {
            AppAction::None => {}
            AppAction::Quit => break,
            AppAction::SubmitSystemTurn(_) | AppAction::SubmitSubagentTurn { .. } => {
                app.status = Some("Demo mode: sends disabled".to_string());
            }
            AppAction::OpenThread { thread_id, .. } => {
                app.status = Some(format!(
                    "Loaded thread {} (demo)",
                    timeline::short_thread_id(&thread_id)
                ));
            }
        }
    }

    terminal.show_cursor().map_err(map_terminal_err)?;
    drop(terminal);
    cleanup.disable();
    Ok(())
}

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
pub(super) enum PendingOutcome {
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
    let mut needs_redraw = true;
    let mut animation_active = false;
    let mut next_animation_tick: Option<Instant> = None;

    loop {
        if pending.as_ref().is_some_and(|handle| handle.is_finished()) {
            let finished = pending.take().expect("pending task must exist");
            let outcome = match finished.await {
                Ok(outcome) => outcome,
                Err(err) => PendingOutcome::WorkerFailed(err.to_string()),
            };
            app.handle_pending_outcome(rpc, outcome).await;
            needs_redraw = true;
        }

        let should_animate = app.animation_active();
        if should_animate != animation_active {
            animation_active = should_animate;
            next_animation_tick =
                animation_active.then(|| Instant::now() + ANIMATION_FRAME_INTERVAL);
            needs_redraw = true;
        }

        if needs_redraw {
            terminal
                .draw(|frame| app.render(frame))
                .map_err(map_terminal_err)?;
            needs_redraw = false;
            if animation_active {
                next_animation_tick = Some(Instant::now() + ANIMATION_FRAME_INTERVAL);
            }
        }

        let fallback_timeout = if pending.is_some() {
            PENDING_POLL_INTERVAL
        } else {
            IDLE_POLL_INTERVAL
        };
        let poll_timeout =
            poll_timeout_with_animation(fallback_timeout, animation_active, next_animation_tick);
        if !event::poll(poll_timeout).map_err(map_terminal_err)? {
            if animation_tick_due(animation_active, next_animation_tick) {
                needs_redraw = true;
            }
            continue;
        }

        let next = event::read().map_err(map_terminal_err)?;
        needs_redraw = true;
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
                            rpc.orchestrator_thread_resurrect(OrchestratorThreadResurrectParams {
                                thread_id: thread_id.clone(),
                            })
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{animation_tick_due, parse_ndjson_message_line, poll_timeout_with_animation};

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

    #[test]
    fn animation_poll_timeout_uses_fallback_when_inactive() {
        let timeout = poll_timeout_with_animation(
            Duration::from_millis(250),
            false,
            Some(Instant::now() + Duration::from_millis(10)),
        );
        assert_eq!(timeout, Duration::from_millis(250));
    }

    #[test]
    fn animation_poll_timeout_clamps_to_next_tick_when_active() {
        let timeout = poll_timeout_with_animation(
            Duration::from_millis(250),
            true,
            Some(Instant::now() + Duration::from_millis(5)),
        );
        assert!(timeout <= Duration::from_millis(5));
    }

    #[test]
    fn animation_tick_due_only_when_active_and_elapsed() {
        let elapsed_tick = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("instant arithmetic should not underflow");
        assert!(animation_tick_due(true, Some(elapsed_tick)));
        assert!(!animation_tick_due(false, Some(elapsed_tick)));
        assert!(!animation_tick_due(true, None));
    }
}
