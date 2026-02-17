use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use sqlx::SqlitePool;
use tracing::{info, instrument, warn};
use zeroize::Zeroize;

use neuromancer_core::error::{InfraError, NeuromancerError, PolicyError};
use neuromancer_core::secrets::{
    ResolvedSecret, SecretAcl, SecretInjectionMode, SecretKind, SecretRef, SecretUsage,
    SecretsBroker,
};
use neuromancer_core::tool::AgentContext;

use crate::crypto;

/// SQLite-backed local secrets broker with ACL enforcement and AES-256-GCM encryption.
pub struct LocalSecretsBroker {
    pool: SqlitePool,
    master_key: SecretString,
}

impl LocalSecretsBroker {
    /// Create a new broker and run migrations.
    pub async fn new(pool: SqlitePool, master_key: SecretString) -> Result<Self, NeuromancerError> {
        let broker = Self { pool, master_key };
        broker.migrate().await?;
        Ok(broker)
    }

    /// Access the underlying pool (for composite broker).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Access the master key (for composite broker scanner builds).
    pub fn master_key(&self) -> &SecretString {
        &self.master_key
    }

    async fn migrate(&self) -> Result<(), NeuromancerError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS secrets (
                id TEXT PRIMARY KEY,
                encrypted_value BLOB NOT NULL,
                nonce BLOB NOT NULL,
                kind TEXT NOT NULL DEFAULT 'credential',
                allowed_agents TEXT NOT NULL DEFAULT '[]',
                allowed_skills TEXT NOT NULL DEFAULT '[]',
                allowed_mcp_servers TEXT NOT NULL DEFAULT '[]',
                injection_modes TEXT NOT NULL DEFAULT '[]',
                expires_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        // Add columns if migrating from older schema (silently ignore if already exists)
        for stmt in [
            "ALTER TABLE secrets ADD COLUMN kind TEXT NOT NULL DEFAULT 'credential'",
            "ALTER TABLE secrets ADD COLUMN expires_at TEXT",
        ] {
            let _ = sqlx::query(stmt).execute(&self.pool).await;
        }

        Ok(())
    }

    fn check_acl(&self, ctx: &AgentContext, acl: &SecretAcl) -> Result<(), NeuromancerError> {
        if acl.allowed_agents.contains(&ctx.agent_id) {
            return Ok(());
        }
        Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
            agent_id: ctx.agent_id.clone(),
            secret_ref: acl.secret_id.clone(),
        }))
    }

    /// Resolve a local secret by ID, returning the decrypted value.
    /// Does NOT check ACLs — caller must verify access first.
    pub async fn resolve_plaintext(&self, id: &str) -> Result<String, NeuromancerError> {
        let row = sqlx::query_as::<_, SecretRow>(
            "SELECT id, encrypted_value, nonce, kind, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, expires_at FROM secrets WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let row = row.ok_or_else(|| {
            NeuromancerError::Infra(InfraError::Database(format!("secret '{id}' not found")))
        })?;

        let mut plaintext = crypto::decrypt(
            &row.encrypted_value,
            &row.nonce,
            self.master_key.expose_secret().as_bytes(),
        )
        .map_err(|e| {
            NeuromancerError::Infra(InfraError::Database(format!("decryption failed: {e}")))
        })?;

        let value = String::from_utf8(plaintext.clone()).map_err(|e| {
            NeuromancerError::Infra(InfraError::Database(format!("invalid utf8: {e}")))
        })?;

        plaintext.zeroize();
        Ok(value)
    }

    /// Get all locally stored secret IDs and their decrypted values.
    /// Used for building the scanner.
    pub async fn all_plaintext_pairs(&self) -> Result<Vec<(String, String)>, NeuromancerError> {
        let rows = sqlx::query_as::<_, SecretRow>(
            "SELECT id, encrypted_value, nonce, kind, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, expires_at FROM secrets",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let mut pairs = Vec::with_capacity(rows.len());
        for row in &rows {
            // Skip rows with empty encrypted_value (ACL-only entries from TOML sync)
            if row.encrypted_value.is_empty() {
                continue;
            }
            let mut plaintext = crypto::decrypt(
                &row.encrypted_value,
                &row.nonce,
                self.master_key.expose_secret().as_bytes(),
            )
            .map_err(|e| {
                NeuromancerError::Infra(InfraError::Database(format!("decryption failed: {e}")))
            })?;

            let value = String::from_utf8(plaintext.clone()).map_err(|e| {
                NeuromancerError::Infra(InfraError::Database(format!("invalid utf8: {e}")))
            })?;

            plaintext.zeroize();
            pairs.push((row.id.clone(), value));
        }

        Ok(pairs)
    }

    /// Upsert only ACL metadata for a secret (used during TOML→SQLite sync).
    /// Does not touch encrypted_value if the row already exists.
    pub async fn upsert_acl(&self, acl: &SecretAcl) -> Result<(), NeuromancerError> {
        let kind = serde_json::to_string(&acl.kind)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_agents = serde_json::to_string(&acl.allowed_agents)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_skills = serde_json::to_string(&acl.allowed_skills)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_mcp = serde_json::to_string(&acl.allowed_mcp_servers)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let injection_modes = serde_json::to_string(&acl.injection_modes)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let expires_at = acl.expires_at.map(|dt| dt.to_rfc3339());

        // Only update ACL fields if row exists; insert empty-valued row if not
        sqlx::query(
            r#"
            INSERT INTO secrets (id, encrypted_value, nonce, kind, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, expires_at, updated_at)
            VALUES (?, X'', X'', ?, ?, ?, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                kind = excluded.kind,
                allowed_agents = excluded.allowed_agents,
                allowed_skills = excluded.allowed_skills,
                allowed_mcp_servers = excluded.allowed_mcp_servers,
                injection_modes = excluded.injection_modes,
                expires_at = excluded.expires_at,
                updated_at = datetime('now')
            "#,
        )
        .bind(&acl.secret_id)
        .bind(&kind)
        .bind(&allowed_agents)
        .bind(&allowed_skills)
        .bind(&allowed_mcp)
        .bind(&injection_modes)
        .bind(&expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        Ok(())
    }
}

#[async_trait]
impl SecretsBroker for LocalSecretsBroker {
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
        if !ctx.allowed_secrets.contains(&secret_ref) {
            warn!(
                secret.outcome = "denied",
                "secret not in agent context allowlist"
            );
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref,
            }));
        }

        let row = sqlx::query_as::<_, SecretRow>(
            "SELECT id, encrypted_value, nonce, kind, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, expires_at FROM secrets WHERE id = ?",
        )
        .bind(&secret_ref)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let row = row.ok_or_else(|| {
            warn!(secret.outcome = "denied", "secret not found");
            NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: secret_ref.clone(),
            })
        })?;

        let acl = row.to_acl();

        // Check expiry
        if let Some(expires_at) = acl.expires_at
            && expires_at < chrono::Utc::now()
        {
            warn!(
                secret.outcome = "expired",
                secret.kind = ?acl.kind,
                "secret expired"
            );
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: acl.secret_id.clone(),
            }));
        }

        // Check ACL
        self.check_acl(ctx, &acl)?;

        if !acl.allowed_skills.is_empty() && !acl.allowed_skills.contains(&usage.tool_id) {
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: acl.secret_id.clone(),
            }));
        }

        if !ctx.allowed_mcp_servers.is_empty() && !ctx.allowed_mcp_servers.contains(&usage.tool_id)
        {
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: acl.secret_id.clone(),
            }));
        }

        if !acl.allowed_mcp_servers.is_empty() && !acl.allowed_mcp_servers.contains(&usage.tool_id)
        {
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: acl.secret_id.clone(),
            }));
        }

        // Decrypt value
        let mut plaintext = crypto::decrypt(
            &row.encrypted_value,
            &row.nonce,
            self.master_key.expose_secret().as_bytes(),
        )
        .map_err(|e| {
            NeuromancerError::Infra(InfraError::Database(format!("decryption failed: {e}")))
        })?;

        let value = String::from_utf8(plaintext.clone()).map_err(|e| {
            NeuromancerError::Infra(InfraError::Database(format!("invalid utf8: {e}")))
        })?;

        plaintext.zeroize();

        // Determine injection mode — default to HandleReplacement
        let injection_mode = acl
            .injection_modes
            .first()
            .cloned()
            .unwrap_or(SecretInjectionMode::HandleReplacement);

        info!(
            secret.outcome = "granted",
            secret.kind = ?acl.kind,
            tool.id = %usage.tool_id,
            secret.reason = %usage.purpose,
            "secret access granted"
        );

        Ok(ResolvedSecret {
            value,
            injection_mode,
        })
    }

    #[instrument(skip(self, ctx), fields(agent.id = %ctx.agent_id))]
    async fn list_handles(&self, ctx: &AgentContext) -> Result<Vec<String>, NeuromancerError> {
        let rows = sqlx::query_as::<_, SecretRow>(
            "SELECT id, encrypted_value, nonce, kind, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, expires_at FROM secrets",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let handles: Vec<String> = rows
            .iter()
            .filter(|row| {
                let acl = row.to_acl();
                acl.allowed_agents.contains(&ctx.agent_id)
                    && (ctx.allowed_secrets.is_empty() || ctx.allowed_secrets.contains(&row.id))
            })
            .map(|row| row.id.clone())
            .collect();

        Ok(handles)
    }

    #[instrument(skip(self, value), fields(secret.id = %id))]
    async fn store(&self, id: &str, value: &str, acl: SecretAcl) -> Result<(), NeuromancerError> {
        let (encrypted, nonce) =
            crypto::encrypt(value.as_bytes(), self.master_key.expose_secret().as_bytes()).map_err(
                |e| {
                    NeuromancerError::Infra(InfraError::Database(format!("encryption failed: {e}")))
                },
            )?;

        let kind = serde_json::to_string(&acl.kind)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_agents = serde_json::to_string(&acl.allowed_agents)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_skills = serde_json::to_string(&acl.allowed_skills)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_mcp = serde_json::to_string(&acl.allowed_mcp_servers)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let injection_modes = serde_json::to_string(&acl.injection_modes)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let expires_at = acl.expires_at.map(|dt| dt.to_rfc3339());

        sqlx::query(
            r#"
            INSERT INTO secrets (id, encrypted_value, nonce, kind, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, expires_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                encrypted_value = excluded.encrypted_value,
                nonce = excluded.nonce,
                kind = excluded.kind,
                allowed_agents = excluded.allowed_agents,
                allowed_skills = excluded.allowed_skills,
                allowed_mcp_servers = excluded.allowed_mcp_servers,
                injection_modes = excluded.injection_modes,
                expires_at = excluded.expires_at,
                updated_at = datetime('now')
            "#,
        )
        .bind(id)
        .bind(&encrypted)
        .bind(&nonce)
        .bind(&kind)
        .bind(&allowed_agents)
        .bind(&allowed_skills)
        .bind(&allowed_mcp)
        .bind(&injection_modes)
        .bind(&expires_at)
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        info!(secret.id = %id, "secret stored");
        Ok(())
    }

    #[instrument(skip(self), fields(secret.id = %id))]
    async fn revoke(&self, id: &str) -> Result<(), NeuromancerError> {
        let result = sqlx::query("DELETE FROM secrets WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        if result.rows_affected() == 0 {
            warn!(secret.id = %id, "secret not found for revocation");
        } else {
            info!(secret.id = %id, "secret revoked");
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
        // Look up existing entry to check ACL and kind
        let row = sqlx::query_as::<_, SecretRow>(
            "SELECT id, encrypted_value, nonce, kind, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, expires_at FROM secrets WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let row = row.ok_or_else(|| {
            NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: id.to_string(),
            })
        })?;

        let acl = row.to_acl();

        // Must be BrowserSession kind
        if acl.kind != SecretKind::BrowserSession {
            return Err(NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: id.to_string(),
            }));
        }

        // Agent must be in allowed_agents
        self.check_acl(ctx, &acl)?;

        // Encrypt new value
        let (encrypted, nonce) =
            crypto::encrypt(value.as_bytes(), self.master_key.expose_secret().as_bytes()).map_err(
                |e| {
                    NeuromancerError::Infra(InfraError::Database(format!("encryption failed: {e}")))
                },
            )?;

        let expires_str = expires_at.map(|dt| dt.to_rfc3339());

        sqlx::query(
            "UPDATE secrets SET encrypted_value = ?, nonce = ?, expires_at = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(&encrypted)
        .bind(&nonce)
        .bind(&expires_str)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        info!(
            secret.outcome = "granted",
            secret.kind = "browser_session",
            "session stored"
        );

        Ok(())
    }
}

/// Internal row representation for SQLite queries.
#[derive(Debug, sqlx::FromRow)]
struct SecretRow {
    id: String,
    encrypted_value: Vec<u8>,
    nonce: Vec<u8>,
    kind: String,
    allowed_agents: String,
    allowed_skills: String,
    allowed_mcp_servers: String,
    injection_modes: String,
    expires_at: Option<String>,
}

impl SecretRow {
    fn to_acl(&self) -> SecretAcl {
        let kind: SecretKind = serde_json::from_str(&self.kind).unwrap_or(SecretKind::Credential);
        let expires_at = self
            .expires_at
            .as_ref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));

        SecretAcl {
            secret_id: self.id.clone(),
            kind,
            allowed_agents: serde_json::from_str(&self.allowed_agents).unwrap_or_default(),
            allowed_skills: serde_json::from_str(&self.allowed_skills).unwrap_or_default(),
            allowed_mcp_servers: serde_json::from_str(&self.allowed_mcp_servers)
                .unwrap_or_default(),
            injection_modes: serde_json::from_str(&self.injection_modes).unwrap_or_default(),
            expires_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::secrets::SecretInjectionMode;

    async fn test_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite")
    }

    fn test_ctx(agent_id: &str) -> AgentContext {
        test_ctx_with_caps(agent_id, vec![], vec![])
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
            allowed_secrets: allowed_secrets.into_iter().map(str::to_string).collect(),
            allowed_memory_partitions: vec![],
        }
    }

    fn test_master_key() -> SecretString {
        SecretString::from("test-master-key-0123456789abcdef")
    }

    fn test_acl(id: &str, agents: Vec<&str>) -> SecretAcl {
        SecretAcl {
            secret_id: id.into(),
            kind: SecretKind::Credential,
            allowed_agents: agents.into_iter().map(str::to_string).collect(),
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn store_and_resolve_secret() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "my-api-key".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![SecretInjectionMode::EnvVar {
                name: "API_KEY".into(),
            }],
            expires_at: None,
        };

        broker
            .store("my-api-key", "super-secret-value", acl)
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["my-api-key"], vec!["http-client"]);
        let usage = SecretUsage {
            tool_id: "http-client".into(),
            purpose: "API call".into(),
        };

        let resolved = broker
            .resolve_handle_for_tool(&ctx, "my-api-key".into(), usage)
            .await
            .unwrap();

        assert_eq!(resolved.value, "super-secret-value");
        assert!(
            matches!(resolved.injection_mode, SecretInjectionMode::EnvVar { name } if name == "API_KEY")
        );
    }

    #[tokio::test]
    async fn acl_denies_unauthorized_agent() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        broker
            .store(
                "restricted",
                "secret-val",
                test_acl("restricted", vec!["browser"]),
            )
            .await
            .unwrap();

        let ctx = test_ctx("planner"); // not in allowed_agents
        let usage = SecretUsage {
            tool_id: "tool".into(),
            purpose: "test".into(),
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
    async fn resolve_requires_secret_in_agent_context_allowlist() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        broker
            .store(
                "context-guarded",
                "secret-val",
                test_acl("context-guarded", vec!["browser"]),
            )
            .await
            .unwrap();

        // Agent identity is allowed by ACL, but runtime context did not grant this secret.
        let ctx = test_ctx_with_caps("browser", vec![], vec!["http-client"]);
        let usage = SecretUsage {
            tool_id: "http-client".into(),
            purpose: "test".into(),
        };

        let err = broker
            .resolve_handle_for_tool(&ctx, "context-guarded".into(), usage)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn resolve_honors_secret_acl_allowed_mcp_servers() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "mcp-guarded".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec!["playwright".into()],
            injection_modes: vec![],
            expires_at: None,
        };

        broker
            .store("mcp-guarded", "secret-val", acl)
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["mcp-guarded"], vec!["filesystem"]);
        let usage = SecretUsage {
            tool_id: "filesystem".into(),
            purpose: "test".into(),
        };

        let err = broker
            .resolve_handle_for_tool(&ctx, "mcp-guarded".into(), usage)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn resolve_honors_secret_acl_allowed_skills_denied() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "skill-guarded".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec!["allowed-skill".into()],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };

        broker
            .store("skill-guarded", "secret-val", acl)
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["skill-guarded"], vec![]);
        let usage = SecretUsage {
            tool_id: "forbidden-skill".into(),
            purpose: "test".into(),
        };

        let err = broker
            .resolve_handle_for_tool(&ctx, "skill-guarded".into(), usage)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn resolve_honors_secret_acl_allowed_skills_allowed() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "skill-allowed".into(),
            kind: SecretKind::Credential,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec!["allowed-skill".into()],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };

        broker
            .store("skill-allowed", "secret-val", acl)
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["skill-allowed"], vec![]);
        let usage = SecretUsage {
            tool_id: "allowed-skill".into(),
            purpose: "test".into(),
        };

        let resolved = broker
            .resolve_handle_for_tool(&ctx, "skill-allowed".into(), usage)
            .await
            .unwrap();

        assert_eq!(resolved.value, "secret-val");
    }

    #[tokio::test]
    async fn list_handles_filters_by_agent() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        broker
            .store(
                "browser-key",
                "val1",
                test_acl("browser-key", vec!["browser"]),
            )
            .await
            .unwrap();
        broker
            .store(
                "planner-key",
                "val2",
                test_acl("planner-key", vec!["planner"]),
            )
            .await
            .unwrap();

        let browser_ctx = test_ctx("browser");
        let handles = broker.list_handles(&browser_ctx).await.unwrap();
        assert_eq!(handles, vec!["browser-key"]);

        let planner_ctx = test_ctx("planner");
        let handles = broker.list_handles(&planner_ctx).await.unwrap();
        assert_eq!(handles, vec!["planner-key"]);
    }

    #[tokio::test]
    async fn revoke_deletes_secret() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        broker
            .store("temp", "val", test_acl("temp", vec!["browser"]))
            .await
            .unwrap();
        broker.revoke("temp").await.unwrap();

        let ctx = test_ctx("browser");
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };

        let err = broker
            .resolve_handle_for_tool(&ctx, "temp".into(), usage)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn store_upserts_existing_secret() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let acl = test_acl("key", vec!["browser"]);

        broker.store("key", "old-val", acl.clone()).await.unwrap();
        broker.store("key", "new-val", acl).await.unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["key"], vec!["t"]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };
        let resolved = broker
            .resolve_handle_for_tool(&ctx, "key".into(), usage)
            .await
            .unwrap();
        assert_eq!(resolved.value, "new-val");
    }

    #[tokio::test]
    async fn default_injection_mode_is_handle_replacement() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        broker
            .store("no-mode", "val", test_acl("no-mode", vec!["browser"]))
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["no-mode"], vec![]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };
        let resolved = broker
            .resolve_handle_for_tool(&ctx, "no-mode".into(), usage)
            .await
            .unwrap();
        assert!(matches!(
            resolved.injection_mode,
            SecretInjectionMode::HandleReplacement
        ));
    }

    #[tokio::test]
    async fn store_session_requires_browser_session_kind() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        // Store a Credential-kind secret
        broker
            .store(
                "cred-secret",
                "initial",
                test_acl("cred-secret", vec!["browser"]),
            )
            .await
            .unwrap();

        let ctx = test_ctx("browser");
        let err = broker
            .store_session(&ctx, "cred-secret", "new-session", None)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn store_session_allowed_agent_succeeds() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let acl = SecretAcl {
            secret_id: "session".into(),
            kind: SecretKind::BrowserSession,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: None,
        };
        broker
            .store("session", "initial-session", acl)
            .await
            .unwrap();

        let ctx = test_ctx("browser");
        broker
            .store_session(&ctx, "session", "updated-session", None)
            .await
            .unwrap();

        // Verify the updated value
        let resolve_ctx = test_ctx_with_caps("browser", vec!["session"], vec![]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };
        let resolved = broker
            .resolve_handle_for_tool(&resolve_ctx, "session".into(), usage)
            .await
            .unwrap();
        assert_eq!(resolved.value, "updated-session");
    }

    #[tokio::test]
    async fn store_session_denied_agent_fails() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

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

        let ctx = test_ctx("planner"); // not allowed
        let err = broker
            .store_session(&ctx, "session", "evil-session", None)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn expired_session_returns_error() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        let acl = SecretAcl {
            secret_id: "expired-session".into(),
            kind: SecretKind::BrowserSession,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: Some(past),
        };
        broker
            .store("expired-session", "old-cookies", acl)
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["expired-session"], vec![]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };

        let err = broker
            .resolve_handle_for_tool(&ctx, "expired-session".into(), usage)
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })
        ));
    }

    #[tokio::test]
    async fn non_expired_session_succeeds() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        let future = chrono::Utc::now() + chrono::Duration::hours(24);
        let acl = SecretAcl {
            secret_id: "fresh-session".into(),
            kind: SecretKind::BrowserSession,
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
            expires_at: Some(future),
        };
        broker
            .store("fresh-session", "fresh-cookies", acl)
            .await
            .unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["fresh-session"], vec![]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };

        let resolved = broker
            .resolve_handle_for_tool(&ctx, "fresh-session".into(), usage)
            .await
            .unwrap();
        assert_eq!(resolved.value, "fresh-cookies");
    }

    #[tokio::test]
    async fn kind_round_trips_correctly() {
        let pool = test_pool().await;
        let broker = LocalSecretsBroker::new(pool, test_master_key())
            .await
            .unwrap();

        for kind in [
            SecretKind::Credential,
            SecretKind::TotpSeed,
            SecretKind::BrowserSession,
            SecretKind::CertificateOrKey,
        ] {
            let id = format!("kind-{kind:?}");
            let acl = SecretAcl {
                secret_id: id.clone(),
                kind: kind.clone(),
                allowed_agents: vec!["browser".into()],
                allowed_skills: vec![],
                allowed_mcp_servers: vec![],
                injection_modes: vec![],
                expires_at: None,
            };
            broker.store(&id, "val", acl).await.unwrap();

            // Read back and verify kind
            let row = sqlx::query_as::<_, (String,)>("SELECT kind FROM secrets WHERE id = ?")
                .bind(&id)
                .fetch_one(broker.pool())
                .await
                .unwrap();

            let stored_kind: SecretKind = serde_json::from_str(&row.0).unwrap();
            assert_eq!(stored_kind, kind);
        }
    }
}
