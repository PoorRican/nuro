mod cli;
mod daemon;
mod e2e;
mod install;
mod output;
mod rpc_client;

use clap::Parser;

use cli::{
    Cli, Command, ConfigCommand, DaemonCommand, E2eCommand, OrchestratorCommand,
    OrchestratorRunsCommand, RpcCommand,
};
use daemon::{DaemonStartOptions, DaemonStopOptions, daemon_status, start_daemon, stop_daemon};
use e2e::{SmokeOptions, run_smoke};
use install::run_install;
use neuromancer_core::rpc::{OrchestratorRunGetParams, OrchestratorTurnParams};
use rpc_client::RpcClient;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Usage(String),

    #[error("{0}")]
    Lifecycle(String),

    #[error("{0}")]
    RpcTransport(String),

    #[error("{0}")]
    Smoke(String),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::Lifecycle(_) => 3,
            Self::RpcTransport(_) => 4,
            Self::Smoke(_) => 5,
        }
    }
}

impl From<rpc_client::RpcClientError> for CliError {
    fn from(value: rpc_client::RpcClientError) -> Self {
        match value {
            rpc_client::RpcClientError::InvalidRequest(message) => Self::Usage(message),
            rpc_client::RpcClientError::Transport(message)
            | rpc_client::RpcClientError::InvalidResponse(message)
            | rpc_client::RpcClientError::Rpc(_, message) => Self::RpcTransport(message),
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json_mode = cli.json;

    let result = run(cli).await;
    match result {
        Ok(payload) => {
            output::print_success(json_mode, &payload);
        }
        Err(err) => {
            let exit_code = err.exit_code();
            output::print_error(json_mode, &err.to_string(), exit_code);
            std::process::exit(exit_code);
        }
    }
}

async fn run(cli: Cli) -> Result<serde_json::Value, CliError> {
    match cli.command {
        Command::Install(args) => {
            let result = run_install(&args.config)?;
            Ok(serde_json::json!(result))
        }
        Command::Daemon { command } => match command {
            DaemonCommand::Start(args) => {
                let result = start_daemon(&DaemonStartOptions {
                    config: args.config,
                    daemon_bin: args.daemon_bin,
                    pid_file: args.pid_file,
                    wait_healthy: args.wait_healthy,
                    addr: cli.addr,
                    timeout: cli.timeout,
                })
                .await?;

                Ok(serde_json::json!(result))
            }
            DaemonCommand::Stop(args) => {
                let result = stop_daemon(&DaemonStopOptions {
                    pid_file: args.pid_file,
                    grace: args.grace,
                })
                .await?;
                Ok(serde_json::json!(result))
            }
            DaemonCommand::Status(args) => {
                let result = daemon_status(&args.pid_file, &cli.addr, cli.timeout).await?;
                Ok(serde_json::json!(result))
            }
        },
        Command::Health => {
            let rpc = RpcClient::new(&cli.addr, cli.timeout)?;
            let health = rpc.health().await?;
            Ok(serde_json::json!(health))
        }
        Command::Config { command } => {
            let rpc = RpcClient::new(&cli.addr, cli.timeout)?;
            match command {
                ConfigCommand::Reload => {
                    let response = rpc.config_reload().await?;
                    Ok(serde_json::json!(response))
                }
            }
        }
        Command::Rpc { command } => {
            let rpc = RpcClient::new(&cli.addr, cli.timeout)?;
            match command {
                RpcCommand::Call(args) => {
                    let params = parse_json_params(args.params)?;
                    let response = rpc.call(&args.method, params).await?;
                    Ok(serde_json::json!({
                        "method": args.method,
                        "result": response,
                    }))
                }
            }
        }
        Command::E2e { command } => match command {
            E2eCommand::Smoke(args) => {
                let result = run_smoke(&SmokeOptions {
                    config: args.config,
                    daemon_bin: args.daemon_bin,
                    pid_file: args.pid_file,
                    addr: cli.addr,
                    timeout: cli.timeout,
                })
                .await?;

                Ok(serde_json::json!(result))
            }
        },
        Command::Orchestrator { command } => {
            let rpc = RpcClient::new(&cli.addr, cli.timeout)?;
            match command {
                OrchestratorCommand::Turn(args) => {
                    let response = rpc
                        .orchestrator_turn(OrchestratorTurnParams {
                            message: args.message,
                        })
                        .await?;
                    Ok(serde_json::json!(response))
                }
                OrchestratorCommand::Runs { command } => match command {
                    OrchestratorRunsCommand::List => {
                        let response = rpc.orchestrator_runs_list().await?;
                        Ok(serde_json::json!(response))
                    }
                    OrchestratorRunsCommand::Get(args) => {
                        let response = rpc
                            .orchestrator_run_get(OrchestratorRunGetParams {
                                run_id: args.run_id,
                            })
                            .await?;
                        Ok(serde_json::json!(response))
                    }
                },
            }
        }
    }
}

fn parse_json_params(params: Option<String>) -> Result<Option<serde_json::Value>, CliError> {
    match params {
        None => Ok(None),
        Some(raw) => {
            let parsed: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|err| CliError::Usage(format!("invalid --params JSON: {err}")))?;
            Ok(Some(parsed))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CliError;

    #[test]
    fn error_code_mapping_is_stable() {
        assert_eq!(CliError::Usage("x".into()).exit_code(), 2);
        assert_eq!(CliError::Lifecycle("x".into()).exit_code(), 3);
        assert_eq!(CliError::RpcTransport("x".into()).exit_code(), 4);
        assert_eq!(CliError::Smoke("x".into()).exit_code(), 5);
    }
}
