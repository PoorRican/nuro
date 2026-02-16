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
use crate::orchestrator::tool_id::AuthenticatedAdaptiveToolId;

impl System0ToolBroker {
    pub(crate) async fn handle_authenticated_adaptive_action(
        &self,
        id: AuthenticatedAdaptiveToolId,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let mut inner = self.inner.lock().await;
        let turn_id = inner.turn.current_turn_id;
        let trigger_type = inner.turn.current_trigger_type;

        match id {
            AuthenticatedAdaptiveToolId::AuthorizeProposal => {
                handle_authorize_proposal(&mut inner, call, turn_id, trigger_type)
            }
            AuthenticatedAdaptiveToolId::ApplyAuthorizedProposal => {
                handle_apply_authorized_proposal(&mut inner, call, turn_id, trigger_type)
            }
            AuthenticatedAdaptiveToolId::ModifySkill => {
                handle_modify_skill(&mut inner, call, turn_id, trigger_type)
            }
        }
    }
}

use crate::orchestrator::state::System0BrokerInner;

/// Gate: check self-improvement enabled + admin trigger. Returns `Ok(())` to proceed,
/// or `Ok(ToolResult)` for an early-return error response.
fn gate_self_improvement(
    inner: &mut System0BrokerInner,
    call: &ToolCall,
    turn_id: uuid::Uuid,
    trigger_type: neuromancer_core::trigger::TriggerType,
    tool_name: &str,
) -> Result<(), ToolResult> {
    if !inner.improvement.config.enabled {
        let result = self_improvement_disabled_result(call);
        inner.runs.record_invocation(turn_id, call, &result.output);
        return Err(result);
    }
    if let Err(message) = ensure_admin_trigger(
        trigger_type,
        inner.improvement.config.require_admin_message_for_mutations,
        tool_name,
    ) {
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Error(message.clone()),
        };
        inner.runs.record_invocation(turn_id, call, &result.output);
        inner.proposals.record_mutation_audit(
            tool_name,
            "denied_non_admin_trigger",
            trigger_type,
            None,
            serde_json::json!({ "reason": message }),
        );
        return Err(result);
    }
    Ok(())
}

fn handle_authorize_proposal(
    inner: &mut System0BrokerInner,
    call: ToolCall,
    turn_id: uuid::Uuid,
    trigger_type: neuromancer_core::trigger::TriggerType,
) -> Result<ToolResult, NeuromancerError> {
    if let Err(result) = gate_self_improvement(inner, &call, turn_id, trigger_type, "authorize_proposal") {
        return Ok(result);
    }

    let Some(proposal_id) = call.arguments.get("proposal_id").and_then(|v| v.as_str()) else {
        let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: "authorize_proposal".to_string(),
            message: "missing 'proposal_id'".to_string(),
        });
        inner.runs.record_invocation_err(turn_id, &call, &err);
        return Err(err);
    };
    let force = call.arguments.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    let Some(mut proposal) = inner.proposals.proposals_index.get(proposal_id).cloned() else {
        let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: "authorize_proposal".to_string(),
            message: format!("proposal '{}' not found", proposal_id),
        });
        inner.runs.record_invocation_err(turn_id, &call, &err);
        return Err(err);
    };

    if inner.improvement.config.verify_before_authorize && !proposal.verification_report.passed {
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Error("proposal verification failed; cannot authorize".to_string()),
        };
        inner.runs.record_invocation(turn_id, &call, &result.output);
        inner.proposals.record_mutation_audit(
            "authorize_proposal", "denied_verification_failed", trigger_type,
            Some(&proposal),
            serde_json::json!({ "issues": proposal.verification_report.issues }),
        );
        return Ok(result);
    }

    if matches!(proposal.audit_verdict.risk_level, AuditRiskLevel::High | AuditRiskLevel::Critical) && !force {
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Error(
                "high/critical risk proposal requires force=true in admin turn".to_string(),
            ),
        };
        inner.runs.record_invocation(turn_id, &call, &result.output);
        inner.proposals.record_mutation_audit(
            "authorize_proposal", "denied_high_risk_without_force", trigger_type,
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
    inner.proposals.proposals_index.insert(proposal.proposal_id.clone(), proposal.clone());
    inner.proposals.record_mutation_audit(
        "authorize_proposal", "authorized", trigger_type, Some(&proposal),
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
    inner.runs.record_invocation(turn_id, &call, &result.output);
    Ok(result)
}

fn handle_apply_authorized_proposal(
    inner: &mut System0BrokerInner,
    call: ToolCall,
    turn_id: uuid::Uuid,
    trigger_type: neuromancer_core::trigger::TriggerType,
) -> Result<ToolResult, NeuromancerError> {
    if let Err(result) = gate_self_improvement(inner, &call, turn_id, trigger_type, "apply_authorized_proposal") {
        return Ok(result);
    }

    let Some(proposal_id) = call.arguments.get("proposal_id").and_then(|v| v.as_str()) else {
        let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: "apply_authorized_proposal".to_string(),
            message: "missing 'proposal_id'".to_string(),
        });
        inner.runs.record_invocation_err(turn_id, &call, &err);
        return Err(err);
    };

    let Some(mut proposal) = inner.proposals.proposals_index.get(proposal_id).cloned() else {
        let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: "apply_authorized_proposal".to_string(),
            message: format!("proposal '{}' not found", proposal_id),
        });
        inner.runs.record_invocation_err(turn_id, &call, &err);
        return Err(err);
    };
    if !proposal.authorization.authorized {
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Error("proposal must be authorized before apply".to_string()),
        };
        inner.runs.record_invocation(turn_id, &call, &result.output);
        inner.proposals.record_mutation_audit(
            "apply_authorized_proposal", "denied_not_authorized", trigger_type,
            Some(&proposal), serde_json::json!({}),
        );
        return Ok(result);
    }

    if let Err(err) = inner.agents.execution_guard.pre_apply_proposal(&proposal) {
        proposal.apply_result = Some(ProposalApplyResult {
            promoted: false, rolled_back: true, reason: Some(err.to_string()),
        });
        transition(&mut proposal, ProposalState::RolledBack);
        inner.proposals.proposals_index.insert(proposal.proposal_id.clone(), proposal.clone());
        inner.proposals.record_mutation_audit(
            "apply_authorized_proposal", "rolled_back_guard_block", trigger_type,
            Some(&proposal), serde_json::json!({ "reason": err.to_string() }),
        );
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Success(serde_json::json!({
                "proposal_id": proposal.proposal_id, "state": proposal.state,
                "rolled_back": true, "reason": err.to_string(),
            })),
        };
        inner.runs.record_invocation(turn_id, &call, &result.output);
        return Ok(result);
    }

    // Snapshot improvement state for canary rollback
    let improvement_snapshot = inner.improvement.clone();
    let metrics_before = canary_metrics(&inner.runs.runs_index, &inner.improvement.skill_quality_stats);
    transition(&mut proposal, ProposalState::AppliedCanary);

    let subagent_ids = inner.agents.subagents.keys().cloned().collect::<HashSet<_>>();
    let apply_err = apply_proposal_mutation(&subagent_ids, &mut inner.improvement, &proposal).err();
    let metrics_after = canary_metrics(&inner.runs.runs_index, &inner.improvement.skill_quality_stats);

    let rollback = if let Some(err) = apply_err {
        Some(err.to_string())
    } else if inner.improvement.config.canary_before_promote {
        rollback_reason(&metrics_before, &metrics_after, &inner.improvement.config.thresholds, &proposal.payload)
    } else {
        None
    };

    if let Some(reason) = rollback {
        inner.improvement = improvement_snapshot;
        proposal.apply_result = Some(ProposalApplyResult {
            promoted: false, rolled_back: true, reason: Some(reason.clone()),
        });
        transition(&mut proposal, ProposalState::RolledBack);
        inner.proposals.proposals_index.insert(proposal.proposal_id.clone(), proposal.clone());
        inner.proposals.record_mutation_audit(
            "apply_authorized_proposal", "rolled_back", trigger_type,
            Some(&proposal), serde_json::json!({ "reason": reason }),
        );
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Success(serde_json::json!({
                "proposal_id": proposal.proposal_id, "state": proposal.state,
                "rolled_back": true,
                "reason": proposal.apply_result.as_ref().and_then(|r| r.reason.clone()),
            })),
        };
        inner.runs.record_invocation(turn_id, &call, &result.output);
        return Ok(result);
    }

    proposal.apply_result = Some(ProposalApplyResult { promoted: true, rolled_back: false, reason: None });
    transition(&mut proposal, ProposalState::Promoted);
    inner.improvement.last_known_good_snapshot = inner.improvement.config_snapshot.clone();
    inner.proposals.proposals_index.insert(proposal.proposal_id.clone(), proposal.clone());
    inner.proposals.record_mutation_audit(
        "apply_authorized_proposal", "promoted", trigger_type, Some(&proposal),
        serde_json::json!({ "metrics_before": metrics_before, "metrics_after": metrics_after }),
    );

    let result = ToolResult {
        call_id: call.id.clone(),
        output: ToolOutput::Success(serde_json::json!({
            "proposal_id": proposal.proposal_id, "state": proposal.state, "promoted": true,
        })),
    };
    inner.runs.record_invocation(turn_id, &call, &result.output);
    Ok(result)
}

fn handle_modify_skill(
    inner: &mut System0BrokerInner,
    call: ToolCall,
    turn_id: uuid::Uuid,
    trigger_type: neuromancer_core::trigger::TriggerType,
) -> Result<ToolResult, NeuromancerError> {
    if let Err(result) = gate_self_improvement(inner, &call, turn_id, trigger_type, "modify_skill") {
        return Ok(result);
    }

    let Some(skill_id) = call.arguments.get("skill_id").and_then(|v| v.as_str()) else {
        let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: "modify_skill".to_string(), message: "missing 'skill_id'".to_string(),
        });
        inner.runs.record_invocation_err(turn_id, &call, &err);
        return Err(err);
    };
    let Some(patch) = call.arguments.get("patch").and_then(|v| v.as_str()) else {
        let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: "modify_skill".to_string(), message: "missing 'patch'".to_string(),
        });
        inner.runs.record_invocation_err(turn_id, &call, &err);
        return Err(err);
    };

    let kind = if inner.improvement.known_skill_ids.contains(skill_id)
        || inner.improvement.managed_skills.contains_key(skill_id)
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
    let mut proposal = System0ToolBroker::create_proposal(
        inner, kind, Some(skill_id.to_string()), payload,
    );

    if !proposal.verification_report.passed || !proposal.audit_verdict.allow {
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Success(serde_json::json!({ "status": "blocked", "proposal": proposal })),
        };
        inner.runs.record_invocation(turn_id, &call, &result.output);
        return Ok(result);
    }

    proposal.authorization.authorized = true;
    proposal.authorization.force = false;
    proposal.authorization.authorized_at = Some(Utc::now().to_rfc3339());
    proposal.authorization.authorized_trigger_type = Some(trigger_type);
    transition(&mut proposal, ProposalState::Authorized);

    if let Err(err) = inner.agents.execution_guard.pre_apply_proposal(&proposal) {
        proposal.apply_result = Some(ProposalApplyResult {
            promoted: false, rolled_back: true, reason: Some(err.to_string()),
        });
        transition(&mut proposal, ProposalState::RolledBack);
        inner.proposals.proposals_index.insert(proposal.proposal_id.clone(), proposal.clone());
        inner.proposals.proposals_order.push(proposal.proposal_id.clone());
        let result = ToolResult {
            call_id: call.id.clone(),
            output: ToolOutput::Success(serde_json::json!({
                "status": "rolled_back", "proposal": proposal, "reason": err.to_string(),
            })),
        };
        inner.runs.record_invocation(turn_id, &call, &result.output);
        return Ok(result);
    }

    let subagent_ids = inner.agents.subagents.keys().cloned().collect::<HashSet<_>>();
    let apply_status = apply_proposal_mutation(&subagent_ids, &mut inner.improvement, &proposal);
    match apply_status {
        Ok(()) => {
            proposal.apply_result = Some(ProposalApplyResult { promoted: true, rolled_back: false, reason: None });
            transition(&mut proposal, ProposalState::Promoted);
            inner.proposals.proposals_index.insert(proposal.proposal_id.clone(), proposal.clone());
            inner.proposals.proposals_order.push(proposal.proposal_id.clone());
            inner.proposals.record_mutation_audit(
                "modify_skill", "promoted", trigger_type, Some(&proposal), serde_json::json!({}),
            );
            let result = ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::Success(serde_json::json!({
                    "status": "accepted", "skill_id": skill_id, "proposal": proposal,
                })),
            };
            inner.runs.record_invocation(turn_id, &call, &result.output);
            Ok(result)
        }
        Err(err) => {
            proposal.apply_result = Some(ProposalApplyResult {
                promoted: false, rolled_back: true, reason: Some(err.to_string()),
            });
            transition(&mut proposal, ProposalState::RolledBack);
            inner.proposals.proposals_index.insert(proposal.proposal_id.clone(), proposal.clone());
            inner.proposals.proposals_order.push(proposal.proposal_id.clone());
            let result = ToolResult {
                call_id: call.id.clone(),
                output: ToolOutput::Success(serde_json::json!({
                    "status": "rolled_back", "proposal": proposal, "reason": err.to_string(),
                })),
            };
            inner.runs.record_invocation(turn_id, &call, &result.output);
            Ok(result)
        }
    }
}

fn self_improvement_disabled_result(call: &ToolCall) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        output: ToolOutput::Error("self-improvement is disabled".to_string()),
    }
}
