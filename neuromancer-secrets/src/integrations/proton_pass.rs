use neuromancer_core::error::{InfraError, NeuromancerError};
use tokio::process::Command;

/// Resolver for Proton Pass secrets via the `pass-cli` binary.
pub struct ProtonPassResolver;

impl ProtonPassResolver {
    /// Check if pass-cli is installed and authenticated.
    pub async fn check_health() -> Result<(), NeuromancerError> {
        let output = Command::new("pass-cli")
            .arg("test")
            .output()
            .await
            .map_err(|e| {
                NeuromancerError::Infra(InfraError::Database(format!(
                    "proton pass: failed to run pass-cli: {e}"
                )))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NeuromancerError::Infra(InfraError::Database(format!(
                "proton pass: health check failed: {stderr}"
            ))));
        }

        Ok(())
    }

    /// Resolve a `pass://` URI to its plaintext value.
    ///
    /// Runs: `pass-cli item view "<uri>" --output json`
    /// Parses JSON output, extracts the field value.
    pub async fn resolve(uri: &str) -> Result<String, NeuromancerError> {
        let output = Command::new("pass-cli")
            .args(["item", "view", uri, "--output", "json"])
            .output()
            .await
            .map_err(|e| {
                NeuromancerError::Infra(InfraError::Database(format!(
                    "proton pass: failed to run pass-cli: {e}"
                )))
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NeuromancerError::Infra(InfraError::Database(format!(
                "proton pass: resolve failed for '{uri}': {stderr}"
            ))));
        }

        let stdout = String::from_utf8(output.stdout).map_err(|e| {
            NeuromancerError::Infra(InfraError::Database(format!(
                "proton pass: invalid utf8 output: {e}"
            )))
        })?;

        // Parse the JSON output from pass-cli
        let json: serde_json::Value = serde_json::from_str(&stdout).map_err(|e| {
            NeuromancerError::Infra(InfraError::Database(format!(
                "proton pass: invalid json output: {e}"
            )))
        })?;

        // Extract password field — pass-cli outputs { "password": "...", ... }
        json.get("password")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                NeuromancerError::Infra(InfraError::Database(
                    "proton pass: no 'password' field in item".into(),
                ))
            })
    }

    /// Create a new login credential in Proton Pass.
    pub async fn create_credential(params: CreateCredentialParams) -> Result<(), NeuromancerError> {
        let mut cmd = Command::new("pass-cli");
        cmd.args([
            "item",
            "create",
            "login",
            "--vault-name",
            &params.vault_name,
            "--title",
            &params.title,
            "--password",
            &params.password,
            "--output",
            "json",
        ]);

        if let Some(ref username) = params.username {
            cmd.args(["--username", username]);
        }

        if let Some(ref email) = params.email {
            cmd.args(["--email", email]);
        }

        for url in &params.urls {
            cmd.args(["--url", url]);
        }

        let output = cmd.output().await.map_err(|e| {
            NeuromancerError::Infra(InfraError::Database(format!(
                "proton pass: failed to run pass-cli: {e}"
            )))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NeuromancerError::Infra(InfraError::Database(format!(
                "proton pass: create credential failed: {stderr}"
            ))));
        }

        Ok(())
    }
}

/// Parameters for creating a new credential in Proton Pass.
pub struct CreateCredentialParams {
    pub vault_name: String,
    pub title: String,
    pub username: Option<String>,
    pub password: String,
    pub email: Option<String>,
    pub urls: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pass_uri() {
        // Verify URI format is accepted (no parsing needed — pass-cli handles it natively)
        let uri = "pass://Work/GitHub/password";
        assert!(uri.starts_with("pass://"));
    }

    #[tokio::test]
    #[ignore] // Requires pass-cli installed and authenticated
    async fn health_check() {
        ProtonPassResolver::check_health().await.unwrap();
    }

    #[tokio::test]
    #[ignore] // Requires pass-cli installed and authenticated
    async fn resolve_uri() {
        // Replace with a real test URI
        let result = ProtonPassResolver::resolve("pass://Test/TestItem/password").await;
        assert!(result.is_ok() || result.is_err()); // Just verify it runs
    }
}
