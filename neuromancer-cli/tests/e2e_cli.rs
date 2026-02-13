use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::OnceLock;

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

static DAEMON_BIN: OnceLock<PathBuf> = OnceLock::new();

struct Cleanup {
    pid_file: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Ok(raw_pid) = fs::read_to_string(&self.pid_file)
            && let Ok(pid) = raw_pid.trim().parse::<i32>()
        {
            let _ = StdCommand::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status();
        }
        let _ = fs::remove_file(&self.pid_file);
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn daemon_bin() -> &'static PathBuf {
    DAEMON_BIN.get_or_init(|| {
        let root = workspace_root();
        let status = StdCommand::new("cargo")
            .arg("build")
            .arg("-p")
            .arg("neuromancerd")
            .current_dir(&root)
            .status()
            .expect("cargo build should run");

        assert!(status.success(), "failed to build neuromancerd binary");

        root.join("target/debug/neuromancerd")
    })
}

fn neuroctl() -> Command {
    Command::new(assert_cmd::cargo::cargo_bin!("neuroctl"))
}

fn allocate_addrs() -> (String, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);

    (format!("http://{}", addr), addr.to_string())
}

fn write_orchestrator_config(dir: &Path, bind_addr: &str) -> PathBuf {
    let config_path = dir.join("neuromancer.toml");
    let config = format!(
        r#"
[global]
instance_id = "test-instance"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "mock"
model = "test-double"

[routing]
default_agent = "planner"
rules = []

[orchestrator]
model_slot = "executor"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"

[agents.planner]
models.executor = "executor"
system_prompt_path = "prompts/agents/planner/SYSTEM.md"
capabilities.skills = []
capabilities.mcp_servers = []
capabilities.a2a_peers = []
capabilities.secrets = []
capabilities.memory_partitions = []
capabilities.filesystem_roots = []

[admin_api]
bind_addr = "{}"
enabled = true
"#,
        bind_addr
    );

    fs::write(&config_path, config).expect("config should be written");
    config_path
}

fn parse_json_output(output: &[u8]) -> Value {
    serde_json::from_slice(output).expect("command output should be valid json")
}

fn run_install(config: &Path, addr: &str) {
    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(addr)
        .arg("install")
        .arg("--config")
        .arg(config)
        .assert()
        .success();
}

#[test]
fn daemon_lifecycle_start_status_stop() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_orchestrator_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };
    run_install(&config, &addr);

    let start = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("daemon")
        .arg("start")
        .arg("--config")
        .arg(&config)
        .arg("--daemon-bin")
        .arg(daemon_bin())
        .arg("--pid-file")
        .arg(&pid_file)
        .arg("--wait-healthy")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let start_json = parse_json_output(&start);
    assert_eq!(start_json["ok"], Value::Bool(true));

    let status = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("daemon")
        .arg("status")
        .arg("--pid-file")
        .arg(&pid_file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let status_json = parse_json_output(&status);
    assert_eq!(status_json["ok"], Value::Bool(true));
    assert_eq!(status_json["result"]["running"], Value::Bool(true));
    assert_eq!(status_json["result"]["healthy"], Value::Bool(true));

    let stop = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("daemon")
        .arg("stop")
        .arg("--pid-file")
        .arg(&pid_file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stop_json = parse_json_output(&stop);
    assert_eq!(stop_json["ok"], Value::Bool(true));
    assert_eq!(stop_json["result"]["stopped"], Value::Bool(true));
}

#[test]
fn orchestrator_turn_command_routes_via_rpc() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_orchestrator_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };
    run_install(&config, &addr);

    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("daemon")
        .arg("start")
        .arg("--config")
        .arg(&config)
        .arg("--daemon-bin")
        .arg(daemon_bin())
        .arg("--pid-file")
        .arg(&pid_file)
        .arg("--wait-healthy")
        .assert()
        .success();

    let output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("orchestrator")
        .arg("turn")
        .arg("what should I do next?")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json_output(&output);
    assert_eq!(json["ok"], Value::Bool(true));
    assert!(json["result"]["turn_id"].as_str().unwrap_or_default().len() > 8);
    assert!(
        !json["result"]["response"]
            .as_str()
            .unwrap_or_default()
            .is_empty()
    );

    let runs_output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("orchestrator")
        .arg("runs")
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let runs_json = parse_json_output(&runs_output);
    assert_eq!(runs_json["ok"], Value::Bool(true));
    let run_id = runs_json["result"]["runs"]
        .as_array()
        .and_then(|runs| runs.first())
        .and_then(|run| run["run_id"].as_str())
        .expect("at least one delegated run should be tracked")
        .to_string();

    let run_get_output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("orchestrator")
        .arg("runs")
        .arg("get")
        .arg(&run_id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let run_get_json = parse_json_output(&run_get_output);
    assert_eq!(run_get_json["ok"], Value::Bool(true));
    assert_eq!(
        run_get_json["result"]["run"]["run_id"].as_str(),
        Some(run_id.as_str())
    );

    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("daemon")
        .arg("stop")
        .arg("--pid-file")
        .arg(&pid_file)
        .assert()
        .success();
}

#[test]
fn removed_task_command_fails_with_usage_error() {
    neuroctl().arg("task").arg("list").assert().code(2);
}

#[test]
fn install_command_creates_prompt_files() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_orchestrator_config(temp.path(), &bind_addr);

    let output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("install")
        .arg("--config")
        .arg(&config)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json_output(&output);
    assert_eq!(json["ok"], Value::Bool(true));

    let prompt_file = temp.path().join("prompts/orchestrator/SYSTEM.md");
    assert!(prompt_file.exists(), "install should create orchestrator prompt");
}

#[test]
fn install_without_config_uses_xdg_config_and_data_home() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).expect("home dir");
    let neuromancer_config_dir = xdg_config_home.join("neuromancer");
    fs::create_dir_all(&neuromancer_config_dir).expect("xdg config dir");
    let config = neuromancer_config_dir.join("neuromancer.toml");

    let config_toml = format!(
        r#"
[global]
instance_id = "test-instance"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "mock"
model = "test-double"

[routing]
default_agent = "planner"
rules = []

[orchestrator]
model_slot = "executor"

[agents.planner]
models.executor = "executor"
capabilities.skills = []
capabilities.mcp_servers = []
capabilities.a2a_peers = []
capabilities.secrets = []
capabilities.memory_partitions = []
capabilities.filesystem_roots = []

[admin_api]
bind_addr = "{}"
enabled = true
"#,
        bind_addr
    );
    fs::write(&config, config_toml).expect("write default config");

    let output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("install")
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json_output(&output);
    assert_eq!(json["ok"], Value::Bool(true));

    let orchestrator_prompt = xdg_config_home.join("neuromancer/orchestrator/SYSTEM.md");
    let planner_prompt = xdg_config_home.join("neuromancer/agents/planner/SYSTEM.md");
    let runtime_root = xdg_data_home.join("neuromancer");
    assert!(
        orchestrator_prompt.exists(),
        "install should create orchestrator prompt under XDG_CONFIG_HOME",
    );
    assert!(
        planner_prompt.exists(),
        "install should create agent prompt under XDG_CONFIG_HOME",
    );
    assert!(
        runtime_root.exists(),
        "install should create runtime root under XDG_DATA_HOME",
    );
}

#[test]
fn rpc_transport_failures_use_exit_code_4() {
    neuroctl()
        .arg("--addr")
        .arg("http://127.0.0.1:1")
        .arg("health")
        .assert()
        .code(4);
}

#[test]
fn usage_errors_use_exit_code_2() {
    neuroctl()
        .arg("rpc")
        .arg("call")
        .arg("--method")
        .arg("admin.health")
        .arg("--params")
        .arg("not-json")
        .assert()
        .code(2);
}
