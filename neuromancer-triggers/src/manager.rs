use tokio::sync::mpsc;
use tracing::info;

use neuromancer_core::config::TriggersConfig;
use neuromancer_core::trigger::TriggerEvent;

use crate::cron::CronTrigger;
use crate::discord::DiscordTrigger;

/// The TriggerManager owns all trigger sources and sends TriggerEvents to the orchestrator.
pub struct TriggerManager {
    cron: Option<CronTrigger>,
    discord: Option<DiscordTrigger>,
    tx: mpsc::Sender<TriggerEvent>,
}

impl TriggerManager {
    /// Create a new TriggerManager from config. The `tx` sender delivers events to the orchestrator.
    pub fn new(config: &TriggersConfig, tx: mpsc::Sender<TriggerEvent>) -> Self {
        let cron = if config.cron.is_empty() {
            None
        } else {
            Some(CronTrigger::new(config.cron.clone()))
        };

        let discord = config
            .discord
            .as_ref()
            .filter(|d| d.enabled)
            .map(|d| DiscordTrigger::new(d.clone()));

        Self { cron, discord, tx }
    }

    /// Start all trigger sources. Spawns background tasks.
    pub async fn start(&mut self) -> Result<(), TriggerManagerError> {
        if let Some(ref mut cron) = self.cron {
            cron.start(self.tx.clone())
                .await
                .map_err(TriggerManagerError::Cron)?;
            info!("cron trigger manager started");
        }

        if let Some(ref mut discord) = self.discord {
            discord
                .start(self.tx.clone())
                .await
                .map_err(TriggerManagerError::Discord)?;
            info!("discord trigger manager started");
        }

        Ok(())
    }

    /// Gracefully shut down all trigger sources.
    pub async fn shutdown(&mut self) {
        if let Some(ref mut cron) = self.cron {
            cron.shutdown().await;
            info!("cron trigger manager stopped");
        }

        if let Some(ref mut discord) = self.discord {
            discord.shutdown().await;
            info!("discord trigger manager stopped");
        }
    }

    /// Access the Discord trigger (e.g. to notify task start/finish for rate limiting).
    pub fn discord_mut(&mut self) -> Option<&mut DiscordTrigger> {
        self.discord.as_mut()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TriggerManagerError {
    #[error("cron trigger error: {0}")]
    Cron(#[from] crate::cron::CronError),
    #[error("discord trigger error: {0}")]
    Discord(#[from] crate::discord::DiscordError),
}
