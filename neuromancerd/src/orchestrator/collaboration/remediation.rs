use neuromancer_core::agent::{RemediationAction, SubAgentReport};

#[derive(Debug, Clone)]
pub(crate) struct RemediationContext {
    pub(crate) report_repeat_count: usize,
    pub(crate) current_agent_id: String,
    pub(crate) available_agent_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct RemediationRecommendation {
    pub(crate) action: RemediationAction,
    pub(crate) action_name: String,
    pub(crate) reason: String,
}

pub(crate) fn recommend(
    report: &SubAgentReport,
    context: &RemediationContext,
) -> Option<RemediationRecommendation> {
    match report {
        SubAgentReport::Progress { .. } | SubAgentReport::Completed { .. } => None,
        SubAgentReport::ToolFailure {
            tool_id,
            retry_eligible,
            attempted_count,
            ..
        } => {
            if *retry_eligible && *attempted_count < 3 {
                let exponent = attempted_count.saturating_sub(1);
                let backoff_ms = (500_u64.saturating_mul(2_u64.saturating_pow(exponent))).min(5000);
                Some(RemediationRecommendation {
                    action: RemediationAction::Retry {
                        max_attempts: 3,
                        backoff_ms,
                    },
                    action_name: "retry".to_string(),
                    reason: format!(
                        "tool '{tool_id}' failed on attempt {attempted_count}; retry is still eligible"
                    ),
                })
            } else {
                Some(RemediationRecommendation {
                    action: RemediationAction::Clarify {
                        additional_context: format!(
                            "Tool '{tool_id}' failed repeatedly or is not retry-eligible; revise strategy and use alternate steps."
                        ),
                    },
                    action_name: "clarify".to_string(),
                    reason: format!(
                        "tool '{tool_id}' is no longer a safe retry candidate (attempts={attempted_count}, retry_eligible={retry_eligible})"
                    ),
                })
            }
        }
        SubAgentReport::Stuck { reason, .. } => {
            if context.report_repeat_count <= 1 {
                return Some(RemediationRecommendation {
                    action: RemediationAction::Clarify {
                        additional_context: format!(
                            "Agent reported stuck: {reason}. Narrow the scope, restate success criteria, and continue."
                        ),
                    },
                    action_name: "clarify".to_string(),
                    reason: "first stuck report for this run".to_string(),
                });
            }

            if let Some(new_agent_id) = first_alternate_agent(context) {
                return Some(RemediationRecommendation {
                    action: RemediationAction::Reassign {
                        new_agent_id: new_agent_id.clone(),
                        reason: format!("repeated stuck report; reassigning to '{new_agent_id}'"),
                    },
                    action_name: "reassign".to_string(),
                    reason: format!(
                        "stuck report repeated {} times for this run",
                        context.report_repeat_count
                    ),
                });
            }

            Some(RemediationRecommendation {
                action: RemediationAction::EscalateToUser {
                    question: format!(
                        "Sub-agent is repeatedly stuck ({}) and no alternate agent is available. Please provide guidance or approve abort.",
                        context.current_agent_id
                    ),
                    channel: "trigger".to_string(),
                },
                action_name: "escalate_to_user".to_string(),
                reason: "repeated stuck report without alternate agent".to_string(),
            })
        }
        SubAgentReport::PolicyDenied {
            capability_needed,
            policy_code,
            ..
        } => {
            if let Some(target_agent) = resolve_capability_target(
                capability_needed,
                &context.current_agent_id,
                &context.available_agent_ids,
            ) {
                return Some(RemediationRecommendation {
                    action: RemediationAction::Reassign {
                        new_agent_id: target_agent.clone(),
                        reason: format!(
                            "policy denied '{capability_needed}' ({policy_code}); reassigning to '{target_agent}'"
                        ),
                    },
                    action_name: "reassign".to_string(),
                    reason: format!(
                        "policy denial references resolvable capability '{capability_needed}'"
                    ),
                });
            }

            Some(RemediationRecommendation {
                action: RemediationAction::EscalateToUser {
                    question: format!(
                        "Policy denied capability '{capability_needed}' ({policy_code}). Please approve a safe alternative."
                    ),
                    channel: "trigger".to_string(),
                },
                action_name: "escalate_to_user".to_string(),
                reason: format!(
                    "policy denial is not directly reassignable: '{capability_needed}'"
                ),
            })
        }
        SubAgentReport::InputRequired { question, .. } => Some(RemediationRecommendation {
            action: RemediationAction::EscalateToUser {
                question: question.clone(),
                channel: "trigger".to_string(),
            },
            action_name: "escalate_to_user".to_string(),
            reason: "agent requested external input".to_string(),
        }),
        SubAgentReport::Failed { error, .. } => Some(RemediationRecommendation {
            action: RemediationAction::Abort {
                reason: error.clone(),
            },
            action_name: "abort".to_string(),
            reason: "agent reported unrecoverable failure".to_string(),
        }),
    }
}

fn first_alternate_agent(context: &RemediationContext) -> Option<String> {
    let mut candidates = context
        .available_agent_ids
        .iter()
        .filter(|agent| agent.as_str() != context.current_agent_id)
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.into_iter().next()
}

fn resolve_capability_target(
    capability_needed: &str,
    current_agent_id: &str,
    available_agent_ids: &[String],
) -> Option<String> {
    let target = extract_capability_target(capability_needed)?;
    if target == current_agent_id {
        return None;
    }
    if available_agent_ids.iter().any(|agent| agent == target) {
        return Some(target.to_string());
    }
    None
}

fn extract_capability_target(capability_needed: &str) -> Option<&str> {
    for prefix in ["can_request:", "delegate_to_agent:", "agent:", "agent_id:"] {
        if let Some(target) = capability_needed.strip_prefix(prefix) {
            let trimmed = target.trim();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(current: &str, repeat_count: usize) -> RemediationContext {
        RemediationContext {
            report_repeat_count: repeat_count,
            current_agent_id: current.to_string(),
            available_agent_ids: vec![
                "planner".to_string(),
                "browser".to_string(),
                "files".to_string(),
            ],
        }
    }

    #[test]
    fn recommends_retry_for_retryable_tool_failure() {
        let report = SubAgentReport::ToolFailure {
            task_id: uuid::Uuid::new_v4(),
            tool_id: "search".to_string(),
            error: "timeout".to_string(),
            retry_eligible: true,
            attempted_count: 2,
        };
        let recommendation = recommend(&report, &ctx("planner", 1)).expect("recommendation");
        assert_eq!(recommendation.action_name, "retry");
        assert!(matches!(
            recommendation.action,
            RemediationAction::Retry {
                max_attempts: 3,
                backoff_ms: 1000
            }
        ));
    }

    #[test]
    fn recommends_clarify_for_first_stuck() {
        let report = SubAgentReport::Stuck {
            task_id: uuid::Uuid::new_v4(),
            reason: "looping".to_string(),
            partial_result: None,
        };
        let recommendation = recommend(&report, &ctx("planner", 1)).expect("recommendation");
        assert_eq!(recommendation.action_name, "clarify");
    }

    #[test]
    fn recommends_reassign_for_repeated_stuck() {
        let report = SubAgentReport::Stuck {
            task_id: uuid::Uuid::new_v4(),
            reason: "looping".to_string(),
            partial_result: None,
        };
        let recommendation = recommend(&report, &ctx("planner", 2)).expect("recommendation");
        assert_eq!(recommendation.action_name, "reassign");
        assert!(matches!(
            recommendation.action,
            RemediationAction::Reassign { .. }
        ));
    }

    #[test]
    fn recommends_reassign_for_resolvable_policy_denial() {
        let report = SubAgentReport::PolicyDenied {
            task_id: uuid::Uuid::new_v4(),
            action: "delegate".to_string(),
            policy_code: "capability_denied".to_string(),
            capability_needed: "can_request:browser".to_string(),
        };
        let recommendation = recommend(&report, &ctx("planner", 1)).expect("recommendation");
        assert_eq!(recommendation.action_name, "reassign");
    }

    #[test]
    fn recommends_escalation_for_input_required() {
        let report = SubAgentReport::InputRequired {
            task_id: uuid::Uuid::new_v4(),
            question: "Need approval".to_string(),
            context: "file write".to_string(),
            suggested_options: vec![],
        };
        let recommendation = recommend(&report, &ctx("planner", 1)).expect("recommendation");
        assert_eq!(recommendation.action_name, "escalate_to_user");
    }

    #[test]
    fn recommends_abort_for_failed() {
        let report = SubAgentReport::Failed {
            task_id: uuid::Uuid::new_v4(),
            error: "fatal".to_string(),
            partial_result: None,
        };
        let recommendation = recommend(&report, &ctx("planner", 1)).expect("recommendation");
        assert_eq!(recommendation.action_name, "abort");
    }
}
