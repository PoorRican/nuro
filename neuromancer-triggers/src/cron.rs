use chrono::Utc;
use minijinja::Environment;
use tokio::sync::mpsc;
use tokio_cron_scheduler::{Job, JobScheduler};
use tracing::{error, info, instrument, warn};

use neuromancer_core::agent::AgentId;
use neuromancer_core::config::CronTriggerConfig;
use neuromancer_core::trigger::{
    Actor, TriggerEvent, TriggerMetadata, TriggerPayload, TriggerSource, TriggerType,
};

/// Manages all cron trigger jobs.
pub struct CronTrigger {
    configs: Vec<CronTriggerConfig>,
    scheduler: Option<JobScheduler>,
}

impl CronTrigger {
    pub fn new(configs: Vec<CronTriggerConfig>) -> Self {
        Self {
            configs,
            scheduler: None,
        }
    }

    /// Start all enabled cron jobs and send [`TriggerEvents`] to the provided channel.
    #[instrument(skip(self, tx))]
    pub async fn start(&mut self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), CronError> {
        let scheduler = JobScheduler::new()
            .await
            .map_err(|e| CronError::SchedulerInit(e.to_string()))?;

        for config in &self.configs {
            if !config.enabled {
                info!(job_id = %config.id, "cron job disabled, skipping");
                continue;
            }

            let job = build_cron_job(config, tx.clone())?;
            scheduler
                .add(job)
                .await
                .map_err(|e| CronError::JobAdd(config.id.clone(), e.to_string()))?;
            info!(job_id = %config.id, schedule = %config.schedule, "cron job scheduled");
        }

        scheduler
            .start()
            .await
            .map_err(|e| CronError::SchedulerStart(e.to_string()))?;

        self.scheduler = Some(scheduler);
        Ok(())
    }

    /// Gracefully shut down the cron scheduler.
    pub async fn shutdown(&mut self) {
        if let Some(mut scheduler) = self.scheduler.take() {
            if let Err(e) = scheduler.shutdown().await {
                error!(error = %e, "error shutting down cron scheduler");
            }
        }
    }
}

/// Build a single cron Job from config.
fn build_cron_job(
    config: &CronTriggerConfig,
    tx: mpsc::Sender<TriggerEvent>,
) -> Result<Job, CronError> {
    let job_id = config.id.clone();
    let agent: AgentId = config.task_template.agent.clone();
    let instruction_template = config.task_template.instruction.clone();
    let parameters = config.task_template.parameters.clone();
    let idempotency_key_template = config.execution.idempotency_key.clone();
    let schedule = config.schedule.clone();

    let job = Job::new_async(schedule.as_str(), move |_uuid, _lock| {
        let tx = tx.clone();
        let job_id = job_id.clone();
        let agent = agent.clone();
        let instruction_template = instruction_template.clone();
        let parameters = parameters.clone();
        let idempotency_key_template = idempotency_key_template.clone();

        Box::pin(async move {
            let now = Utc::now();

            // Render instruction template.
            let rendered_instruction =
                match render_template(&instruction_template, &parameters, &job_id) {
                    Ok(rendered) => rendered,
                    Err(e) => {
                        error!(job_id = %job_id, error = %e, "failed to render cron instruction template");
                        return;
                    }
                };

            // Compute idempotency key if configured.
            let _idempotency_key = idempotency_key_template.as_ref().and_then(|tmpl| {
                render_template(tmpl, &parameters, &job_id).ok()
            });

            let trigger_id = format!(
                "cron-{}-{}",
                job_id,
                now.format("%Y%m%d%H%M%S")
            );

            let event = TriggerEvent {
                trigger_id,
                occurred_at: now,
                actor: Actor::CronJob {
                    job_id: job_id.clone(),
                },
                trigger_type: TriggerType::Internal,
                source: TriggerSource::Cron,
                payload: TriggerPayload::CronFire {
                    job_id: job_id.clone(),
                    rendered_instruction,
                    parameters,
                },
                route_hint: Some(agent),
                metadata: TriggerMetadata::default(),
            };

            if let Err(e) = tx.send(event).await {
                warn!(job_id = %job_id, error = %e, "failed to send cron trigger event");
            } else {
                info!(job_id = %job_id, "cron trigger fired");
            }
        })
    })
    .map_err(|e| CronError::JobCreate(config.id.clone(), e.to_string()))?;

    Ok(job)
}

/// Render a minijinja template with cron parameters plus built-in variables.
fn render_template(
    template_str: &str,
    parameters: &serde_json::Value,
    job_id: &str,
) -> Result<String, CronError> {
    let mut env = Environment::new();
    env.add_template("tmpl", template_str)
        .map_err(|e| CronError::TemplateRender(e.to_string()))?;

    let tmpl = env
        .get_template("tmpl")
        .map_err(|e| CronError::TemplateRender(e.to_string()))?;

    let now = Utc::now();
    let mut ctx = serde_json::Map::new();

    // Inject parameters.
    if let serde_json::Value::Object(map) = parameters {
        for (k, v) in map {
            ctx.insert(k.clone(), v.clone());
        }
    }

    // Built-in variables.
    ctx.insert(
        "date".to_string(),
        serde_json::Value::String(now.format("%Y-%m-%d").to_string()),
    );
    ctx.insert(
        "datetime".to_string(),
        serde_json::Value::String(now.to_rfc3339()),
    );
    ctx.insert(
        "job_id".to_string(),
        serde_json::Value::String(job_id.to_string()),
    );

    tmpl.render(serde_json::Value::Object(ctx))
        .map_err(|e| CronError::TemplateRender(e.to_string()))
}

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("scheduler init failed: {0}")]
    SchedulerInit(String),
    #[error("scheduler start failed: {0}")]
    SchedulerStart(String),
    #[error("job create failed for {0}: {1}")]
    JobCreate(String, String),
    #[error("job add failed for {0}: {1}")]
    JobAdd(String, String),
    #[error("template render failed: {0}")]
    TemplateRender(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_simple_template() {
        let params = serde_json::json!({
            "repos": ["org/repo-a", "org/repo-b"]
        });
        let result = render_template(
            "Summarize issues for repos: {{repos}}.",
            &params,
            "test-job",
        )
        .unwrap();
        assert!(result.contains("org/repo-a"));
        assert!(result.contains("org/repo-b"));
    }

    #[test]
    fn render_builtin_variables() {
        let params = serde_json::json!({});
        let result = render_template("Job {{job_id}} on {{date}}", &params, "my-job").unwrap();
        assert!(result.contains("my-job"));
        // Date should be YYYY-MM-DD format.
        assert!(result.contains(&Utc::now().format("%Y-%m-%d").to_string()));
    }

    #[test]
    fn render_idempotency_key() {
        let params = serde_json::json!({});
        let result =
            render_template("issue-summary-{{date}}", &params, "daily-issue-summary").unwrap();
        let expected = format!("issue-summary-{}", Utc::now().format("%Y-%m-%d"));
        assert_eq!(result, expected);
    }
}
