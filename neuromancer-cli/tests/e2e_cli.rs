use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::OnceLock;
use std::time::Duration;

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

fn write_blank_config_no_agents(dir: &Path, bind_addr: &str) -> PathBuf {
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

[orchestrator]
model_slot = "executor"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"

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

[orchestrator]
model_slot = "executor"

[agents.finance-manager]
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

    fs::write(&config_path, config).expect("config should be written");
    config_path
}

fn write_finance_skill_fixtures(xdg_config_home: &Path, xdg_data_home: &Path) {
    let skills_root = xdg_config_home.join("neuromancer/skills");
    let bills_skill = skills_root.join("manage-bills");
    let accounts_skill = skills_root.join("manage-accounts");
    fs::create_dir_all(bills_skill.join("scripts")).expect("bills scripts dir");
    fs::create_dir_all(accounts_skill.join("scripts")).expect("accounts scripts dir");

    fs::write(
        bills_skill.join("SKILL.md"),
        r#"---
name: "manage-bills"
version: "0.1.0"
description: "Compute bill totals and due-date priorities from markdown data"
metadata:
  neuromancer:
    data_sources:
      markdown: ["data/bills.md"]
    execution:
      script: "scripts/manage_bills.py"
      timeout_ms: 3000
---
Summarize bill obligations with totals and upcoming due dates.
"#,
    )
    .expect("bills SKILL.md");
    fs::write(
        bills_skill.join("scripts/manage_bills.py"),
        r#"import json
import re
from pathlib import Path
import sys

payload = json.loads(sys.stdin.read())
local_root = Path(payload["local_root"])
markdown_paths = payload.get("data_sources", {}).get("markdown", [])
target = local_root / markdown_paths[0]
text = target.read_text(encoding="utf-8")

entries = []
for line in text.splitlines():
    line = line.strip()
    if not line.startswith("-"):
        continue
    amount_match = re.search(r"\$([0-9]+(?:\.[0-9]{1,2})?)", line)
    due_match = re.search(r"due\s+([0-9]{4}-[0-9]{2}-[0-9]{2})", line)
    if amount_match and due_match:
        entries.append(
            {
                "amount": float(amount_match.group(1)),
                "due": due_match.group(1),
            }
        )

entries.sort(key=lambda item: item["due"])
total_due = round(sum(item["amount"] for item in entries), 2)
next_due = entries[0]["due"] if entries else None
next_due_amount = entries[0]["amount"] if entries else 0.0

print(
    json.dumps(
        {
            "bill_count": len(entries),
            "total_due": total_due,
            "next_due_date": next_due,
            "next_due_amount": next_due_amount,
        }
    )
)
"#,
    )
    .expect("bills script");

    fs::write(
        accounts_skill.join("SKILL.md"),
        r#"---
name: "manage-accounts"
version: "0.1.0"
description: "Compute liquidity summary from CSV account balances"
metadata:
  neuromancer:
    data_sources:
      csv: ["data/accounts.csv"]
    execution:
      script: "scripts/manage_accounts.py"
      timeout_ms: 3000
---
Summarize account balances and available liquidity.
"#,
    )
    .expect("accounts SKILL.md");
    fs::write(
        accounts_skill.join("scripts/manage_accounts.py"),
        r#"import csv
import json
from pathlib import Path
import sys

payload = json.loads(sys.stdin.read())
local_root = Path(payload["local_root"])
csv_paths = payload.get("data_sources", {}).get("csv", [])
target = local_root / csv_paths[0]

total_balance = 0.0
rows = 0
with target.open("r", encoding="utf-8") as handle:
    reader = csv.DictReader(handle)
    for row in reader:
        rows += 1
        total_balance += float(row.get("balance", "0") or "0")

print(json.dumps({"account_count": rows, "total_balance": round(total_balance, 2)}))
"#,
    )
    .expect("accounts script");

    let data_root = xdg_data_home.join("neuromancer/data");
    fs::create_dir_all(&data_root).expect("data root");
    fs::write(
        data_root.join("bills.md"),
        r#"# Bills
- rent: $1200 due 2026-03-01
- electricity: $95 due 2026-02-18
- phone: $65 due 2026-02-15
"#,
    )
    .expect("bills data");
    fs::write(
        data_root.join("accounts.csv"),
        "account,balance\nchecking,3400.50\nsavings,1200.00\n",
    )
    .expect("accounts data");
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
fn daemon_restart_command_restarts_running_daemon() {
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

    let restart = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("daemon")
        .arg("restart")
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

    let restart_json = parse_json_output(&restart);
    assert_eq!(restart_json["ok"], Value::Bool(true));
    assert_eq!(
        restart_json["result"]["previous_running"],
        Value::Bool(true)
    );
    assert_eq!(
        restart_json["result"]["start"]["healthy"],
        Value::Bool(true)
    );
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
    let tool_invocations = json["result"]["tool_invocations"]
        .as_array()
        .expect("turn result should include tool invocations");
    assert!(
        !tool_invocations.is_empty(),
        "mock orchestrator turn should emit tool invocations"
    );
    assert!(
        tool_invocations[0].get("arguments").is_some(),
        "tool invocations should include arguments"
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
fn orchestrator_turn_routes_finance_queries_to_finance_manager_with_numeric_summary() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_finance_config(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).expect("home dir");

    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("install")
        .arg("--config")
        .arg(&config)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .assert()
        .success();

    write_finance_skill_fixtures(&xdg_config_home, &xdg_data_home);

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
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .assert()
        .success();

    let output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("orchestrator")
        .arg("turn")
        .arg("Please review my finance status and summarize bills versus account balance.")
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

    let delegated_tasks = json["result"]["delegated_tasks"]
        .as_array()
        .expect("delegated tasks should exist");
    let finance_run = delegated_tasks.first().expect("finance task should exist");
    assert_eq!(
        finance_run["agent_id"].as_str(),
        Some("finance-manager"),
        "finance queries should be delegated to finance-manager"
    );
    assert_eq!(finance_run["state"].as_str(), Some("queued"));
    let run_id = finance_run["run_id"]
        .as_str()
        .expect("delegated run id should exist")
        .to_string();

    let mut completed_output = None;
    for _ in 0..30 {
        let pulled = neuroctl()
            .arg("--json")
            .arg("--addr")
            .arg(&addr)
            .arg("orchestrator")
            .arg("outputs")
            .arg("pull")
            .arg("--limit")
            .arg("10")
            .env("XDG_CONFIG_HOME", &xdg_config_home)
            .env("XDG_DATA_HOME", &xdg_data_home)
            .env("HOME", &home_dir)
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let pull_json = parse_json_output(&pulled);
        let outputs = pull_json["result"]["outputs"]
            .as_array()
            .expect("outputs should be an array");
        if let Some(found) = outputs
            .iter()
            .find(|item| item["run_id"].as_str() == Some(run_id.as_str()))
        {
            completed_output = Some(found.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let completed_output = completed_output.expect("expected completed output for delegated run");
    let run_summary = completed_output["summary"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let run_content = completed_output["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        run_summary.contains("1360.00") || run_content.contains("1360.00"),
        "bill total should appear in output: summary={run_summary}, content={run_content}"
    );
    assert!(
        run_summary.contains("4600.50") || run_content.contains("4600.50"),
        "account total should appear in output: summary={run_summary}, content={run_content}"
    );

    let tool_invocations = json["result"]["tool_invocations"]
        .as_array()
        .expect("tool invocations should exist");
    let delegate_call = tool_invocations
        .iter()
        .find(|invocation| invocation["tool_id"] == Value::String("delegate_to_agent".into()))
        .expect("delegate_to_agent invocation should exist");
    assert_eq!(
        delegate_call["arguments"]["agent_id"].as_str(),
        Some("finance-manager")
    );

    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("daemon")
        .arg("stop")
        .arg("--pid-file")
        .arg(&pid_file)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .assert()
        .success();
}

#[test]
fn orchestrator_chat_ndjson_streams_snapshot_and_turn_events() {
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
        .arg("chat")
        .write_stdin("{\"message\":\"first line\\nsecond line\"}\nplain follow-up\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rendered = String::from_utf8(output).expect("stdout should be utf8");
    let events: Vec<Value> = rendered
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line should be JSON"))
        .collect();

    assert!(
        events.len() >= 3,
        "expected snapshot + two turn events, got {}",
        events.len()
    );
    assert_eq!(events[0]["event"], Value::String("snapshot".to_string()));
    assert!(events[0]["system_thread"]["messages"].is_array());

    let completes: Vec<&Value> = events
        .iter()
        .filter(|event| event["event"] == Value::String("turn_complete".to_string()))
        .collect();
    assert_eq!(completes.len(), 2, "expected two turn_complete events");
    assert!(
        completes[0]["input"]
            .as_str()
            .unwrap_or_default()
            .contains('\n'),
        "first NDJSON payload should preserve embedded newlines",
    );
    assert!(completes[0]["runs"].is_array());

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
    assert!(
        prompt_file.exists(),
        "install should create orchestrator prompt"
    );
}

#[test]
fn install_without_config_uses_xdg_config_and_data_home() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, _bind_addr) = allocate_addrs();
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).expect("home dir");

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

    let generated_config = xdg_config_home.join("neuromancer/neuromancer.toml");
    let orchestrator_prompt = xdg_config_home.join("neuromancer/orchestrator/SYSTEM.md");
    let planner_prompt = xdg_config_home.join("neuromancer/agents/planner/SYSTEM.md");
    let runtime_root = xdg_data_home.join("neuromancer");
    let provider_keys_root = xdg_data_home.join("neuromancer/provider_keys");
    assert!(
        generated_config.exists(),
        "install should bootstrap blank-slate config under XDG_CONFIG_HOME",
    );
    assert!(
        orchestrator_prompt.exists(),
        "install should create orchestrator prompt under XDG_CONFIG_HOME",
    );
    assert!(
        !planner_prompt.exists(),
        "blank-slate install should not create per-agent prompt files",
    );
    assert!(
        runtime_root.exists(),
        "install should create runtime root under XDG_DATA_HOME",
    );
    assert!(
        provider_keys_root.exists(),
        "install should create provider key directory under runtime home fallback",
    );
}

#[test]
fn install_with_missing_explicit_config_bootstraps() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, _bind_addr) = allocate_addrs();
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).expect("home dir");

    let explicit_config = temp.path().join("custom-config/neuromancer.toml");
    assert!(
        !explicit_config.exists(),
        "precondition: config should not exist"
    );

    let output = neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("install")
        .arg("--config")
        .arg(&explicit_config)
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
    assert!(
        explicit_config.exists(),
        "install should bootstrap missing explicit config file",
    );

    let orchestrator_prompt = xdg_config_home.join("neuromancer/orchestrator/SYSTEM.md");
    let planner_prompt = xdg_config_home.join("neuromancer/agents/planner/SYSTEM.md");
    let runtime_root = xdg_data_home.join("neuromancer");
    let provider_keys_root = xdg_data_home.join("neuromancer/provider_keys");
    assert!(
        orchestrator_prompt.exists(),
        "install should create orchestrator prompt under XDG_CONFIG_HOME",
    );
    assert!(
        !planner_prompt.exists(),
        "blank-slate config should not imply planner prompt creation",
    );
    assert!(
        runtime_root.exists(),
        "install should create runtime root under XDG_DATA_HOME",
    );
    assert!(
        provider_keys_root.exists(),
        "install should create provider key directory under runtime home fallback",
    );
}

#[test]
fn install_with_override_config_rewrites_existing_config() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, _bind_addr) = allocate_addrs();
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).expect("home dir");

    let explicit_config = temp.path().join("custom-config/neuromancer.toml");
    fs::create_dir_all(
        explicit_config
            .parent()
            .expect("explicit config should have parent"),
    )
    .expect("config dir");
    fs::write(
        &explicit_config,
        r#"[models.executor]
provider = "mock"
model = "test-double"
"#,
    )
    .expect("seed config");

    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("install")
        .arg("--config")
        .arg(&explicit_config)
        .arg("--override-config")
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .assert()
        .success();

    let rewritten = fs::read_to_string(&explicit_config).expect("read rewritten config");
    assert!(
        rewritten.contains("provider = \"groq\""),
        "override should rewrite model provider to groq default",
    );
    assert!(
        rewritten.contains("model = \"openai/gpt-oss-120B\""),
        "override should rewrite model id to gpt-oss-120B default",
    );
}

#[test]
fn rpc_transport_failures_use_exit_code_4() {
    let stderr = neuroctl()
        .arg("--addr")
        .arg("http://127.0.0.1:1")
        .arg("health")
        .assert()
        .code(4)
        .get_output()
        .stderr
        .clone();

    let message = String::from_utf8(stderr).expect("stderr should be utf-8");
    assert!(
        message.contains("does not appear to be running"),
        "transport error should explain that daemon is likely down: {message}",
    );
}

#[test]
fn daemon_start_without_config_uses_default_and_explains_missing_config() {
    let temp = TempDir::new().expect("tempdir");
    let home_dir = temp.path().join("home");
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    fs::create_dir_all(&home_dir).expect("home dir");

    let pid_file = temp.path().join("daemon.pid");
    let stderr = neuroctl()
        .arg("daemon")
        .arg("start")
        .arg("--daemon-bin")
        .arg(daemon_bin())
        .arg("--pid-file")
        .arg(&pid_file)
        .env("HOME", &home_dir)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();

    let message = String::from_utf8(stderr).expect("stderr should be utf-8");
    assert!(
        message.contains("config file"),
        "missing config should be explicitly reported: {message}",
    );
    assert!(
        message.contains("Run `neuroctl install`"),
        "missing config should provide install remediation: {message}",
    );
}

#[test]
fn daemon_start_warns_when_no_agents_configured() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let config = write_blank_config_no_agents(temp.path(), &bind_addr);
    let pid_file = temp.path().join("daemon.pid");
    let _cleanup = Cleanup {
        pid_file: pid_file.clone(),
    };
    run_install(&config, &addr);

    let start = neuroctl()
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
        .stderr
        .clone();

    let stderr = String::from_utf8(start).expect("stderr should be utf-8");
    assert!(
        stderr.contains("warning: no agents are configured"),
        "start should emit a warning when no agents are configured: {stderr}",
    );

    neuroctl()
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
fn daemon_start_fails_with_clear_error_when_groq_key_missing() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).expect("home dir");
    let config = temp.path().join("neuromancer.toml");
    fs::write(
        &config,
        format!(
            r#"
[global]
instance_id = "test-instance"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "groq"
model = "openai/gpt-oss-120B"

[orchestrator]
model_slot = "executor"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"

[admin_api]
bind_addr = "{}"
enabled = true
"#,
            bind_addr
        ),
    )
    .expect("write config");
    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("install")
        .arg("--config")
        .arg(&config)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .assert()
        .success();

    let pid_file = temp.path().join("daemon.pid");
    let stderr = neuroctl()
        .arg("daemon")
        .arg("start")
        .arg("--config")
        .arg(&config)
        .arg("--daemon-bin")
        .arg(daemon_bin())
        .arg("--pid-file")
        .arg(&pid_file)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .env_remove("GROQ_API_KEY")
        .assert()
        .code(2)
        .get_output()
        .stderr
        .clone();

    let message = String::from_utf8(stderr).expect("stderr should be utf-8");
    assert!(
        message.contains("provider 'groq'"),
        "missing provider credential should be reported explicitly: {message}",
    );
    assert!(
        message.contains("GROQ_API_KEY"),
        "missing env var should be named explicitly: {message}",
    );
    assert!(
        message.contains("Run `neuroctl install`"),
        "remediation should direct user to install key capture: {message}",
    );
}

#[test]
fn daemon_start_uses_provider_key_file_without_env_var() {
    let temp = TempDir::new().expect("tempdir");
    let (addr, bind_addr) = allocate_addrs();
    let xdg_config_home = temp.path().join("xdg-config-home");
    let xdg_data_home = temp.path().join("xdg-data-home");
    let home_dir = temp.path().join("home");
    fs::create_dir_all(&home_dir).expect("home dir");

    let config = temp.path().join("neuromancer.toml");
    fs::write(
        &config,
        format!(
            r#"
[global]
instance_id = "test-instance"
workspace_dir = "/tmp"
data_dir = "/tmp"

[models.executor]
provider = "groq"
model = "openai/gpt-oss-120B"

[orchestrator]
model_slot = "executor"
system_prompt_path = "prompts/orchestrator/SYSTEM.md"

[admin_api]
bind_addr = "{}"
enabled = true
"#,
            bind_addr
        ),
    )
    .expect("write config");

    neuroctl()
        .arg("--json")
        .arg("--addr")
        .arg(&addr)
        .arg("install")
        .arg("--config")
        .arg(&config)
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .assert()
        .success();

    let key_file = xdg_data_home.join("neuromancer/provider_keys/groq.key");
    fs::create_dir_all(
        key_file
            .parent()
            .expect("key file should have parent directory"),
    )
    .expect("provider key dir");
    fs::write(&key_file, "dummy-key\n").expect("write provider key file");

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
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env("XDG_DATA_HOME", &xdg_data_home)
        .env("HOME", &home_dir)
        .env_remove("GROQ_API_KEY")
        .assert()
        .success();

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
