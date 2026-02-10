use async_trait::async_trait;
use secrecy::{ExposeSecret, SecretString};
use sqlx::SqlitePool;
use tracing::{info, instrument, warn};
use zeroize::Zeroize;

use neuromancer_core::error::{InfraError, NeuromancerError, PolicyError};
use neuromancer_core::secrets::{ResolvedSecret, SecretAcl, SecretInjectionMode, SecretRef, SecretUsage, SecretsBroker};
use neuromancer_core::tool::AgentContext;

mod crypto;

/// SQLite-backed secrets broker with ACL enforcement.
pub struct SqliteSecretsBroker {
    pool: SqlitePool,
    master_key: SecretString,
}

impl SqliteSecretsBroker {
    /// Create a new broker and run migrations.
    pub async fn new(pool: SqlitePool, master_key: SecretString) -> Result<Self, NeuromancerError> {
        let broker = Self { pool, master_key };
        broker.migrate().await?;
        Ok(broker)
    }

    async fn migrate(&self) -> Result<(), NeuromancerError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS secrets (
                id TEXT PRIMARY KEY,
                encrypted_value BLOB NOT NULL,
                nonce BLOB NOT NULL,
                allowed_agents TEXT NOT NULL DEFAULT '[]',
                allowed_skills TEXT NOT NULL DEFAULT '[]',
                allowed_mcp_servers TEXT NOT NULL DEFAULT '[]',
                injection_modes TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

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
}

#[async_trait]
impl SecretsBroker for SqliteSecretsBroker {
    #[instrument(skip(self, ctx), fields(agent_id = %ctx.agent_id, secret_ref = %secret_ref))]
    async fn resolve_handle_for_tool(
        &self,
        ctx: &AgentContext,
        secret_ref: SecretRef,
        usage: SecretUsage,
    ) -> Result<ResolvedSecret, NeuromancerError> {
        // Fetch the secret row
        let row = sqlx::query_as::<_, SecretRow>(
            "SELECT id, encrypted_value, nonce, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes FROM secrets WHERE id = ?",
        )
        .bind(&secret_ref)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let row = row.ok_or_else(|| {
            warn!(secret_ref = %secret_ref, "secret not found");
            NeuromancerError::Policy(PolicyError::SecretAccessDenied {
                agent_id: ctx.agent_id.clone(),
                secret_ref: secret_ref.clone(),
            })
        })?;

        let acl = row.to_acl();

        // Check ACL
        self.check_acl(ctx, &acl)?;

        // Decrypt value
        let mut plaintext = crypto::decrypt(
            &row.encrypted_value,
            &row.nonce,
            self.master_key.expose_secret().as_bytes(),
        )
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(format!("decryption failed: {e}"))))?;

        let value = String::from_utf8(plaintext.clone())
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(format!("invalid utf8: {e}"))))?;

        plaintext.zeroize();

        // Determine injection mode
        let injection_mode = acl
            .injection_modes
            .first()
            .cloned()
            .unwrap_or(SecretInjectionMode::EnvVar {
                name: secret_ref.clone(),
            });

        info!(
            secret_ref = %secret_ref,
            agent_id = %ctx.agent_id,
            tool_id = %usage.tool_id,
            "secret access granted"
        );

        Ok(ResolvedSecret {
            value,
            injection_mode,
        })
    }

    #[instrument(skip(self, ctx), fields(agent_id = %ctx.agent_id))]
    async fn list_handles(&self, ctx: &AgentContext) -> Result<Vec<String>, NeuromancerError> {
        let rows = sqlx::query_as::<_, SecretRow>(
            "SELECT id, encrypted_value, nonce, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes FROM secrets",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        let handles: Vec<String> = rows
            .iter()
            .filter(|row| {
                let acl = row.to_acl();
                acl.allowed_agents.contains(&ctx.agent_id)
            })
            .map(|row| row.id.clone())
            .collect();

        Ok(handles)
    }

    #[instrument(skip(self, value), fields(secret_id = %id))]
    async fn store(
        &self,
        id: &str,
        value: &str,
        acl: SecretAcl,
    ) -> Result<(), NeuromancerError> {
        let (encrypted, nonce) =
            crypto::encrypt(value.as_bytes(), self.master_key.expose_secret().as_bytes())
                .map_err(|e| NeuromancerError::Infra(InfraError::Database(format!("encryption failed: {e}"))))?;

        let allowed_agents = serde_json::to_string(&acl.allowed_agents)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_skills = serde_json::to_string(&acl.allowed_skills)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let allowed_mcp = serde_json::to_string(&acl.allowed_mcp_servers)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;
        let injection_modes = serde_json::to_string(&acl.injection_modes)
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        sqlx::query(
            r#"
            INSERT INTO secrets (id, encrypted_value, nonce, allowed_agents, allowed_skills, allowed_mcp_servers, injection_modes, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, datetime('now'))
            ON CONFLICT(id) DO UPDATE SET
                encrypted_value = excluded.encrypted_value,
                nonce = excluded.nonce,
                allowed_agents = excluded.allowed_agents,
                allowed_skills = excluded.allowed_skills,
                allowed_mcp_servers = excluded.allowed_mcp_servers,
                injection_modes = excluded.injection_modes,
                updated_at = datetime('now')
            "#,
        )
        .bind(id)
        .bind(&encrypted)
        .bind(&nonce)
        .bind(&allowed_agents)
        .bind(&allowed_skills)
        .bind(&allowed_mcp)
        .bind(&injection_modes)
        .execute(&self.pool)
        .await
        .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        info!(secret_id = %id, "secret stored");
        Ok(())
    }

    #[instrument(skip(self), fields(secret_id = %id))]
    async fn revoke(&self, id: &str) -> Result<(), NeuromancerError> {
        let result = sqlx::query("DELETE FROM secrets WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| NeuromancerError::Infra(InfraError::Database(e.to_string())))?;

        if result.rows_affected() == 0 {
            warn!(secret_id = %id, "secret not found for revocation");
        } else {
            info!(secret_id = %id, "secret revoked");
        }

        Ok(())
    }
}

/// Internal row representation for SQLite queries.
#[derive(Debug, sqlx::FromRow)]
struct SecretRow {
    id: String,
    encrypted_value: Vec<u8>,
    nonce: Vec<u8>,
    allowed_agents: String,
    allowed_skills: String,
    allowed_mcp_servers: String,
    injection_modes: String,
}

impl SecretRow {
    fn to_acl(&self) -> SecretAcl {
        SecretAcl {
            secret_id: self.id.clone(),
            allowed_agents: serde_json::from_str(&self.allowed_agents).unwrap_or_default(),
            allowed_skills: serde_json::from_str(&self.allowed_skills).unwrap_or_default(),
            allowed_mcp_servers: serde_json::from_str(&self.allowed_mcp_servers).unwrap_or_default(),
            injection_modes: serde_json::from_str(&self.injection_modes).unwrap_or_default(),
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

    #[tokio::test]
    async fn store_and_resolve_secret() {
        let pool = test_pool().await;
        let broker = SqliteSecretsBroker::new(pool, test_master_key()).await.unwrap();

        let acl = SecretAcl {
            secret_id: "my-api-key".into(),
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![SecretInjectionMode::EnvVar {
                name: "API_KEY".into(),
            }],
        };

        broker.store("my-api-key", "super-secret-value", acl).await.unwrap();

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
        assert!(matches!(resolved.injection_mode, SecretInjectionMode::EnvVar { name } if name == "API_KEY"));
    }

    #[tokio::test]
    async fn acl_denies_unauthorized_agent() {
        let pool = test_pool().await;
        let broker = SqliteSecretsBroker::new(pool, test_master_key()).await.unwrap();

        let acl = SecretAcl {
            secret_id: "restricted".into(),
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
        };

        broker.store("restricted", "secret-val", acl).await.unwrap();

        let ctx = test_ctx("planner"); // not in allowed_agents
        let usage = SecretUsage {
            tool_id: "tool".into(),
            purpose: "test".into(),
        };

        let err = broker
            .resolve_handle_for_tool(&ctx, "restricted".into(), usage)
            .await
            .unwrap_err();

        assert!(matches!(err, NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })));
    }

    #[tokio::test]
    async fn resolve_requires_secret_in_agent_context_allowlist() {
        let pool = test_pool().await;
        let broker = SqliteSecretsBroker::new(pool, test_master_key()).await.unwrap();

        broker
            .store(
                "context-guarded",
                "secret-val",
                SecretAcl {
                    secret_id: "context-guarded".into(),
                    allowed_agents: vec!["browser".into()],
                    allowed_skills: vec![],
                    allowed_mcp_servers: vec![],
                    injection_modes: vec![],
                },
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
        let broker = SqliteSecretsBroker::new(pool, test_master_key()).await.unwrap();

        broker
            .store(
                "mcp-guarded",
                "secret-val",
                SecretAcl {
                    secret_id: "mcp-guarded".into(),
                    allowed_agents: vec!["browser".into()],
                    allowed_skills: vec![],
                    allowed_mcp_servers: vec!["playwright".into()],
                    injection_modes: vec![],
                },
            )
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
    async fn list_handles_filters_by_agent() {
        let pool = test_pool().await;
        let broker = SqliteSecretsBroker::new(pool, test_master_key()).await.unwrap();

        // Secret accessible to browser
        broker
            .store(
                "browser-key",
                "val1",
                SecretAcl {
                    secret_id: "browser-key".into(),
                    allowed_agents: vec!["browser".into()],
                    allowed_skills: vec![],
                    allowed_mcp_servers: vec![],
                    injection_modes: vec![],
                },
            )
            .await
            .unwrap();

        // Secret accessible to planner
        broker
            .store(
                "planner-key",
                "val2",
                SecretAcl {
                    secret_id: "planner-key".into(),
                    allowed_agents: vec!["planner".into()],
                    allowed_skills: vec![],
                    allowed_mcp_servers: vec![],
                    injection_modes: vec![],
                },
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
        let broker = SqliteSecretsBroker::new(pool, test_master_key()).await.unwrap();

        let acl = SecretAcl {
            secret_id: "temp".into(),
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
        };

        broker.store("temp", "val", acl).await.unwrap();
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
        assert!(matches!(err, NeuromancerError::Policy(PolicyError::SecretAccessDenied { .. })));
    }

    #[tokio::test]
    async fn store_upserts_existing_secret() {
        let pool = test_pool().await;
        let broker = SqliteSecretsBroker::new(pool, test_master_key()).await.unwrap();

        let acl = SecretAcl {
            secret_id: "key".into(),
            allowed_agents: vec!["browser".into()],
            allowed_skills: vec![],
            allowed_mcp_servers: vec![],
            injection_modes: vec![],
        };

        broker.store("key", "old-val", acl.clone()).await.unwrap();
        broker.store("key", "new-val", acl).await.unwrap();

        let ctx = test_ctx_with_caps("browser", vec!["key"], vec!["t"]);
        let usage = SecretUsage {
            tool_id: "t".into(),
            purpose: "t".into(),
        };
        let resolved = broker.resolve_handle_for_tool(&ctx, "key".into(), usage).await.unwrap();
        assert_eq!(resolved.value, "new-val");
    }
}
