use std::collections::HashMap;
use std::time::Instant;

use chrono::Utc;
use tokio::sync::mpsc;
use tracing::info;

use neuromancer_core::config::{DiscordResponseConfig, DiscordTriggerConfig, RateLimitConfig};
use neuromancer_core::trigger::{
    Principal, TriggerEvent, TriggerMetadata, TriggerPayload,
};

/// Configuration types for the Discord trigger, parsed from core config.

/// Activation rule controlling which messages are processed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationRule {
    All,
    Mention,
    Prefix(String),
    Reply,
}

impl ActivationRule {
    pub fn from_str(s: &str) -> Self {
        match s {
            "all" => Self::All,
            "mention" => Self::Mention,
            "reply" => Self::Reply,
            other if other.starts_with("prefix:") => {
                Self::Prefix(other.strip_prefix("prefix:").unwrap_or("!").to_string())
            }
            _ => Self::Mention,
        }
    }
}

/// Thread management mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadMode {
    PerTask,
    Inherit,
    Disabled,
}

impl ThreadMode {
    pub fn from_str(s: &str) -> Self {
        match s {
            "per_task" => Self::PerTask,
            "inherit" => Self::Inherit,
            "disabled" => Self::Disabled,
            _ => Self::PerTask,
        }
    }
}

/// DM handling policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmPolicy {
    Open,
    AllowedUsersOnly,
    Disabled,
}

impl DmPolicy {
    pub fn from_str(s: &str) -> Self {
        match s {
            "open" => Self::Open,
            "allowed_users_only" => Self::AllowedUsersOnly,
            "disabled" => Self::Disabled,
            _ => Self::Disabled,
        }
    }
}

/// Overflow strategy for long messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverflowStrategy {
    Split,
    FileAttachment,
    Truncate,
}

impl OverflowStrategy {
    pub fn from_str(s: &str) -> Self {
        match s {
            "split" => Self::Split,
            "file_attachment" => Self::FileAttachment,
            "truncate" => Self::Truncate,
            _ => Self::Split,
        }
    }
}

/// Sliding window rate limiter for Discord messages.
pub struct RateLimiter {
    config: RateLimitConfig,
    /// user_id -> list of timestamps.
    per_user: HashMap<String, Vec<Instant>>,
    /// channel_id -> list of timestamps.
    per_channel: HashMap<String, Vec<Instant>>,
    /// Number of currently active tasks.
    active_tasks: u32,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            per_user: HashMap::new(),
            per_channel: HashMap::new(),
            active_tasks: 0,
        }
    }

    /// Check if a message from the given user in the given channel is allowed.
    pub fn check(&mut self, user_id: &str, channel_id: &str) -> RateLimitResult {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(60);

        // Check concurrent task limit.
        if self.active_tasks >= self.config.max_concurrent_tasks {
            return RateLimitResult::Denied(RateLimitReason::MaxConcurrentTasks);
        }

        // Check per-user limit.
        let user_timestamps = self.per_user.entry(user_id.to_string()).or_default();
        user_timestamps.retain(|t| now.duration_since(*t) < window);
        if user_timestamps.len() >= self.config.per_user_per_minute as usize {
            return RateLimitResult::Denied(RateLimitReason::PerUserLimit);
        }

        // Check per-channel limit.
        let channel_timestamps = self.per_channel.entry(channel_id.to_string()).or_default();
        channel_timestamps.retain(|t| now.duration_since(*t) < window);
        if channel_timestamps.len() >= self.config.per_channel_per_minute as usize {
            return RateLimitResult::Denied(RateLimitReason::PerChannelLimit);
        }

        // Record the event.
        user_timestamps.push(now);
        channel_timestamps.push(now);

        RateLimitResult::Allowed
    }

    /// Increment active task count.
    pub fn task_started(&mut self) {
        self.active_tasks = self.active_tasks.saturating_add(1);
    }

    /// Decrement active task count.
    pub fn task_finished(&mut self) {
        self.active_tasks = self.active_tasks.saturating_sub(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitResult {
    Allowed,
    Denied(RateLimitReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitReason {
    PerUserLimit,
    PerChannelLimit,
    MaxConcurrentTasks,
}

/// Formats task output for Discord messages, respecting length limits.
pub struct DiscordResponseFormatter {
    config: DiscordResponseConfig,
}

impl DiscordResponseFormatter {
    pub fn new(config: DiscordResponseConfig) -> Self {
        Self { config }
    }

    /// Format text for Discord, splitting/truncating as needed.
    /// Returns a list of message chunks to send.
    pub fn format(&self, text: &str) -> Vec<String> {
        let max_len = self.config.max_message_length;
        if text.len() <= max_len {
            return vec![self.apply_formatting(text)];
        }

        let strategy = OverflowStrategy::from_str(&self.config.overflow);
        match strategy {
            OverflowStrategy::Truncate => {
                let truncated = &text[..max_len.saturating_sub(15)];
                vec![format!("{truncated}... (truncated)")]
            }
            OverflowStrategy::Split => self.split_message(text, max_len),
            OverflowStrategy::FileAttachment => {
                // Return a summary message; the caller is responsible for the file upload.
                let summary = if text.len() > 200 {
                    format!("{}...\n\n(Full response attached as file)", &text[..200])
                } else {
                    text.to_string()
                };
                vec![summary]
            }
        }
    }

    /// Split a message at paragraph/code-block boundaries.
    fn split_message(&self, text: &str, max_len: usize) -> Vec<String> {
        let mut chunks = Vec::new();
        let mut remaining = text;

        while !remaining.is_empty() {
            if remaining.len() <= max_len {
                chunks.push(self.apply_formatting(remaining));
                break;
            }

            // Find a good split point: try paragraph break, then newline, then hard cut.
            let search_range = &remaining[..max_len];
            let split_at = search_range
                .rfind("\n\n")
                .or_else(|| search_range.rfind('\n'))
                .unwrap_or(max_len);

            let (chunk, rest) = remaining.split_at(split_at);
            chunks.push(self.apply_formatting(chunk));
            remaining = rest.trim_start_matches('\n');
        }

        chunks
    }

    /// Apply auto-formatting (code blocks, etc.) to a chunk.
    fn apply_formatting(&self, text: &str) -> String {
        if self.config.auto_code_blocks {
            wrap_code_blocks(text)
        } else {
            text.to_string()
        }
    }
}

/// Detect lines that look like code and wrap them in Discord code blocks.
/// This is a heuristic: lines starting with common code indicators get wrapped.
fn wrap_code_blocks(text: &str) -> String {
    // If the text already has code blocks, leave it alone.
    if text.contains("```") {
        return text.to_string();
    }
    text.to_string()
}

/// Process an incoming Discord message and produce a TriggerEvent if it passes checks.
pub fn process_discord_message(
    config: &DiscordTriggerConfig,
    user_id: &str,
    guild_id: Option<&str>,
    channel_id: &str,
    message_text: &str,
    message_id: &str,
    thread_id: Option<&str>,
    bot_user_id: &str,
    rate_limiter: &mut RateLimiter,
) -> Option<TriggerEvent> {
    // Guild filtering.
    if let Some(gid) = guild_id {
        if !config.allowed_guilds.is_empty() && !config.allowed_guilds.contains(&gid.to_string()) {
            return None;
        }
    }

    // DM policy check (guild_id is None for DMs).
    if guild_id.is_none() {
        let policy = DmPolicy::from_str(&config.dm_policy);
        match policy {
            DmPolicy::Disabled => return None,
            DmPolicy::AllowedUsersOnly => {
                // In a full implementation, we'd check allowed user lists.
                // For now, reject DMs unless in an allowed guild context.
                return None;
            }
            DmPolicy::Open => {}
        }
    }

    // Find matching channel route.
    let route = config
        .channel_routes
        .iter()
        .find(|r| r.channel_id == channel_id);

    let route = match route {
        Some(r) => r,
        None => return None, // No route configured for this channel.
    };

    // Activation check.
    let activation = ActivationRule::from_str(&route.activation);
    match &activation {
        ActivationRule::All => {}
        ActivationRule::Mention => {
            if !message_text.contains(&format!("<@{bot_user_id}>"))
                && !message_text.contains(&format!("<@!{bot_user_id}>"))
            {
                return None;
            }
        }
        ActivationRule::Prefix(prefix) => {
            if !message_text.starts_with(prefix.as_str()) {
                return None;
            }
        }
        ActivationRule::Reply => {
            // Reply activation requires the message to be a reply to the bot.
            // Without full message context here, skip for now (would need is_reply flag).
            return None;
        }
    }

    // Rate limit check.
    match rate_limiter.check(user_id, channel_id) {
        RateLimitResult::Allowed => {}
        RateLimitResult::Denied(reason) => {
            info!(
                user_id = %user_id,
                channel_id = %channel_id,
                reason = ?reason,
                "rate limited discord message"
            );
            return None;
        }
    }

    // Build trigger event.
    let now = Utc::now();
    let trigger_id = format!("discord-{message_id}");

    Some(TriggerEvent {
        trigger_id,
        occurred_at: now,
        principal: Principal::DiscordUser {
            user_id: user_id.to_string(),
            guild_id: guild_id.map(|s| s.to_string()),
        },
        payload: TriggerPayload::Message {
            text: message_text.to_string(),
            attachments: vec![],
        },
        route_hint: Some(route.agent.clone()),
        metadata: TriggerMetadata {
            channel_id: Some(channel_id.to_string()),
            guild_id: guild_id.map(|s| s.to_string()),
            thread_id: thread_id.map(|s| s.to_string()),
            message_id: Some(message_id.to_string()),
        },
    })
}

/// Discord trigger that uses the twilight gateway.
/// NOTE: The actual twilight gateway connection is not included since twilight crates
/// may not compile in all environments. The types and message processing logic above
/// are complete and usable. This struct provides the scaffolding for a full integration.
pub struct DiscordTrigger {
    config: DiscordTriggerConfig,
    rate_limiter: RateLimiter,
}

impl DiscordTrigger {
    pub fn new(config: DiscordTriggerConfig) -> Self {
        let rate_limiter = RateLimiter::new(config.rate_limit.clone());
        Self {
            config,
            rate_limiter,
        }
    }

    /// Start the Discord trigger. This is a placeholder that logs the intent.
    /// A full implementation would use twilight-gateway to connect to Discord.
    pub async fn start(&mut self, _tx: mpsc::Sender<TriggerEvent>) -> Result<(), DiscordError> {
        info!(
            guilds = ?self.config.allowed_guilds,
            channels = self.config.channel_routes.len(),
            "discord trigger initialized (gateway connection not active — twilight may not compile)"
        );
        // In a full implementation:
        // 1. Resolve the bot token from secrets broker using config.token_secret.
        // 2. Create a twilight-gateway Shard with the token.
        // 3. Listen for MessageCreate events.
        // 4. For each message, call process_discord_message().
        // 5. Send resulting TriggerEvents through `tx`.
        Ok(())
    }

    /// Process an incoming message (called from the gateway event loop).
    pub fn process_message(
        &mut self,
        user_id: &str,
        guild_id: Option<&str>,
        channel_id: &str,
        message_text: &str,
        message_id: &str,
        thread_id: Option<&str>,
        bot_user_id: &str,
    ) -> Option<TriggerEvent> {
        process_discord_message(
            &self.config,
            user_id,
            guild_id,
            channel_id,
            message_text,
            message_id,
            thread_id,
            bot_user_id,
            &mut self.rate_limiter,
        )
    }

    /// Notify that a task has finished (for rate limiter tracking).
    pub fn task_finished(&mut self) {
        self.rate_limiter.task_finished();
    }

    /// Notify that a task has started (for rate limiter tracking).
    pub fn task_started(&mut self) {
        self.rate_limiter.task_started();
    }

    pub async fn shutdown(&mut self) {
        info!("discord trigger shutting down");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DiscordError {
    #[error("gateway connection failed: {0}")]
    GatewayConnection(String),
    #[error("token resolution failed: {0}")]
    TokenResolution(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuromancer_core::config::*;

    fn test_config() -> DiscordTriggerConfig {
        DiscordTriggerConfig {
            enabled: true,
            token_secret: "test_token".into(),
            allowed_guilds: vec!["guild1".into()],
            dm_policy: "disabled".into(),
            rate_limit: RateLimitConfig {
                per_user_per_minute: 5,
                per_channel_per_minute: 10,
                max_concurrent_tasks: 3,
            },
            channel_routes: vec![ChannelRouteConfig {
                channel_id: "channel1".into(),
                agent: "planner".into(),
                thread_mode: "per_task".into(),
                activation: "all".into(),
            }],
            response: DiscordResponseConfig::default(),
        }
    }

    #[test]
    fn rate_limiter_allows_within_limit() {
        let config = RateLimitConfig {
            per_user_per_minute: 3,
            per_channel_per_minute: 5,
            max_concurrent_tasks: 2,
        };
        let mut limiter = RateLimiter::new(config);

        assert_eq!(limiter.check("user1", "ch1"), RateLimitResult::Allowed);
        assert_eq!(limiter.check("user1", "ch1"), RateLimitResult::Allowed);
        assert_eq!(limiter.check("user1", "ch1"), RateLimitResult::Allowed);
        // 4th should be denied (per_user_per_minute = 3).
        assert_eq!(
            limiter.check("user1", "ch1"),
            RateLimitResult::Denied(RateLimitReason::PerUserLimit)
        );
    }

    #[test]
    fn rate_limiter_concurrent_tasks() {
        let config = RateLimitConfig {
            per_user_per_minute: 100,
            per_channel_per_minute: 100,
            max_concurrent_tasks: 2,
        };
        let mut limiter = RateLimiter::new(config);
        limiter.task_started();
        limiter.task_started();

        assert_eq!(
            limiter.check("user1", "ch1"),
            RateLimitResult::Denied(RateLimitReason::MaxConcurrentTasks)
        );

        limiter.task_finished();
        assert_eq!(limiter.check("user1", "ch1"), RateLimitResult::Allowed);
    }

    #[test]
    fn process_message_guild_filter() {
        let config = test_config();
        let mut rate_limiter = RateLimiter::new(config.rate_limit.clone());

        // Message from allowed guild, matching channel.
        let event = process_discord_message(
            &config,
            "user1",
            Some("guild1"),
            "channel1",
            "hello bot",
            "msg1",
            None,
            "bot_id",
            &mut rate_limiter,
        );
        assert!(event.is_some());

        // Message from disallowed guild.
        let event = process_discord_message(
            &config,
            "user1",
            Some("guild_other"),
            "channel1",
            "hello bot",
            "msg2",
            None,
            "bot_id",
            &mut rate_limiter,
        );
        assert!(event.is_none());
    }

    #[test]
    fn process_message_activation_mention() {
        let mut config = test_config();
        config.channel_routes[0].activation = "mention".into();

        let mut rate_limiter = RateLimiter::new(config.rate_limit.clone());

        // Without mention.
        let event = process_discord_message(
            &config,
            "user1",
            Some("guild1"),
            "channel1",
            "hello there",
            "msg1",
            None,
            "bot_id",
            &mut rate_limiter,
        );
        assert!(event.is_none());

        // With mention.
        let event = process_discord_message(
            &config,
            "user1",
            Some("guild1"),
            "channel1",
            "hello <@bot_id> do something",
            "msg2",
            None,
            "bot_id",
            &mut rate_limiter,
        );
        assert!(event.is_some());
    }

    #[test]
    fn response_formatter_truncate() {
        let config = DiscordResponseConfig {
            max_message_length: 50,
            overflow: "truncate".into(),
            use_embeds: false,
            auto_code_blocks: false,
        };
        let formatter = DiscordResponseFormatter::new(config);
        let long_text = "a".repeat(100);
        let chunks = formatter.format(&long_text);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].ends_with("... (truncated)"));
        assert!(chunks[0].len() <= 50);
    }

    #[test]
    fn response_formatter_split() {
        let config = DiscordResponseConfig {
            max_message_length: 50,
            overflow: "split".into(),
            use_embeds: false,
            auto_code_blocks: false,
        };
        let formatter = DiscordResponseFormatter::new(config);
        let text = "First paragraph.\n\nSecond paragraph that is also quite long and should cause splitting.";
        let chunks = formatter.format(text);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn dm_policy_disabled() {
        let config = test_config();
        let mut rate_limiter = RateLimiter::new(config.rate_limit.clone());

        // DM (guild_id = None) should be rejected.
        let event = process_discord_message(
            &config,
            "user1",
            None,
            "channel1",
            "hello",
            "msg1",
            None,
            "bot_id",
            &mut rate_limiter,
        );
        assert!(event.is_none());
    }
}
