use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use neuromancer_core::rpc::OrchestratorTurnParams;

use crate::CliError;
use crate::daemon::{DaemonStartOptions, DaemonStopOptions, start_daemon, stop_daemon};
use crate::install::run_install;
use crate::rpc_client::RpcClient;

#[derive(Debug, Clone)]
pub struct SmokeOptions {
    pub config: PathBuf,
    pub daemon_bin: Option<PathBuf>,
    pub pid_file: PathBuf,
    pub addr: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmokeResult {
    pub turn_id: String,
    pub response: String,
}

pub async fn run_smoke(options: &SmokeOptions) -> Result<SmokeResult, CliError> {
    let _install = run_install(&options.config)?;

    let start_options = DaemonStartOptions {
        config: options.config.clone(),
        daemon_bin: options.daemon_bin.clone(),
        pid_file: options.pid_file.clone(),
        wait_healthy: true,
        addr: options.addr.clone(),
        timeout: options.timeout,
    };

    start_daemon(&start_options).await?;

    let rpc = RpcClient::new(&options.addr, options.timeout)?;

    let smoke_result = async {
        let _health = rpc.health().await?;

        let turn = rpc
            .orchestrator_turn(OrchestratorTurnParams {
                message: "e2e smoke turn".to_string(),
            })
            .await?;

        if turn.turn_id.trim().is_empty() {
            return Err(CliError::Smoke(
                "orchestrator.turn returned an empty turn_id".to_string(),
            ));
        }

        if turn.response.trim().is_empty() {
            return Err(CliError::Smoke(
                "orchestrator.turn returned an empty response".to_string(),
            ));
        }

        Ok(SmokeResult {
            turn_id: turn.turn_id,
            response: turn.response,
        })
    }
    .await;

    let stop_result = stop_daemon(&DaemonStopOptions {
        pid_file: options.pid_file.clone(),
        grace: Duration::from_secs(15),
    })
    .await;

    match (smoke_result, stop_result) {
        (Ok(result), Ok(_)) => Ok(result),
        (Err(err), Ok(_)) => Err(err),
        (Ok(_), Err(stop_err)) => Err(stop_err),
        (Err(smoke_err), Err(stop_err)) => Err(CliError::Smoke(format!(
            "smoke failed: {smoke_err}; daemon stop also failed: {stop_err}"
        ))),
    }
}
