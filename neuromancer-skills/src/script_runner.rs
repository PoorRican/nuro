use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command as TokioCommand;

use crate::SkillError;

pub async fn run_skill_script(
    script_path: &Path,
    payload: &serde_json::Value,
    timeout: Duration,
    agent_id: &str,
    task_id: &str,
    tool_id: &str,
) -> Result<serde_json::Value, SkillError> {
    let started_at = std::time::Instant::now();
    tracing::info!(
        agent_id = %agent_id,
        task_id = %task_id,
        tool_id = %tool_id,
        script_path = %script_path.display(),
        timeout_ms = timeout.as_millis(),
        "skill_script_started"
    );

    let stdin_payload = serde_json::to_vec(payload).map_err(|err| {
        SkillError::ScriptExecution(format!(
            "failed to encode script input payload: {err}"
        ))
    })?;

    let mut child = TokioCommand::new("python3")
        .arg("-I")
        .arg(script_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|err| {
            SkillError::ScriptExecution(format!(
                "failed to start python script '{}': {err}",
                script_path.display()
            ))
        })?;

    let mut stdin = child.stdin.take().ok_or_else(|| {
        SkillError::ScriptExecution(format!(
            "script stdin is unavailable for '{}'",
            script_path.display()
        ))
    })?;
    stdin.write_all(&stdin_payload).await.map_err(|err| {
        SkillError::ScriptExecution(format!(
            "failed to write input to script '{}': {err}",
            script_path.display()
        ))
    })?;
    drop(stdin);

    let mut stdout = child.stdout.take().ok_or_else(|| {
        SkillError::ScriptExecution(format!(
            "script stdout is unavailable for '{}'",
            script_path.display()
        ))
    })?;
    let mut stderr = child.stderr.take().ok_or_else(|| {
        SkillError::ScriptExecution(format!(
            "script stderr is unavailable for '{}'",
            script_path.display()
        ))
    })?;

    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).await.map(|_| buffer)
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        stderr.read_to_end(&mut buffer).await.map(|_| buffer)
    });

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|err| {
            SkillError::ScriptExecution(format!(
                "failed waiting for script '{}': {err}",
                script_path.display()
            ))
        })?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(SkillError::ScriptTimeout(format!(
                "script '{}' exceeded timeout of {}ms",
                script_path.display(),
                timeout.as_millis()
            )));
        }
    };

    let stdout_bytes = stdout_task
        .await
        .map_err(|err| {
            SkillError::ScriptExecution(format!(
                "failed joining stdout reader for '{}': {err}",
                script_path.display()
            ))
        })?
        .map_err(|err| {
            SkillError::ScriptExecution(format!(
                "failed reading script stdout '{}': {err}",
                script_path.display()
            ))
        })?;
    let stderr_bytes = stderr_task
        .await
        .map_err(|err| {
            SkillError::ScriptExecution(format!(
                "failed joining stderr reader for '{}': {err}",
                script_path.display()
            ))
        })?
        .map_err(|err| {
            SkillError::ScriptExecution(format!(
                "failed reading script stderr '{}': {err}",
                script_path.display()
            ))
        })?;

    let stderr = String::from_utf8_lossy(&stderr_bytes);
    if !status.success() {
        return Err(SkillError::ScriptExecution(format!(
            "script '{}' exited with status {}: {}",
            script_path.display(),
            status,
            stderr.trim()
        )));
    }

    let stdout = String::from_utf8(stdout_bytes).map_err(|err| {
        SkillError::ScriptInvalidOutput(format!(
            "script '{}' emitted non-utf8 stdout: {err}",
            script_path.display()
        ))
    })?;
    let parsed = serde_json::from_str::<serde_json::Value>(stdout.trim()).map_err(|err| {
        SkillError::ScriptInvalidOutput(format!(
            "script '{}' emitted invalid JSON: {err}; stderr='{}'",
            script_path.display(),
            stderr.trim()
        ))
    })?;

    tracing::info!(
        agent_id = %agent_id,
        task_id = %task_id,
        tool_id = %tool_id,
        script_path = %script_path.display(),
        duration_ms = started_at.elapsed().as_millis(),
        status = %status,
        stdout_bytes = stdout.len(),
        stderr_bytes = stderr.len(),
        "skill_script_finished"
    );

    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{}_{}", prefix, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn returns_json() {
        if !python3_available() {
            return;
        }

        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("run.py");
        fs::write(
            &script,
            r#"import json, sys
payload = json.loads(sys.stdin.read())
print(json.dumps({"ok": True, "skill": payload.get("skill")}))
"#,
        )
        .expect("script write");

        let payload = serde_json::json!({
            "skill": "manage-bills",
            "arguments": {}
        });
        let value = run_skill_script(
            &script,
            &payload,
            Duration::from_secs(1),
            "finance-manager",
            "task-1",
            "manage-bills",
        )
        .await
        .expect("script should succeed");
        assert_eq!(value["ok"], serde_json::json!(true));
        assert_eq!(value["skill"], serde_json::json!("manage-bills"));
    }

    #[tokio::test]
    async fn times_out() {
        if !python3_available() {
            return;
        }

        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("slow.py");
        fs::write(
            &script,
            r#"import time
time.sleep(1.0)
print("{}")
"#,
        )
        .expect("script write");

        let err = run_skill_script(
            &script,
            &serde_json::json!({}),
            Duration::from_millis(10),
            "finance-manager",
            "task-1",
            "manage-bills",
        )
        .await
        .expect_err("script should time out");
        assert!(
            err.to_string().contains("script_timeout"),
            "unexpected timeout error: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_invalid_json() {
        if !python3_available() {
            return;
        }

        let skill_root = temp_dir("nm_skill_root");
        let script_dir = skill_root.join("scripts");
        fs::create_dir_all(&script_dir).expect("script dir");
        let script = script_dir.join("invalid.py");
        fs::write(&script, r#"print("not-json")"#).expect("script write");

        let err = run_skill_script(
            &script,
            &serde_json::json!({}),
            Duration::from_secs(1),
            "finance-manager",
            "task-1",
            "manage-bills",
        )
        .await
        .expect_err("script output should fail JSON parsing");
        assert!(
            err.to_string().contains("script_invalid_json"),
            "unexpected parse error: {err}"
        );
    }
}
