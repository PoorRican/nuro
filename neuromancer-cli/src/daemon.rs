use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use neuromancer_core::config::NeuromancerConfig;
use neuromancer_core::xdg::XdgLayout;
use serde::Serialize;

use crate::CliError;
use crate::provider_keys::{provider_credential_targets, read_nonempty_env, read_nonempty_file};
use crate::rpc_client::RpcClient;

#[derive(Debug, Clone)]
pub struct DaemonStartOptions {
    pub config: PathBuf,
    pub daemon_bin: Option<PathBuf>,
    pub pid_file: PathBuf,
    pub wait_healthy: bool,
    pub addr: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct DaemonStopOptions {
    pub pid_file: PathBuf,
    pub grace: Duration,
}

#[derive(Debug, Clone)]
pub struct DaemonRestartOptions {
    pub config: PathBuf,
    pub daemon_bin: Option<PathBuf>,
    pub pid_file: PathBuf,
    pub grace: Duration,
    pub wait_healthy: bool,
    pub addr: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStartResult {
    pub pid: i32,
    pub pid_file: String,
    pub healthy: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStopResult {
    pub pid: i32,
    pub stopped: bool,
    pub forced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonRestartResult {
    pub previous_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<DaemonStopResult>,
    pub start: DaemonStartResult,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DaemonStatusResult {
    pub running: bool,
    pub pid: Option<i32>,
    pub pid_file: String,
    pub healthy: Option<bool>,
    pub detail: Option<String>,
}

pub async fn start_daemon(options: &DaemonStartOptions) -> Result<DaemonStartResult, CliError> {
    let parsed_config = load_and_validate_daemon_config(&options.config)?;
    let layout = XdgLayout::from_env().map_err(|err| CliError::Lifecycle(err.to_string()))?;
    let provider_env_values = resolve_provider_env_values(&parsed_config, &layout)?;
    let mut warnings = Vec::new();
    if parsed_config.agents.is_empty() {
        warnings.push(
            "no agents are configured; System0 can answer turns but cannot delegate until agents are added.".to_string(),
        );
    }

    if let Ok(existing_pid) = read_pid_file(&options.pid_file) {
        if pid_is_alive(existing_pid) {
            return Err(CliError::Lifecycle(format!(
                "daemon appears to already be running with pid {existing_pid}"
            )));
        }
        remove_pid_file(&options.pid_file)?;
    }

    let daemon_bin = resolve_daemon_bin(options.daemon_bin.as_deref())?;

    let mut command = Command::new(&daemon_bin);
    command
        .arg("--config")
        .arg(&options.config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (env_var, value) in provider_env_values {
        command.env(env_var, value);
    }

    let child = command.spawn().map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to spawn daemon '{}' with config '{}': {err}",
                daemon_bin.display(),
                options.config.display()
            ))
        })?;

    let pid = i32::try_from(child.id())
        .map_err(|_| CliError::Lifecycle("daemon pid did not fit in i32".to_string()))?;

    write_pid_file(&options.pid_file, pid)?;
    let readiness_timeout = if options.wait_healthy {
        options.timeout
    } else {
        std::cmp::min(options.timeout, Duration::from_secs(5))
    };
    wait_for_startup_readiness(
        pid,
        &options.pid_file,
        &options.config,
        &options.addr,
        readiness_timeout,
    )
    .await?;
    let healthy = Some(true);

    Ok(DaemonStartResult {
        pid,
        pid_file: options.pid_file.display().to_string(),
        healthy,
        warnings,
    })
}

pub async fn restart_daemon(
    options: &DaemonRestartOptions,
) -> Result<DaemonRestartResult, CliError> {
    let mut warnings = Vec::new();
    let status = daemon_status(
        &options.pid_file,
        &options.addr,
        std::cmp::min(options.timeout, Duration::from_secs(5)),
    )
    .await?;

    let mut stop_result = None;
    if status.running {
        let stopped = stop_daemon(&DaemonStopOptions {
            pid_file: options.pid_file.clone(),
            grace: options.grace,
        })
        .await?;
        stop_result = Some(stopped);
    } else {
        warnings.push("daemon was not running; starting a new instance.".to_string());
    }

    let mut started = start_daemon(&DaemonStartOptions {
        config: options.config.clone(),
        daemon_bin: options.daemon_bin.clone(),
        pid_file: options.pid_file.clone(),
        wait_healthy: options.wait_healthy,
        addr: options.addr.clone(),
        timeout: options.timeout,
    })
    .await?;
    warnings.append(&mut started.warnings);

    Ok(DaemonRestartResult {
        previous_running: status.running,
        stop: stop_result,
        start: started,
        warnings,
    })
}

pub async fn stop_daemon(options: &DaemonStopOptions) -> Result<DaemonStopResult, CliError> {
    let pid = read_pid_file(&options.pid_file)?;

    if !pid_is_alive(pid) {
        remove_pid_file(&options.pid_file)?;
        return Err(CliError::Lifecycle(format!(
            "daemon pid {pid} is not running; removed stale pid file"
        )));
    }

    send_signal(pid, "-TERM")?;

    let deadline = Instant::now() + options.grace;
    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            remove_pid_file(&options.pid_file)?;
            return Ok(DaemonStopResult {
                pid,
                stopped: true,
                forced: false,
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    send_signal(pid, "-KILL")?;

    let kill_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < kill_deadline {
        if !pid_is_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    remove_pid_file(&options.pid_file)?;

    Ok(DaemonStopResult {
        pid,
        stopped: !pid_is_alive(pid),
        forced: true,
    })
}

pub async fn daemon_status(
    pid_file: &Path,
    addr: &str,
    timeout: Duration,
) -> Result<DaemonStatusResult, CliError> {
    if !pid_file.exists() {
        return Ok(DaemonStatusResult {
            running: false,
            pid: None,
            pid_file: pid_file.display().to_string(),
            healthy: None,
            detail: Some("pid file not found".to_string()),
        });
    }

    let pid = match read_pid_file(pid_file) {
        Ok(pid) => pid,
        Err(err) => {
            return Ok(DaemonStatusResult {
                running: false,
                pid: None,
                pid_file: pid_file.display().to_string(),
                healthy: None,
                detail: Some(err.to_string()),
            });
        }
    };

    if !pid_is_alive(pid) {
        return Ok(DaemonStatusResult {
            running: false,
            pid: Some(pid),
            pid_file: pid_file.display().to_string(),
            healthy: None,
            detail: Some("pid not running".to_string()),
        });
    }

    let rpc = RpcClient::new(addr, timeout)?;
    match rpc.health().await {
        Ok(_) => Ok(DaemonStatusResult {
            running: true,
            pid: Some(pid),
            pid_file: pid_file.display().to_string(),
            healthy: Some(true),
            detail: None,
        }),
        Err(err) => Ok(DaemonStatusResult {
            running: true,
            pid: Some(pid),
            pid_file: pid_file.display().to_string(),
            healthy: Some(false),
            detail: Some(err.to_string()),
        }),
    }
}

fn load_and_validate_daemon_config(config_path: &Path) -> Result<NeuromancerConfig, CliError> {
    if config_path.is_dir() {
        return Err(CliError::Usage(format!(
            "config path '{}' is a directory; expected a TOML file",
            config_path.display()
        )));
    }

    if !config_path.exists() {
        return Err(CliError::Usage(format!(
            "config file '{}' was not found. {}",
            config_path.display(),
            install_create_hint_for(config_path)
        )));
    }

    let raw = fs::read_to_string(config_path).map_err(|err| {
        CliError::Usage(format!(
            "failed to read config '{}': {err}",
            config_path.display()
        ))
    })?;

    let config = toml::from_str::<NeuromancerConfig>(&raw).map_err(|err| {
        CliError::Usage(format!(
            "config '{}' is invalid: {err}. {}",
            config_path.display(),
            install_override_hint_for(config_path)
        ))
    })?;

    Ok(config)
}

fn resolve_provider_env_values(
    config: &NeuromancerConfig,
    layout: &XdgLayout,
) -> Result<Vec<(String, String)>, CliError> {
    let mut resolved = Vec::new();
    for target in provider_credential_targets(config, layout) {
        if let Some(value) = read_nonempty_env(&target.env_var) {
            resolved.push((target.env_var, value));
            continue;
        }

        let file_value = read_nonempty_file(&target.path).map_err(|err| {
            CliError::Lifecycle(format!(
                "failed reading provider key file '{}': {err}",
                target.path.display()
            ))
        })?;
        if let Some(value) = file_value {
            resolved.push((target.env_var, value));
            continue;
        }

        return Err(CliError::Usage(format!(
            "config uses provider '{}' but no credential was found. Expected '{}' env var or key file '{}'. Run `neuroctl install` to capture provider keys.",
            target.provider,
            target.env_var,
            target.path.display()
        )));
    }

    Ok(resolved)
}

fn install_create_hint_for(config_path: &Path) -> String {
    match XdgLayout::from_env() {
        Ok(layout) if config_path == layout.default_config_path() => {
            "Run `neuroctl install` to bootstrap defaults.".to_string()
        }
        _ => format!(
            "Run `neuroctl install --config {}` to bootstrap defaults for this path.",
            config_path.display()
        ),
    }
}

fn install_override_hint_for(config_path: &Path) -> String {
    match XdgLayout::from_env() {
        Ok(layout) if config_path == layout.default_config_path() => {
            "Run `neuroctl install --override-config` to rewrite defaults.".to_string()
        }
        _ => format!(
            "Run `neuroctl install --config {} --override-config` to rewrite defaults for this path.",
            config_path.display()
        ),
    }
}

pub fn resolve_daemon_bin(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }

    if let Some(path) = env::var_os("NEUROMANCERD_BIN") {
        return Ok(PathBuf::from(path));
    }

    if let Some(path) = find_in_path("neuromancerd") {
        return Ok(path);
    }

    let fallback = PathBuf::from("target/debug/neuromancerd");
    if fallback.exists() {
        return Ok(fallback);
    }

    Err(CliError::Lifecycle(
        "unable to resolve neuromancerd binary (use --daemon-bin or NEUROMANCERD_BIN)".to_string(),
    ))
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = env::var_os("PATH")?;
    env::split_paths(&path_var)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.exists())
}

fn read_pid_file(pid_file: &Path) -> Result<i32, CliError> {
    let raw = fs::read_to_string(pid_file).map_err(|err| {
        CliError::Lifecycle(format!(
            "failed to read pid file '{}': {err}",
            pid_file.display()
        ))
    })?;

    raw.trim().parse::<i32>().map_err(|err| {
        CliError::Lifecycle(format!(
            "invalid pid in file '{}': {err}",
            pid_file.display()
        ))
    })
}

fn write_pid_file(pid_file: &Path, pid: i32) -> Result<(), CliError> {
    if let Some(parent) = pid_file.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to create pid dir '{}': {err}",
                parent.display()
            ))
        })?;
    }

    fs::write(pid_file, format!("{pid}\n")).map_err(|err| {
        CliError::Lifecycle(format!(
            "failed to write pid file '{}': {err}",
            pid_file.display()
        ))
    })
}

fn remove_pid_file(pid_file: &Path) -> Result<(), CliError> {
    if pid_file.exists() {
        fs::remove_file(pid_file).map_err(|err| {
            CliError::Lifecycle(format!(
                "failed to remove pid file '{}': {err}",
                pid_file.display()
            ))
        })?;
    }
    Ok(())
}

fn pid_is_alive(pid: i32) -> bool {
    let status = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    matches!(status, Ok(exit) if exit.success())
}

fn send_signal(pid: i32, signal: &str) -> Result<(), CliError> {
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .map_err(|err| {
            CliError::Lifecycle(format!("failed to invoke kill for pid {pid}: {err}"))
        })?;

    if !status.success() {
        return Err(CliError::Lifecycle(format!(
            "failed to send signal {signal} to pid {pid}"
        )));
    }

    Ok(())
}

async fn wait_for_startup_readiness(
    pid: i32,
    pid_file: &Path,
    config_path: &Path,
    addr: &str,
    timeout: Duration,
) -> Result<(), CliError> {
    let deadline = Instant::now() + timeout;
    let client = RpcClient::new(addr, Duration::from_secs(1))?;
    let mut last_error = None;

    while Instant::now() < deadline {
        if !pid_is_alive(pid) {
            remove_pid_file(pid_file)?;
            return Err(CliError::Lifecycle(format!(
                "daemon process exited during startup (pid {pid}). Check config '{}'.",
                config_path.display()
            )));
        }

        match client.health().await {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_error = Some(err.to_string());
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    }

    if !pid_is_alive(pid) {
        remove_pid_file(pid_file)?;
        return Err(CliError::Lifecycle(format!(
            "daemon process exited during startup (pid {pid}) before health became available."
        )));
    }

    Err(CliError::Lifecycle(format!(
        "daemon started (pid {pid}) but admin RPC did not become healthy within {} at '{}'. {}",
        humantime::format_duration(timeout),
        addr,
        last_error.unwrap_or_else(|| "no response".to_string())
    )))
}
