use std::collections::HashSet;

use chrono::Utc;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};

use crate::orchestrator::adaptation::canary::{canary_metrics, rollback_reason};
use crate::orchestrator::proposals::apply::apply_proposal_mutation;
use crate::orchestrator::proposals::lifecycle::transition;
use crate::orchestrator::proposals::model::{
    ChangeProposalKind, ProposalApplyResult, ProposalState,
};
use crate::orchestrator::security::audit::AuditRiskLevel;
use crate::orchestrator::security::trigger_gate::ensure_admin_trigger;
use crate::orchestrator::state::System0ToolBroker;

pub const TOOL_IDS: &[&str] = &[
    "authorize_proposal",
    "apply_authorized_proposal",
    "modify_skill",
];

pub fn contains(tool_id: &str) -> bool {
    TOOL_IDS.contains(&tool_id)
}

impl System0ToolBroker {
    pub(crate) async fn handle_authenticated_adaptive_action(
        &self,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let mut inner = self.inner.lock().await;
        let turn_id = inner.current_turn_id;
        let trigger_type = inner.current_trigger_type;

        match call.tool_id.as_str() {
            "authorize_proposal" => {
                if !inner.self_improvement.enabled {
                    let result = self_improvement_disabled_result(&call);
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }
                if let Err(message) = ensure_admin_trigger(
                    trigger_type,
                    inner.self_improvement.require_admin_message_for_mutations,
                    "authorize_proposal",
                ) {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Error(message.clone()),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    Self::record_mutation_audit(
                        &mut inner,
                        "authorize_proposal",
                        "denied_non_admin_trigger",
                        trigger_type,
                        None,
                        serde_json::json!({ "reason": message }),
                    );
                    return Ok(result);
                }

                let Some(proposal_id) = call
                    .arguments
                    .get("proposal_id")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "authorize_proposal".to_string(),
                        message: "missing 'proposal_id'".to_string(),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };
                let force = call
                    .arguments
                    .get("force")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);

                let Some(mut proposal) = inner.proposals_index.get(proposal_id).cloned() else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "authorize_proposal".to_string(),
                        message: format!("proposal '{}' not found", proposal_id),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };

                if inner.self_improvement.verify_before_authorize
                    && !proposal.verification_report.passed
                {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Error(
                            "proposal verification failed; cannot authorize".to_string(),
                        ),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    Self::record_mutation_audit(
                        &mut inner,
                        "authorize_proposal",
                        "denied_verification_failed",
                        trigger_type,
                        Some(&proposal),
                        serde_json::json!({ "issues": proposal.verification_report.issues }),
                    );
                    return Ok(result);
                }

                if matches!(
                    proposal.audit_verdict.risk_level,
                    AuditRiskLevel::High | AuditRiskLevel::Critical
                ) && !force
                {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Error(
                            "high/critical risk proposal requires force=true in admin turn"
                                .to_string(),
                        ),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    Self::record_mutation_audit(
                        &mut inner,
                        "authorize_proposal",
                        "denied_high_risk_without_force",
                        trigger_type,
                        Some(&proposal),
                        serde_json::json!({ "risk_level": proposal.audit_verdict.risk_level }),
                    );
                    return Ok(result);
                }

                proposal.authorization.authorized = true;
                proposal.authorization.force = force;
                proposal.authorization.authorized_at = Some(Utc::now().to_rfc3339());
                proposal.authorization.authorized_trigger_type = Some(trigger_type);
                transition(&mut proposal, ProposalState::Authorized);
                inner
                    .proposals_index
                    .insert(proposal.proposal_id.clone(), proposal.clone());

                Self::record_mutation_audit(
                    &mut inner,
                    "authorize_proposal",
                    "authorized",
                    trigger_type,
                    Some(&proposal),
                    serde_json::json!({ "force": force }),
                );

                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "proposal_id": proposal.proposal_id,
                        "state": proposal.state,
                        "authorized": true,
                        "force": force,
                    })),
                };
                Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                Ok(result)
            }
            "apply_authorized_proposal" => {
                if !inner.self_improvement.enabled {
                    let result = self_improvement_disabled_result(&call);
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }
                if let Err(message) = ensure_admin_trigger(
                    trigger_type,
                    inner.self_improvement.require_admin_message_for_mutations,
                    "apply_authorized_proposal",
                ) {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Error(message.clone()),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    Self::record_mutation_audit(
                        &mut inner,
                        "apply_authorized_proposal",
                        "denied_non_admin_trigger",
                        trigger_type,
                        None,
                        serde_json::json!({ "reason": message }),
                    );
                    return Ok(result);
                }

                let Some(proposal_id) = call
                    .arguments
                    .get("proposal_id")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "apply_authorized_proposal".to_string(),
                        message: "missing 'proposal_id'".to_string(),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };

                let Some(mut proposal) = inner.proposals_index.get(proposal_id).cloned() else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "apply_authorized_proposal".to_string(),
                        message: format!("proposal '{}' not found", proposal_id),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };
                if !proposal.authorization.authorized {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Error(
                            "proposal must be authorized before apply".to_string(),
                        ),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    Self::record_mutation_audit(
                        &mut inner,
                        "apply_authorized_proposal",
                        "denied_not_authorized",
                        trigger_type,
                        Some(&proposal),
                        serde_json::json!({}),
                    );
                    return Ok(result);
                }

                if let Err(err) = inner.execution_guard.pre_apply_proposal(&proposal) {
                    proposal.apply_result = Some(ProposalApplyResult {
                        promoted: false,
                        rolled_back: true,
                        reason: Some(err.to_string()),
                    });
                    transition(&mut proposal, ProposalState::RolledBack);
                    inner
                        .proposals_index
                        .insert(proposal.proposal_id.clone(), proposal.clone());
                    Self::record_mutation_audit(
                        &mut inner,
                        "apply_authorized_proposal",
                        "rolled_back_guard_block",
                        trigger_type,
                        Some(&proposal),
                        serde_json::json!({ "reason": err.to_string() }),
                    );
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Success(serde_json::json!({
                            "proposal_id": proposal.proposal_id,
                            "state": proposal.state,
                            "rolled_back": true,
                            "reason": err.to_string(),
                        })),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }

                let managed_skills_before = inner.managed_skills.clone();
                let managed_agents_before = inner.managed_agents.clone();
                let known_skills_before = inner.known_skill_ids.clone();
                let config_snapshot_before = inner.config_snapshot.clone();
                let patches_before = inner.config_patch_history.clone();

                let metrics_before = canary_metrics(&inner.runs_index, &inner.skill_quality_stats);
                transition(&mut proposal, ProposalState::AppliedCanary);

                let subagent_ids = inner.subagents.keys().cloned().collect::<HashSet<_>>();
                let managed_agent_ids =
                    inner.managed_agents.keys().cloned().collect::<HashSet<_>>();
                let apply_err = {
                    let inner_ref: &mut crate::orchestrator::state::System0BrokerInner = &mut inner;
                    let crate::orchestrator::state::System0BrokerInner {
                        known_skill_ids,
                        managed_skills,
                        managed_agents,
                        config_snapshot,
                        config_patch_history,
                        ..
                    } = inner_ref;
                    apply_proposal_mutation(
                        &subagent_ids,
                        &managed_agent_ids,
                        known_skill_ids,
                        managed_skills,
                        managed_agents,
                        config_snapshot,
                        config_patch_history,
                        &proposal,
                    )
                    .err()
                };
                let metrics_after = canary_metrics(&inner.runs_index, &inner.skill_quality_stats);

                let rollback_reason = if let Some(err) = apply_err {
                    Some(err.to_string())
                } else if inner.self_improvement.canary_before_promote {
                    rollback_reason(
                        &metrics_before,
                        &metrics_after,
                        &inner.self_improvement.thresholds,
                        &proposal.payload,
                    )
                } else {
                    None
                };

                if let Some(reason) = rollback_reason {
                    inner.managed_skills = managed_skills_before;
                    inner.managed_agents = managed_agents_before;
                    inner.known_skill_ids = known_skills_before;
                    inner.config_snapshot = config_snapshot_before;
                    inner.config_patch_history = patches_before;

                    proposal.apply_result = Some(ProposalApplyResult {
                        promoted: false,
                        rolled_back: true,
                        reason: Some(reason.clone()),
                    });
                    transition(&mut proposal, ProposalState::RolledBack);
                    inner
                        .proposals_index
                        .insert(proposal.proposal_id.clone(), proposal.clone());
                    Self::record_mutation_audit(
                        &mut inner,
                        "apply_authorized_proposal",
                        "rolled_back",
                        trigger_type,
                        Some(&proposal),
                        serde_json::json!({ "reason": reason }),
                    );
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Success(serde_json::json!({
                            "proposal_id": proposal.proposal_id,
                            "state": proposal.state,
                            "rolled_back": true,
                            "reason": proposal.apply_result.as_ref().and_then(|r| r.reason.clone()),
                        })),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }

                proposal.apply_result = Some(ProposalApplyResult {
                    promoted: true,
                    rolled_back: false,
                    reason: None,
                });
                transition(&mut proposal, ProposalState::Promoted);
                inner.last_known_good_snapshot = inner.config_snapshot.clone();
                inner
                    .proposals_index
                    .insert(proposal.proposal_id.clone(), proposal.clone());
                Self::record_mutation_audit(
                    &mut inner,
                    "apply_authorized_proposal",
                    "promoted",
                    trigger_type,
                    Some(&proposal),
                    serde_json::json!({
                        "metrics_before": metrics_before,
                        "metrics_after": metrics_after,
                    }),
                );

                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "proposal_id": proposal.proposal_id,
                        "state": proposal.state,
                        "promoted": true,
                    })),
                };
                Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                Ok(result)
            }
            "modify_skill" => {
                if !inner.self_improvement.enabled {
                    let result = self_improvement_disabled_result(&call);
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }
                if let Err(message) = ensure_admin_trigger(
                    trigger_type,
                    inner.self_improvement.require_admin_message_for_mutations,
                    "modify_skill",
                ) {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Error(message.clone()),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    Self::record_mutation_audit(
                        &mut inner,
                        "modify_skill",
                        "denied_non_admin_trigger",
                        trigger_type,
                        None,
                        serde_json::json!({ "reason": message }),
                    );
                    return Ok(result);
                }
                let Some(skill_id) = call
                    .arguments
                    .get("skill_id")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "modify_skill".to_string(),
                        message: "missing 'skill_id'".to_string(),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };
                let Some(patch) = call.arguments.get("patch").and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "modify_skill".to_string(),
                        message: "missing 'patch'".to_string(),
                    });
                    Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                    return Err(err);
                };

                let kind = if inner.known_skill_ids.contains(skill_id)
                    || inner.managed_skills.contains_key(skill_id)
                {
                    ChangeProposalKind::SkillUpdate
                } else {
                    ChangeProposalKind::SkillAdd
                };
                let payload = if kind == ChangeProposalKind::SkillAdd {
                    serde_json::json!({ "content": patch })
                } else {
                    serde_json::json!({ "patch": patch })
                };
                let mut proposal =
                    Self::create_proposal(&mut inner, kind, Some(skill_id.to_string()), payload);

                if !proposal.verification_report.passed || !proposal.audit_verdict.allow {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Success(serde_json::json!({
                            "status": "blocked",
                            "proposal": proposal,
                        })),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }

                proposal.authorization.authorized = true;
                proposal.authorization.force = false;
                proposal.authorization.authorized_at = Some(Utc::now().to_rfc3339());
                proposal.authorization.authorized_trigger_type = Some(trigger_type);
                transition(&mut proposal, ProposalState::Authorized);

                if let Err(err) = inner.execution_guard.pre_apply_proposal(&proposal) {
                    proposal.apply_result = Some(ProposalApplyResult {
                        promoted: false,
                        rolled_back: true,
                        reason: Some(err.to_string()),
                    });
                    transition(&mut proposal, ProposalState::RolledBack);
                    inner
                        .proposals_index
                        .insert(proposal.proposal_id.clone(), proposal.clone());
                    inner.proposals_order.push(proposal.proposal_id.clone());
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: ToolOutput::Success(serde_json::json!({
                            "status": "rolled_back",
                            "proposal": proposal,
                            "reason": err.to_string(),
                        })),
                    };
                    Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                    return Ok(result);
                }

                let subagent_ids = inner.subagents.keys().cloned().collect::<HashSet<_>>();
                let managed_agent_ids =
                    inner.managed_agents.keys().cloned().collect::<HashSet<_>>();
                let apply_status = {
                    let inner_ref: &mut crate::orchestrator::state::System0BrokerInner = &mut inner;
                    let crate::orchestrator::state::System0BrokerInner {
                        known_skill_ids,
                        managed_skills,
                        managed_agents,
                        config_snapshot,
                        config_patch_history,
                        ..
                    } = inner_ref;
                    apply_proposal_mutation(
                        &subagent_ids,
                        &managed_agent_ids,
                        known_skill_ids,
                        managed_skills,
                        managed_agents,
                        config_snapshot,
                        config_patch_history,
                        &proposal,
                    )
                };
                match apply_status {
                    Ok(()) => {
                        proposal.apply_result = Some(ProposalApplyResult {
                            promoted: true,
                            rolled_back: false,
                            reason: None,
                        });
                        transition(&mut proposal, ProposalState::Promoted);
                        inner
                            .proposals_index
                            .insert(proposal.proposal_id.clone(), proposal.clone());
                        inner.proposals_order.push(proposal.proposal_id.clone());
                        Self::record_mutation_audit(
                            &mut inner,
                            "modify_skill",
                            "promoted",
                            trigger_type,
                            Some(&proposal),
                            serde_json::json!({}),
                        );
                        let result = ToolResult {
                            call_id: call.id.clone(),
                            output: ToolOutput::Success(serde_json::json!({
                                "status": "accepted",
                                "skill_id": skill_id,
                                "proposal": proposal,
                            })),
                        };
                        Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                        Ok(result)
                    }
                    Err(err) => {
                        proposal.apply_result = Some(ProposalApplyResult {
                            promoted: false,
                            rolled_back: true,
                            reason: Some(err.to_string()),
                        });
                        transition(&mut proposal, ProposalState::RolledBack);
                        inner
                            .proposals_index
                            .insert(proposal.proposal_id.clone(), proposal.clone());
                        inner.proposals_order.push(proposal.proposal_id.clone());
                        let result = ToolResult {
                            call_id: call.id.clone(),
                            output: ToolOutput::Success(serde_json::json!({
                                "status": "rolled_back",
                                "proposal": proposal,
                                "reason": err.to_string(),
                            })),
                        };
                        Self::record_invocation(&mut inner, turn_id, &call, &result.output);
                        Ok(result)
                    }
                }
            }
            _ => {
                let err = Self::not_found_err(&call.tool_id);
                Self::record_invocation_err(&mut inner, turn_id, &call, &err);
                Err(err)
            }
        }
    }
}

fn self_improvement_disabled_result(call: &ToolCall) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        output: ToolOutput::Error("self-improvement is disabled".to_string()),
    }
}
