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
    Install(InstallArgs),
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
    Restart(DaemonRestartArgs),
    Stop(DaemonStopArgs),
    Status(DaemonStatusArgs),
}

#[derive(Debug, Args)]
pub struct DaemonStartArgs {
    /// Config file path. Defaults to XDG config location when omitted.
    #[arg(long)]
    pub config: Option<PathBuf>,

    #[arg(long)]
    pub daemon_bin: Option<PathBuf>,

    #[arg(long, default_value = "/tmp/neuromancer.pid")]
    pub pid_file: PathBuf,

    #[arg(long)]
    pub wait_healthy: bool,
}

#[derive(Debug, Args)]
pub struct DaemonRestartArgs {
    /// Config file path. Defaults to XDG config location when omitted.
    #[arg(long)]
    pub config: Option<PathBuf>,

    #[arg(long)]
    pub daemon_bin: Option<PathBuf>,

    #[arg(long, default_value = "/tmp/neuromancer.pid")]
    pub pid_file: PathBuf,

    #[arg(long, default_value = "15s", value_parser = parse_duration)]
    pub grace: Duration,

    #[arg(long)]
    pub wait_healthy: bool,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Config file path. Defaults to XDG config location when omitted.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Overwrite the target config file from defaults before prompt bootstrap.
    #[arg(long)]
    pub override_config: bool,
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
    /// Config file path. Defaults to XDG config location when omitted.
    #[arg(long)]
    pub config: Option<PathBuf>,

    #[arg(long)]
    pub daemon_bin: Option<PathBuf>,

    #[arg(long, default_value = "/tmp/neuromancer.pid")]
    pub pid_file: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorCommand {
    Turn(OrchestratorTurnArgs),
    Chat(OrchestratorChatArgs),
    Runs {
        #[command(subcommand)]
        command: OrchestratorRunsCommand,
    },
    Events {
        #[command(subcommand)]
        command: OrchestratorEventsCommand,
    },
    Reports {
        #[command(subcommand)]
        command: OrchestratorReportsCommand,
    },
    Stats {
        #[command(subcommand)]
        command: OrchestratorStatsCommand,
    },
    Threads {
        #[command(subcommand)]
        command: OrchestratorThreadsCommand,
    },
    Subagent {
        #[command(subcommand)]
        command: OrchestratorSubagentCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorRunsCommand {
    List,
    Get(OrchestratorRunGetArgs),
    Diagnose(OrchestratorRunDiagnoseArgs),
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorEventsCommand {
    Query(OrchestratorEventsQueryArgs),
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorReportsCommand {
    Query(OrchestratorReportsQueryArgs),
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorStatsCommand {
    Get,
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorThreadsCommand {
    List,
    Get(OrchestratorThreadGetArgs),
    Resurrect(OrchestratorThreadResurrectArgs),
}

#[derive(Debug, Subcommand)]
pub enum OrchestratorSubagentCommand {
    Turn(OrchestratorSubagentTurnArgs),
}

#[derive(Debug, Args)]
pub struct OrchestratorTurnArgs {
    /// Execute one System0 orchestrator turn.
    pub message: String,
}

#[derive(Debug, Args)]
pub struct OrchestratorChatArgs {}

#[derive(Debug, Args)]
pub struct OrchestratorRunGetArgs {
    /// Delegated run id returned by `orchestrator turn`.
    pub run_id: String,
}

#[derive(Debug, Args)]
pub struct OrchestratorRunDiagnoseArgs {
    /// Delegated run id returned by `orchestrator turn`.
    pub run_id: String,
}

#[derive(Debug, Args)]
pub struct OrchestratorEventsQueryArgs {
    #[arg(long)]
    pub thread_id: Option<String>,

    #[arg(long)]
    pub run_id: Option<String>,

    #[arg(long)]
    pub agent_id: Option<String>,

    #[arg(long)]
    pub tool_id: Option<String>,

    #[arg(long)]
    pub event_type: Option<String>,

    #[arg(long)]
    pub error_contains: Option<String>,

    #[arg(long)]
    pub offset: Option<usize>,

    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct OrchestratorReportsQueryArgs {
    #[arg(long)]
    pub thread_id: Option<String>,

    #[arg(long)]
    pub run_id: Option<String>,

    #[arg(long)]
    pub agent_id: Option<String>,

    #[arg(long)]
    pub report_type: Option<String>,

    #[arg(long)]
    pub include_remediation: Option<bool>,

    #[arg(long)]
    pub offset: Option<usize>,

    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct OrchestratorThreadGetArgs {
    pub thread_id: String,

    #[arg(long)]
    pub offset: Option<usize>,

    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct OrchestratorThreadResurrectArgs {
    pub thread_id: String,
}

#[derive(Debug, Args)]
pub struct OrchestratorSubagentTurnArgs {
    #[arg(long)]
    pub thread_id: String,

    pub message: String,
}

fn parse_duration(input: &str) -> Result<Duration, String> {
    humantime::parse_duration(input).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_daemon_start_wait_healthy() {
        let cli = Cli::try_parse_from(["neuroctl", "daemon", "start", "--wait-healthy"])
            .expect("cli should parse");

        match cli.command {
            Command::Daemon {
                command: DaemonCommand::Start(args),
            } => {
                assert!(args.wait_healthy);
                assert!(args.config.is_none());
                assert_eq!(args.pid_file, PathBuf::from("/tmp/neuromancer.pid"));
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_daemon_start_with_explicit_config() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "daemon",
            "start",
            "--config",
            "/tmp/config.toml",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Daemon {
                command: DaemonCommand::Start(args),
            } => {
                assert_eq!(args.config, Some(PathBuf::from("/tmp/config.toml")));
                assert!(!args.wait_healthy);
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_daemon_restart_defaults() {
        let cli = Cli::try_parse_from(["neuroctl", "daemon", "restart"]).expect("cli should parse");

        match cli.command {
            Command::Daemon {
                command: DaemonCommand::Restart(args),
            } => {
                assert!(args.config.is_none());
                assert!(args.daemon_bin.is_none());
                assert_eq!(args.pid_file, PathBuf::from("/tmp/neuromancer.pid"));
                assert_eq!(args.grace, Duration::from_secs(15));
                assert!(!args.wait_healthy);
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_install_command() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "install",
            "--config",
            "/tmp/neuromancer.toml",
            "--override-config",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Install(args) => {
                assert_eq!(args.config, Some(PathBuf::from("/tmp/neuromancer.toml")));
                assert!(args.override_config);
            }
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_install_command_without_config() {
        let cli = Cli::try_parse_from(["neuroctl", "install"]).expect("cli should parse");

        match cli.command {
            Command::Install(args) => {
                assert!(args.config.is_none());
                assert!(!args.override_config);
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
    fn parses_orchestrator_chat_command() {
        let cli =
            Cli::try_parse_from(["neuroctl", "orchestrator", "chat"]).expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Chat(_),
            } => {}
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

    #[test]
    fn parses_orchestrator_runs_diagnose_command() {
        let cli = Cli::try_parse_from(["neuroctl", "orchestrator", "runs", "diagnose", "run-123"])
            .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Runs { command },
            } => match command {
                OrchestratorRunsCommand::Diagnose(args) => assert_eq!(args.run_id, "run-123"),
                other => panic!("unexpected runs command parsed: {other:?}"),
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_orchestrator_events_query_command() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "orchestrator",
            "events",
            "query",
            "--thread-id",
            "thread-1",
            "--tool-id",
            "delegate_to_agent",
            "--limit",
            "50",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Events { command },
            } => match command {
                OrchestratorEventsCommand::Query(args) => {
                    assert_eq!(args.thread_id.as_deref(), Some("thread-1"));
                    assert_eq!(args.tool_id.as_deref(), Some("delegate_to_agent"));
                    assert_eq!(args.limit, Some(50));
                }
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_orchestrator_reports_query_command() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "orchestrator",
            "reports",
            "query",
            "--run-id",
            "run-123",
            "--report-type",
            "stuck",
            "--include-remediation",
            "true",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Reports { command },
            } => match command {
                OrchestratorReportsCommand::Query(args) => {
                    assert_eq!(args.run_id.as_deref(), Some("run-123"));
                    assert_eq!(args.report_type.as_deref(), Some("stuck"));
                    assert_eq!(args.include_remediation, Some(true));
                }
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_orchestrator_stats_get_command() {
        let cli = Cli::try_parse_from(["neuroctl", "orchestrator", "stats", "get"])
            .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Stats { command },
            } => match command {
                OrchestratorStatsCommand::Get => {}
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_orchestrator_threads_resurrect_command() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "orchestrator",
            "threads",
            "resurrect",
            "thread-123",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Threads { command },
            } => match command {
                OrchestratorThreadsCommand::Resurrect(args) => {
                    assert_eq!(args.thread_id, "thread-123")
                }
                other => panic!("unexpected threads command parsed: {other:?}"),
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }

    #[test]
    fn parses_orchestrator_subagent_turn_command() {
        let cli = Cli::try_parse_from([
            "neuroctl",
            "orchestrator",
            "subagent",
            "turn",
            "--thread-id",
            "thread-123",
            "continue from here",
        ])
        .expect("cli should parse");

        match cli.command {
            Command::Orchestrator {
                command: OrchestratorCommand::Subagent { command },
            } => match command {
                OrchestratorSubagentCommand::Turn(args) => {
                    assert_eq!(args.thread_id, "thread-123");
                    assert_eq!(args.message, "continue from here");
                }
            },
            other => panic!("unexpected command parsed: {other:?}"),
        }
    }
}
