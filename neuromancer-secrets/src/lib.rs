use std::collections::HashMap;

use async_trait::async_trait;
use secrecy::SecretString;
use sqlx::SqlitePool;
use tracing::{info, instrument, warn};

use neuromancer_core::config::SecretEntryConfig;
use neuromancer_core::error::{NeuromancerError, PolicyError};
use neuromancer_core::secrets::{
    ResolvedSecret, SecretAcl, SecretInjectionMode, SecretKind, SecretRef, SecretUsage,
    SecretsBroker,
};
use neuromancer_core::tool::AgentContext;

mod crypto;
pub mod integrations;
pub mod keychain;
pub mod local;
pub mod scanner;

pub use local::LocalSecretsBroker;
pub use scanner::{SecretScanner, SharedScanner, new_shared_scanner, rebuild_scanner};

/// Composite secrets broker that dispatches between local SQLite storage
/// and Proton Pass CLI based on per-entry config.
pub struct CompositeSecretsBroker {
    local: LocalSecretsBroker,
    /// Secret entries from TOML config, keyed by secret ID.
    entries: HashMap<String, SecretEntryConfig>,
    /// Shared scanner instance for exfiltration detection.
    scanner: SharedScanner,
}

impl CompositeSecretsBroker {
    /// Create a new composite broker, sync TOML entries to SQLite, and build initial scanner.
    pub async fn new(
        pool: SqlitePool,
        master_key: SecretString,
        entries: HashMap<String, SecretEntryConfig>,
    ) -> Result<Self, NeuromancerError> {
        let local = LocalSecretsBroker::new(pool, master_key).await?;
        let scanner = new_shared_scanner();

        let broker = Self {
            local,
            entries,
            scanner,
        };

        // Sync TOML entries to SQLite (ACLs only)
        broker.sync_entries_to_sqlite().await?;

        // Build initial scanner
        broker.build_scanner().await?;

        Ok(broker)
    }

    /// Sync TOML entry configs to SQLite as ACL metadata.
    async fn sync_entries_to_sqlite(&self) -> Result<(), NeuromancerError> {
        for (id, entry) in &self.entries {
            // Warn about env_var injection mode
            for mode in &entry.allowed_injection_modes {
                if matches!(mode, SecretInjectionMode::EnvVar { .. }) {
                    warn!(
                        secret.id = %id,
                        "secret uses env_var injection — consider switching to handle_replacement"
                    );
                }
            }

            let expires_at = entry
                .ttl
                .as_ref()
                .and_then(|ttl| ttl.parse::<humantime::Duration>().ok())
                .map(|d| chrono::Utc::now() + chrono::Duration::from_std(*d).unwrap_or_default());

            let acl = SecretAcl {
                secret_id: id.clone(),
                kind: entry.kind.clone(),
                allowed_agents: entry.allowed_agents.clone(),
                allowed_skills: entry.allowed_skills.clone(),
                allowed_mcp_servers: entry.allowed_mcp_servers.clone(),
                injection_modes: if entry.allowed_injection_modes.is_empty() {
                    vec![SecretInjectionMode::HandleReplacement]
                } else {
                    entry.allowed_injection_modes.clone()
                },
                expires_at,
            };

            // TODO(secrets): persist parsed TOTP metadata (policy/params) in SQLite once
            // runtime TOTP integration is implemented end-to-end.
            if entry.kind == SecretKind::TotpSeed {
                let _ = entry.totp_policy.clone();
                let _ = entry.effective_totp_params();
            }

            self.local.upsert_acl(&acl).await?;
        }

        Ok(())
    }

    /// Resolve all secret values and rebuild the scanner atomically.
    pub async fn build_scanner(&self) -> Result<(), NeuromancerError> {
        let mut pairs = self.local.all_plaintext_pairs().await?;

        // Also resolve pass:// entries for scanner coverage
        for (id, entry) in &self.entries {
            if let Some(ref source) = entry.source
                && source.starts_with("pass://")
            {
                match integrations::proton_pass::ProtonPassResolver::resolve(source).await {
                    Ok(value) => pairs.push((id.clone(), value)),
                    Err(e) => {
                        warn!(
                            secret.id = %id,
                            error = %e,
                            "failed to resolve pass:// secret for scanner — skipping"
                        );
                    }
                }
            }
        }

        rebuild_scanner(&self.scanner, &pairs);
        Ok(())
    }

    /// Return the shared scanner for other crates to use at chokepoints.
    pub fn scanner(&self) -> SharedScanner {
        self.scanner.clone()
    }

    /// Create a credential in Proton Pass.
    pub async fn create_proton_credential(
        params: integrations::proton_pass::CreateCredentialParams,
    ) -> Result<(), NeuromancerError> {
        integrations::proton_pass::ProtonPassResolver::create_credential(params).await
    }

    /// Check if a secret entry is configured as pass:// sourced.
    fn is_pass_sourced(&self, id: &str) -> Option<&str> {
        self.entries
            .get(id)
            .and_then(|e| e.source.as_deref())
            .filter(|s| s.starts_with("pass://"))
    }

    /// Check ACL from TOML entries config.
    fn check_entry_acl(&self, id: &str, agent_id: &str) -> Result<(), NeuromancerError> {
        if let Some(entry) = self.entries.get(id)
            && !entry.allowed_agents.is_empty()
            && !entry.allowed_agents.contains(&agent_id.to_string())
        {
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: agent_id.to_string(),
                secret_ref: id.to_string(),
            }));
        }
        Ok(())
    }

    /// Check optional skill-level ACL from TOML entries config.
    fn check_entry_skill_scope(
        &self,
        id: &str,
        agent_id: &str,
        tool_id: &str,
    ) -> Result<(), NeuromancerError> {
        if let Some(entry) = self.entries.get(id)
            && !entry.allowed_skills.is_empty()
            && !entry.allowed_skills.contains(&tool_id.to_string())
        {
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: agent_id.to_string(),
                secret_ref: id.to_string(),
            }));
        }
        Ok(())
    }
}

#[async_trait]
impl SecretsBroker for CompositeSecretsBroker {
    #[instrument(skip(self, ctx), fields(
        secret.id = %secret_ref,
        agent.id = %ctx.agent_id,
        task.id = %ctx.task_id,
    ))]
    async fn resolve_handle_for_tool(
        &self,
        ctx: &AgentContext,
        secret_ref: SecretRef,
        usage: SecretUsage,
    ) -> Result<ResolvedSecret, NeuromancerError> {
        // Check TOML-level ACL first
        self.check_entry_acl(&secret_ref, &ctx.agent_id)?;
        self.check_entry_skill_scope(&secret_ref, &ctx.agent_id, &usage.tool_id)?;

        // If this is a pass:// sourced secret, resolve via Proton Pass
        if let Some(uri) = self.is_pass_sourced(&secret_ref) {
            // Must still be in agent context allowlist
            if !ctx.allowed_secrets.contains(&secret_ref) {
                return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                    agent_id: ctx.agent_id.clone(),
                    secret_ref,
                }));
            }

            let value = integrations::proton_pass::ProtonPassResolver::resolve(uri).await?;

            let injection_mode = self
                .entries
                .get(&secret_ref)
                .and_then(|e| e.allowed_injection_modes.first().cloned())
                .unwrap_or(SecretInjectionMode::HandleReplacement);

            info!(
                secret.outcome = "granted",
                secret.kind = "credential",
                tool.id = %usage.tool_id,
                secret.reason = %usage.purpose,
                "pass:// secret resolved"
            );

            return Ok(ResolvedSecret {
                value,
                injection_mode,
            });
        }

        // Delegate to local broker
        self.local
            .resolve_handle_for_tool(ctx, secret_ref, usage)
            .await
    }

    #[instrument(skip(self, ctx), fields(agent.id = %ctx.agent_id))]
    async fn list_handles(&self, ctx: &AgentContext) -> Result<Vec<String>, NeuromancerError> {
        let mut handles = self.local.list_handles(ctx).await?;

        // Add pass:// entries that this agent can access
        for (id, entry) in &self.entries {
            if entry
                .source
                .as_ref()
                .is_some_and(|s| s.starts_with("pass://"))
                && entry.allowed_agents.contains(&ctx.agent_id)
                && (ctx.allowed_secrets.is_empty() || ctx.allowed_secrets.contains(id))
                && !handles.contains(id)
            {
                handles.push(id.clone());
            }
        }

        Ok(handles)
    }

    async fn store(&self, id: &str, value: &str, acl: SecretAcl) -> Result<(), NeuromancerError> {
        self.local.store(id, value, acl).await?;
        if let Err(err) = self.build_scanner().await {
            warn!(secret.id = %id, operation = "store", error = %err, "scanner rebuild failed");
            return Err(err);
        }
        Ok(())
    }

    async fn revoke(&self, id: &str) -> Result<(), NeuromancerError> {
        self.local.revoke(id).await?;
        if let Err(err) = self.build_scanner().await {
            warn!(secret.id = %id, operation = "revoke", error = %err, "scanner rebuild failed");
            return Err(err);
        }
        Ok(())
    }

    #[instrument(skip(self, ctx, value), fields(
        secret.id = %id,
        agent.id = %ctx.agent_id,
    ))]
    async fn store_session(
        &self,
        ctx: &AgentContext,
        id: &str,
        value: &str,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), NeuromancerError> {
        self.local.store_session(ctx, id, value, expires_at).await?;

        // Rebuild scanner to include the new session value
        if let Err(err) = self.build_scanner().await {
            warn!(
                secret.id = %id,
                operation = "store_session",
                error = %err,
                "scanner rebuild failed"
            );
            return Err(err);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::secrets::SecretKind;

    async fn test_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite")
    }

    fn test_master_key() -> SecretString {
        SecretString::from("test-master-key-0123456789abcdef")
    }

    fn test_ctx_with_caps(
        agent_id: &str,
        allowed_secrets: Vec<&str>,
        allowed_mcp_servers: Vec<&str>,
    ) -> AgentContext {
        AgentContext {
            agent_id: agent_id.to_string(),
            task_id: uuid::Uuid::new_v4(),
            allowed_tools: vec![],
            allowed_mcp_servers: allowed_mcp_servers
                .into_iter()
                .map(str::to_string)
                .collect(),
            allowed_peer_agents: vec![],
            allowed_secrets: allowed_secrets.into_iter().map(str::to_string).collect(),
            allowed_memory_partitions: vec![],
        }
    }

    #[tokio::test]
    async fn composite_broker_local_secret_round_trip() {
        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), HashMap::new())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "test-key".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };

        broker.store("test-key", "secret-val", acl).await.unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["test-key"], vec![]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };
        let resolved = broker
            .resolve_handle_for_tool(&ctx, "test-key".into(), usage)
            .await
            .unwrap();
        assert_eq!(resolved.value, "secret-val");
    }

    #[tokio::test]
    async fn composite_broker_acl_enforcement() {
        let mut entries = HashMap::new();
        entries.insert(
            "restricted".into(),
            SecretEntryConfig {
                kind: SecretKind::Credential,
                source: None,
                allowed_agents: vec!["browser".into()],
                allowed_skills: vec![],
                allowed_mcp_servers: vec![],
                allowed_injection_modes: vec![],
                totp_policy: None,
                totp_digits: None,
                totp_period: None,
                totp_algorithm: None,
                ttl: None,
                rotation_metadata: None,
            },
        );

        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), entries)
            .await
            .unwrap();

        // Try to resolve as unauthorized agent
        let ctx = test_ctx_with_caps("planner", vec!["restricted"], vec![]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };
        let err = broker
            .resolve_handle_for_tool(&ctx, "restricted".into(), usage)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn composite_broker_list_handles_filtering() {
        let mut entries = HashMap::new();
        entries.insert(
            "pass-secret".into(),
            SecretEntryConfig {
                kind: SecretKind::Credential,
                source: Some("pass://Work/Test/password".into()),
                allowed_agents: vec!["browser".into()],
                allowed_skills: vec![],
                allowed_mcp_servers: vec![],
                allowed_injection_modes: vec![],
                totp_policy: None,
                totp_digits: None,
                totp_period: None,
                totp_algorithm: None,
                ttl: None,
                rotation_metadata: None,
            },
        );

        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), entries)
            .await
            .unwrap();

        // Store a local secret
        let acl = SecretAcl {
            secret_id: "local-key".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };
        broker.store("local-key", "val", acl).await.unwrap();

        let ctx = test_ctx_with_caps("browser", vec![], vec![]);
        let handles = broker.list_handles(&ctx).await.unwrap();
        // Should include both local and pass:// entries for browser agent
        assert!(handles.contains(&"local-key".to_string()));
        assert!(handles.contains(&"pass-secret".to_string()));

        // Planner should see neither
        let planner_ctx = test_ctx_with_caps("planner", vec![], vec![]);
        let handles = broker.list_handles(&planner_ctx).await.unwrap();
        assert!(!handles.contains(&"pass-secret".to_string()));
    }

    #[tokio::test]
    async fn composite_broker_scanner_wiring() {
        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), HashMap::new())
            .await
            .unwrap();

        // Store a secret
        let acl = SecretAcl {
            secret_id: "scannable".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };
        broker
            .store("scannable", "my-secret-api-token-xyz", acl)
            .await
            .unwrap();

        // Rebuild scanner
        broker.build_scanner().await.unwrap();

        let scanner = broker.scanner();
        let found = scanner
            .load()
            .scan("response contained my-secret-api-token-xyz in output");
        assert_eq!(found, vec!["scannable"]);
    }

    #[tokio::test]
    async fn composite_broker_store_rebuilds_scanner() {
        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), HashMap::new())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "stored-secret".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };
        broker
            .store("stored-secret", "stored-secret-value-123", acl)
            .await
            .unwrap();

        let scanner = broker.scanner();
        let found = scanner.load().scan("stored-secret-value-123");
        assert_eq!(found, vec!["stored-secret"]);
    }

    #[tokio::test]
    async fn composite_broker_revoke_rebuilds_scanner() {
        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), HashMap::new())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "revoked-secret".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };
        broker
            .store("revoked-secret", "revoked-secret-value-123", acl)
            .await
            .unwrap();
        broker.revoke("revoked-secret").await.unwrap();

        let scanner = broker.scanner();
        let found = scanner.load().scan("revoked-secret-value-123");
        assert!(found.is_empty());
    }

    #[tokio::test]
    async fn composite_broker_pass_secret_honors_allowed_skills() {
        let mut entries = HashMap::new();
        entries.insert(
            "pass-secret".into(),
            SecretEntryConfig {
                kind: SecretKind::Credential,
                source: Some("pass://Work/Test/password".into()),
                allowed_agents: vec!["browser".into()],
                allowed_skills: vec!["allowed-tool".into()],
                allowed_mcp_servers: vec![],
                allowed_injection_modes: vec![],
                totp_policy: None,
                totp_digits: None,
                totp_period: None,
                totp_algorithm: None,
                ttl: None,
                rotation_metadata: None,
            },
        );

        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), entries)
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["pass-secret"], vec![]);
        let usage = SecretUsage {
            tool_id: "forbidden-tool".into(),
            purpose: "test".into(),
        };
        let err = broker
            .resolve_handle_for_tool(&ctx, "pass-secret".into(), usage)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn composite_broker_session_write_back() {
        let pool = test_pool().await;
        let broker = CompositeSecretsBroker::new(pool, test_master_key(), HashMap::new())
            .await
            .unwrap();

        // Create browser session entry
        let acl = SecretAcl {
            secret_id: "session".into(),
            kind: SecretKind::BrowserSession,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };
        broker.store("session", "initial", acl).await.unwrap();

        // Write back via composite broker
        let ctx = test_ctx_with_caps("browser", vec!["session"], vec![]);
        broker
            .store_session(&ctx, "session", "updated-cookies", None)
            .await
            .unwrap();

        // Resolve to verify
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };
        let resolved = broker
            .resolve_handle_for_tool(&ctx, "session".into(), usage)
            .await
            .unwrap();
        assert_eq!(resolved.value, "updated-cookies");
    }
}
