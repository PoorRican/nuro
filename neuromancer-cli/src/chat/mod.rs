mod app;
mod clipboard;
mod composer;
mod events;
mod terminal;
mod timeline;

use std::io::{self, BufRead, IsTerminal, Write};
use std::time::Duration;

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
    use super::parse_ndjson_message_line;

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
