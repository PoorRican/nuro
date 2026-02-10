use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use neuromancer_core::rpc::{TaskCancelParams, TaskGetParams, TaskSubmitParams};

use crate::CliError;
use crate::daemon::{DaemonStartOptions, DaemonStopOptions, start_daemon, stop_daemon};
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
    pub task_id: String,
    pub cancelled: bool,
}

pub async fn run_smoke(options: &SmokeOptions) -> Result<SmokeResult, CliError> {
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

        let submit = rpc
            .task_submit(TaskSubmitParams {
                instruction: "e2e smoke task".to_string(),
                agent: "planner".to_string(),
            })
            .await?;

        let list = rpc.task_list().await?;
        if !list.tasks.iter().any(|task| task.id == submit.task_id) {
            return Err(CliError::Smoke(
                "submitted task was not returned by task.list".to_string(),
            ));
        }

        let fetched = rpc
            .task_get(TaskGetParams {
                task_id: submit.task_id.clone(),
            })
            .await?;

        if fetched.task.instruction != "e2e smoke task" {
            return Err(CliError::Smoke(
                "task.get instruction does not match submitted instruction".to_string(),
            ));
        }

        if fetched.task.assigned_agent != "planner" {
            return Err(CliError::Smoke(
                "task.get assigned_agent does not match submitted agent".to_string(),
            ));
        }

        let _cancel = rpc
            .task_cancel(TaskCancelParams {
                task_id: submit.task_id.clone(),
            })
            .await?;

        let cancelled = rpc
            .task_get(TaskGetParams {
                task_id: submit.task_id.clone(),
            })
            .await?;

        if cancelled.task.state != "cancelled" {
            return Err(CliError::Smoke(
                "task.cancel did not transition task state to cancelled".to_string(),
            ));
        }

        Ok(SmokeResult {
            task_id: submit.task_id,
            cancelled: true,
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
