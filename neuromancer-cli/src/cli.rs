use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "neuroctl",
    version,
    about = "Neuromancer control CLI over JSON-RPC"
)]
pub struct Cli {
    /// Daemon admin API base URL.
    #[arg(long, default_value = "http://127.0.0.1:9090")]
    pub addr: String,

    /// Emit stable JSON envelopes.
    #[arg(long)]
    pub json: bool,

    /// Request timeout for RPC calls.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub timeout: Duration,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Health,
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Rpc {
        #[command(subcommand)]
        command: RpcCommand,
    },
    E2e {
        #[command(subcommand)]
        command: E2eCommand,
    },
    Orchestrator {
        #[command(subcommand)]
        command: OrchestratorCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    Start(DaemonStartArgs),
    Stop(DaemonStopArgs),
    Status(DaemonStatusArgs),
}

#[derive(Debug, Args)]
pub struct DaemonStartArgs {
    #[arg(long)]
    pub config: PathBuf,

    #[arg(long)]
    pub daemon_bin: Option<PathBuf>,

    #[arg(long, default_value = "/tmp/neuromancer.pid")]
    pub pid_file: PathBuf,

    #[arg(long)]
    pub wait_healthy: bool,
}

#[derive(Debug, Args)]
pub struct DaemonStopArgs {
    #[arg(long, default_value = "/tmp/neuromancer.pid")]
    pub pid_file: PathBuf,

    #[arg(long, default_value = "15s", value_parser = parse_duration)]
    pub grace: Duration,
}

#[derive(Debug, Args)]
pub struct DaemonStatusArgs {
    #[arg(long, default_value = "/tmp/neuromancer.pid")]
    pub pid_file: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Reload,
}

#[derive(Debug, Subcommand)]
pub enum RpcCommand {
    Call(RpcCallArgs),
}

#[derive(Debug, Args)]
pub struct RpcCallArgs {
    #[arg(long)]
    pub method: String,

    #[arg(long)]
    pub params: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum E2eCommand {
    Smoke(E2eSmokeArgs),
}

#[derive(Debug, Args)]
pub struct E2eSmokeArgs {
    #[arg(long)]
    pub config: PathBuf,

    #[arg(long)]
    pub daemon_bin: Option<PathBuf>,

    #[arg(long, default_value = "/tmp/neuromancer.pid")]
    pub pid_file: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorCommand {
    Turn(OrchestratorTurnArgs),
    Runs {
        #[command(subcommand)]
        command: OrchestratorRunsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorRunsCommand {
    List,
    Get(OrchestratorRunGetArgs),
}

#[derive(Debug, Args)]
pub struct OrchestratorTurnArgs {
    /// Execute one System0 orchestrator turn.
    pub message: String,
}

#[derive(Debug, Args)]
pub struct OrchestratorRunGetArgs {
    /// Delegated run id returned by `orchestrator turn`.
    pub run_id: String,
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    humantime::parse_duration(input).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_daemon_start_wait_healthy() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "daemon",
            "start",
            "--config",
            "/tmp/config.toml",
            "--wait-healthy",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Daemon {
                command: DaemonCommand::Start(args),
            } => {
                assert!(args.wait_healthy);
                assert_eq!(args.pid_file, PathBuf::from("/tmp/neuromancer.pid"));
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_timeout_duration() {
        let cli =
            Cli::try_parse_from(["neuroctl", "--timeout", "3s", "health"]).expect("cli parse");
        assert_eq!(cli.timeout, Duration::from_secs(3));
    }

    #[test]
    fn parses_orchestrator_turn_command() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "orchestrator",
            "turn",
            "what should I pay first?",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Turn(args),
            } => {
                assert_eq!(args.message, "what should I pay first?");
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_orchestrator_runs_get_command() {
        let cli = Cli::try_parse_from(["neuroctl", "orchestrator", "runs", "get", "run-123"])
            .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Runs { command },
            } => match command {
                OrchestratorRunsCommand::Get(args) => assert_eq!(args.run_id, "run-123"),
                other => panic!("unexpected runs command parsed: {other:?}"),
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_orchestrator_runs_list_command() {
        let cli = Cli::try_parse_from(["neuroctl", "orchestrator", "runs", "list"])
            .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Runs { command },
            } => match command {
                OrchestratorRunsCommand::List => {}
                other => panic!("unexpected runs command parsed: {other:?}"),
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }
}
