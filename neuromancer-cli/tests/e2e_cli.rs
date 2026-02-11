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

fn write_minimal_config(dir: &Path, bind_addr: &str) -> PathBuf {
    let config_path = dir.join("neuromancer.toml");
    let config = format!(
        r#"
[global]
instance_id = "test-instance"
workspace_dir = "/tmp"
data_dir = "/tmp"

[routing]
default_agent = "planner"
rules = []

[agents.planner]

[admin_api]
bind_addr = "{}"
enabled = true
"#,
        bind_addr
    );

    fs::write(&config_path, config).expect("config should be written");
    config_path
}

fn write_finance_config(dir: &Path, bind_addr: &str) -> PathBuf {
    let config_path = dir.join("neuromancer-finance.toml");
    let config = format!(
        r#"
[global]
instance_id = "finance-test-instance"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "mock"
model = "test-double"

[routing]
default_agent = "finance_manager"
rules = []

[agents.finance_manager]
models.executor = "executor"
capabilities.skills = ["manage-bills", "manage-accounts"]
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

    fs::write(&config_path, config).expect("finance config should be written");
    config_path
}

fn write_finance_xdg_files(home: &Path) {
    let skills_root = home.join(".config/neuromancer/skills");
    let bills_skill = skills_root.join("manage-bills");
    let accounts_skill = skills_root.join("manage-accounts");
    fs::create_dir_all(&bills_skill).expect("create bills skill dir");
    fs::create_dir_all(&accounts_skill).expect("create accounts skill dir");

    fs::write(
        bills_skill.join("SKILL.md"),
        r#"---
name: "manage-bills"
version: "0.1.0"
description: "Read personal bills markdown"
metadata:
  neuromancer:
    data_sources:
      markdown: ["data/bills.md"]
---
Read bills markdown and summarize due items.
"#,
    )
    .expect("write manage-bills skill");

    fs::write(
        accounts_skill.join("SKILL.md"),
        r#"---
name: "manage-accounts"
version: "0.1.0"
description: "Read account balances from CSV"
metadata:
  neuromancer:
    data_sources:
      csv: ["data/accounts.csv"]
---
Read accounts csv and report available balances.
"#,
    )
    .expect("write manage-accounts skill");

    let data_dir = home.join(".local/neuromancer/data");
    fs::create_dir_all(&data_dir).expect("create local data dir");
    fs::write(
        data_dir.join("bills.md"),
        "# Bills\n- rent: $1200 due 2026-03-01\n- internet: $80 due 2026-02-20\n",
    )
    .expect("write bills markdown");
    fs::write(
        data_dir.join("accounts.csv"),
        "account,balance\nchecking,3400.50\nsavings,1200.00\n",
    )
    .expect("write accounts csv");
}

fn parse_json_output(output: &[u8]) -> Value {
    serde_json::from_slice(output).expect("command output should be valid json")
}

#[test]
fn daemon_lifecycle_start_status_stop() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_minimal_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };

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
fn task_roundtrip_over_rpc_commands() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_minimal_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };

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

    let submit_output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("task")
        .arg("submit")
        .arg("--agent")
        .arg("planner")
        .arg("--instruction")
        .arg("cli roundtrip")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let submit_json = parse_json_output(&submit_output);
    let task_id = submit_json["result"]["task_id"]
        .as_str()
        .expect("submit should return task_id")
        .to_string();

    let list_output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("task")
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let list_json = parse_json_output(&list_output);
    let contains_task = list_json["result"]["tasks"]
        .as_array()
        .expect("tasks should be array")
        .iter()
        .any(|task| task["id"] == Value::String(task_id.clone()));
    assert!(contains_task, "submitted task should be in task.list");

    let get_output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("task")
        .arg("get")
        .arg(&task_id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let get_json = parse_json_output(&get_output);
    assert_eq!(
        get_json["result"]["task"]["instruction"],
        Value::String("cli roundtrip".to_string())
    );

    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("task")
        .arg("cancel")
        .arg(&task_id)
        .assert()
        .success();

    let cancelled_get_output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("task")
        .arg("get")
        .arg(&task_id)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let cancelled_get_json = parse_json_output(&cancelled_get_output);
    assert_eq!(
        cancelled_get_json["result"]["task"]["state"],
        Value::String("cancelled".to_string())
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
fn smoke_command_runs_end_to_end() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_minimal_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };

    let smoke_output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("e2e")
        .arg("smoke")
        .arg("--config")
        .arg(&config)
        .arg("--daemon-bin")
        .arg(daemon_bin())
        .arg("--pid-file")
        .arg(&pid_file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let smoke_json = parse_json_output(&smoke_output);
    assert_eq!(smoke_json["ok"], Value::Bool(true));
    assert_eq!(smoke_json["result"]["cancelled"], Value::Bool(true));
    assert!(
        !pid_file.exists(),
        "smoke should stop daemon and remove pid file"
    );
}

#[test]
fn json_output_contract_is_stable() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_minimal_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };

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

    for command in [
        vec!["health"],
        vec!["task", "list"],
        vec!["config", "reload"],
    ] {
        let mut cmd = neuroctl();
        cmd.arg("--json").arg("--addr").arg(&addr);
        for part in command {
            cmd.arg(part);
        }

        let output = cmd.assert().success().get_output().stdout.clone();
        let json = parse_json_output(&output);
        assert!(json.get("ok").is_some(), "json envelope should include ok");
        assert!(
            json.get("result").is_some(),
            "json envelope should include result"
        );
    }

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
        .arg("task.list")
        .arg("--params")
        .arg("not-json")
        .assert()
        .code(2);
}

#[test]
fn message_command_routes_via_orchestrator_and_skills() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home dir");
    write_finance_xdg_files(&home);

    let (addr, bind_addr) = allocate_addrs();
    let config = write_finance_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };

    neuroctl()
        .env("HOME", &home)
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
        .env("HOME", &home)
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("message")
        .arg("What bills should I pay first and do I have enough cash?")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json = parse_json_output(&output);
    assert_eq!(json["ok"], Value::Bool(true));
    assert_eq!(
        json["result"]["assigned_agent"],
        Value::String("finance_manager".to_string())
    );
    assert_eq!(
        json["result"]["tool_usage"]["manage-bills"],
        Value::Number(1_u64.into())
    );
    assert_eq!(
        json["result"]["tool_usage"]["manage-accounts"],
        Value::Number(1_u64.into())
    );

    neuroctl()
        .env("HOME", &home)
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
