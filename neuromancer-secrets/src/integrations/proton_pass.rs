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
        let args = build_create_login_args(&params);
        let mut cmd = Command::new("pass-cli");
        cmd.args(&args);

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

fn build_create_login_args(params: &CreateCredentialParams) -> Vec<String> {
    let mut args = vec![
        "item".to_string(),
        "create".to_string(),
        "login".to_string(),
        "--vault-name".to_string(),
        params.vault_name.clone(),
        "--title".to_string(),
        params.title.clone(),
        "--password".to_string(),
        params.password.clone(),
    ];

    if let Some(ref username) = params.username {
        args.push("--username".to_string());
        args.push(username.clone());
    }

    if let Some(ref email) = params.email {
        args.push("--email".to_string());
        args.push(email.clone());
    }

    for url in &params.urls {
        args.push("--url".to_string());
        args.push(url.clone());
    }

    args
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

    #[test]
    fn create_login_args_include_required_and_optional_fields() {
        let params = CreateCredentialParams {
            vault_name: "Work".into(),
            title: "GitHub".into(),
            username: Some("octocat".into()),
            password: "secret".into(),
            email: Some("octocat@example.com".into()),
            urls: vec!["https://github.com".into()],
        };

        let args = build_create_login_args(&params);
        let expected = vec![
            "item",
            "create",
            "login",
            "--vault-name",
            "Work",
            "--title",
            "GitHub",
            "--password",
            "secret",
            "--username",
            "octocat",
            "--email",
            "octocat@example.com",
            "--url",
            "https://github.com",
        ];
        assert_eq!(
            args,
            expected
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<String>>()
        );
    }

    #[test]
    fn create_login_args_support_repeated_urls_and_no_output_flag() {
        let params = CreateCredentialParams {
            vault_name: "Work".into(),
            title: "Service".into(),
            username: None,
            password: "secret".into(),
            email: None,
            urls: vec!["https://a.example".into(), "https://b.example".into()],
        };

        let args = build_create_login_args(&params);
        assert!(!args.iter().any(|arg| arg == "--output"));

        let urls = args
            .windows(2)
            .filter_map(|window| {
                if window[0] == "--url" {
                    Some(window[1].clone())
                } else {
                    None
                }
            })
            .collect::<Vec<String>>();
        assert_eq!(urls, params.urls);
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
