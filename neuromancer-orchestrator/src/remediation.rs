use neuromancer_core::agent::{RemediationAction, SubAgentReport};

/// Default remediation policy configuration.
pub struct RemediationPolicy {
    pub max_retry_attempts: u32,
    pub retry_backoff_ms: u64,
    /// Whether to auto-grant temporary capabilities on PolicyDenied.
    pub auto_grant_temporary: bool,
    pub default_escalation_channel: String,
}

impl Default for RemediationPolicy {
    fn default() -> Self {
        Self {
            max_retry_attempts: 3,
            retry_backoff_ms: 1000,
            auto_grant_temporary: false,
            default_escalation_channel: "admin".into(),
        }
    }
}

/// Determine remediation action for a sub-agent report.
pub fn remediate(report: &SubAgentReport, policy: &RemediationPolicy) -> RemediationAction {
    match report {
        SubAgentReport::ToolFailure {
            retry_eligible,
            attempted_count,
            error,
            ..
        } => {
            if *retry_eligible && *attempted_count < policy.max_retry_attempts {
                RemediationAction::Retry {
                    max_attempts: policy.max_retry_attempts,
                    backoff_ms: policy.retry_backoff_ms * (*attempted_count as u64 + 1),
                }
            } else {
                RemediationAction::Abort {
                    reason: format!("tool failure after {attempted_count} attempts: {error}"),
                }
            }
        }

        SubAgentReport::PolicyDenied {
            capability_needed,
            action,
            task_id,
            ..
        } => {
            if policy.auto_grant_temporary {
                RemediationAction::GrantTemporary {
                    capability: capability_needed.clone(),
                    scope: *task_id,
                }
            } else {
                RemediationAction::EscalateToUser {
                    question: format!(
                        "Agent requested capability '{capability_needed}' for action '{action}'. Grant?"
                    ),
                    channel: policy.default_escalation_channel.clone(),
                }
            }
        }

        SubAgentReport::Stuck { reason, .. } => RemediationAction::Abort {
            reason: format!("agent stuck: {reason}"),
        },

        SubAgentReport::InputRequired { question, .. } => RemediationAction::EscalateToUser {
            question: question.clone(),
            channel: policy.default_escalation_channel.clone(),
        },

        SubAgentReport::Completed { .. } => {
            // No remediation needed, this is informational
            RemediationAction::Abort {
                reason: "no remediation needed (task completed)".into(),
            }
        }

        SubAgentReport::Failed { error, .. } => RemediationAction::Abort {
            reason: format!("agent failed: {error}"),
        },

        SubAgentReport::Progress { .. } => {
            // Progress reports don't need remediation
            RemediationAction::Abort {
                reason: "no remediation needed (progress report)".into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_failure_retry_eligible() {
        let report = SubAgentReport::ToolFailure {
            task_id: uuid::Uuid::new_v4(),
            tool_id: "search".into(),
            error: "timeout".into(),
            retry_eligible: true,
            attempted_count: 1,
        };
        let policy = RemediationPolicy::default();
        let action = remediate(&report, &policy);
        assert!(matches!(action, RemediationAction::Retry { .. }));
    }

    #[test]
    fn tool_failure_exhausted_retries() {
        let report = SubAgentReport::ToolFailure {
            task_id: uuid::Uuid::new_v4(),
            tool_id: "search".into(),
            error: "timeout".into(),
            retry_eligible: true,
            attempted_count: 5,
        };
        let policy = RemediationPolicy::default();
        let action = remediate(&report, &policy);
        assert!(matches!(action, RemediationAction::Abort { .. }));
    }

    #[test]
    fn policy_denied_escalates() {
        let report = SubAgentReport::PolicyDenied {
            task_id: uuid::Uuid::new_v4(),
            action: "write_file".into(),
            policy_code: "fs_write".into(),
            capability_needed: "filesystem:write".into(),
        };
        let policy = RemediationPolicy::default();
        let action = remediate(&report, &policy);
        assert!(matches!(action, RemediationAction::EscalateToUser { .. }));
    }

    #[test]
    fn policy_denied_auto_grants() {
        let report = SubAgentReport::PolicyDenied {
            task_id: uuid::Uuid::new_v4(),
            action: "write_file".into(),
            policy_code: "fs_write".into(),
            capability_needed: "filesystem:write".into(),
        };
        let policy = RemediationPolicy {
            auto_grant_temporary: true,
            ..Default::default()
        };
        let action = remediate(&report, &policy);
        assert!(matches!(action, RemediationAction::GrantTemporary { .. }));
    }

    #[test]
    fn stuck_aborts() {
        let report = SubAgentReport::Stuck {
            task_id: uuid::Uuid::new_v4(),
            reason: "circular reasoning".into(),
            partial_result: None,
        };
        let policy = RemediationPolicy::default();
        let action = remediate(&report, &policy);
        assert!(matches!(action, RemediationAction::Abort { .. }));
    }
}
