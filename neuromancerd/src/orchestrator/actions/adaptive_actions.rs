use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolResult};

use crate::orchestrator::adaptation::analytics::{
    failure_clusters, routing_adaptation, skill_scores,
};
use crate::orchestrator::adaptation::lessons::LESSONS_MEMORY_PARTITION;
use crate::orchestrator::adaptation::redteam::run_redteam_eval;
use crate::orchestrator::proposals::model::ChangeProposalKind;
use crate::orchestrator::proposals::verification::known_skills;
use crate::orchestrator::state::System0ToolBroker;

impl System0ToolBroker {
    pub(crate) async fn handle_adaptive_action(
        &self,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let mut inner = self.inner.lock().await;
        let turn_id = inner.turn.current_turn_id;

        // TODO: can we use an enum?
        match call.tool_id.as_str() {
            "list_proposals" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let proposals: Vec<serde_json::Value> = inner
                    .proposals
                    .proposals_order
                    .iter()
                    .filter_map(|id| inner.proposals.proposals_index.get(id))
                    .map(|proposal| {
                        serde_json::json!({
                            "proposal_id": proposal.proposal_id,
                            "proposal_hash": proposal.proposal_hash,
                            "kind": proposal.kind,
                            "state": proposal.state,
                            "created_at": proposal.created_at,
                            "updated_at": proposal.updated_at,
                        })
                    })
                    .collect();
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({ "proposals": proposals })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "get_proposal" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let Some(proposal_id) = call
                    .arguments
                    .get("proposal_id")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "get_proposal".to_string(),
                        message: "missing 'proposal_id'".to_string(),
                    });
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                };
                let Some(proposal) = inner.proposals.proposals_index.get(proposal_id) else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "get_proposal".to_string(),
                        message: format!("proposal '{}' not found", proposal_id),
                    });
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                };
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::to_value(proposal).unwrap_or_default()),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "propose_config_change"
            | "propose_skill_add"
            | "propose_skill_update"
            | "propose_agent_add"
            | "propose_agent_update" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    // TODO: [low-pri] should we log a warning? Should this be part of analytics / auditing?
                    return Ok(result);
                }

                // TODO: break out sub-functions?
                let (kind, target_id, payload) = match call.tool_id.as_str() {
                    "propose_config_change" => (
                        ChangeProposalKind::ConfigChange,
                        None,
                        serde_json::json!({
                            "patch": call.arguments.get("patch").and_then(|v| v.as_str()).unwrap_or_default(),
                            "simulate_regression": call.arguments.get("simulate_regression").and_then(|v| v.as_bool()).unwrap_or(false),
                            "required_safeguards": call.arguments.get("required_safeguards").cloned().unwrap_or(serde_json::json!([])),
                        }),
                    ),
                    "propose_skill_add" => (
                        ChangeProposalKind::SkillAdd,
                        call.arguments
                            .get("skill_id")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string),
                        serde_json::json!({
                            "content": call.arguments.get("content").and_then(|v| v.as_str()).unwrap_or_default(),
                            "simulate_regression": call.arguments.get("simulate_regression").and_then(|v| v.as_bool()).unwrap_or(false),
                            "required_safeguards": call.arguments.get("required_safeguards").cloned().unwrap_or(serde_json::json!([])),
                        }),
                    ),
                    "propose_skill_update" => (
                        ChangeProposalKind::SkillUpdate,
                        call.arguments
                            .get("skill_id")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string),
                        serde_json::json!({
                            "patch": call.arguments.get("patch").and_then(|v| v.as_str()).unwrap_or_default(),
                            "simulate_regression": call.arguments.get("simulate_regression").and_then(|v| v.as_bool()).unwrap_or(false),
                            "required_safeguards": call.arguments.get("required_safeguards").cloned().unwrap_or(serde_json::json!([])),
                        }),
                    ),
                    "propose_agent_add" => (
                        ChangeProposalKind::AgentAdd,
                        call.arguments
                            .get("agent_id")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string),
                        serde_json::json!({
                            "patch": call.arguments.get("patch").and_then(|v| v.as_str()).unwrap_or_default(),
                            "simulate_regression": call.arguments.get("simulate_regression").and_then(|v| v.as_bool()).unwrap_or(false),
                            "required_safeguards": call.arguments.get("required_safeguards").cloned().unwrap_or(serde_json::json!([])),
                        }),
                    ),
                    "propose_agent_update" => (
                        ChangeProposalKind::AgentUpdate,
                        call.arguments
                            .get("agent_id")
                            .and_then(|v| v.as_str())
                            .map(ToString::to_string),
                        serde_json::json!({
                            "patch": call.arguments.get("patch").and_then(|v| v.as_str()).unwrap_or_default(),
                            "simulate_regression": call.arguments.get("simulate_regression").and_then(|v| v.as_bool()).unwrap_or(false),
                            "required_safeguards": call.arguments.get("required_safeguards").cloned().unwrap_or(serde_json::json!([])),
                        }),
                    ),
                    _ => unreachable!(),
                };

                if target_id
                    .as_deref()
                    .is_some_and(|target| target.trim().is_empty())
                {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: call.tool_id.clone(),
                        message: "target id must not be empty".to_string(),
                    });
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                }

                let proposal = Self::create_proposal(&mut inner, kind, target_id, payload);
                inner
                    .proposals
                    .proposals_order
                    .push(proposal.proposal_id.clone());
                inner
                    .proposals
                    .proposals_index
                    .insert(proposal.proposal_id.clone(), proposal.clone());

                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "proposal": proposal
                    })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "analyze_failures" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let clusters = failure_clusters(&inner.runs.runs_index);
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({ "clusters": clusters })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "score_skills" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let skills = known_skills(
                    &inner.improvement.known_skill_ids,
                    &inner.improvement.managed_skills,
                    &inner.improvement.config_snapshot,
                );
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "scores": skill_scores(skills, &inner.improvement.skill_quality_stats),
                    })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "adapt_routing" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "recommendations": routing_adaptation(&inner.runs.runs_index),
                    })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "record_lesson" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let Some(lesson) = call
                    .arguments
                    .get("lesson")
                    .and_then(|value| value.as_str())
                else {
                    let err = NeuromancerError::Tool(ToolError::ExecutionFailed {
                        tool_id: "record_lesson".to_string(),
                        message: "missing 'lesson'".to_string(),
                    });
                    inner.runs.record_invocation_err(turn_id, &call, &err);
                    return Err(err);
                };
                inner.improvement.lessons_learned.push(lesson.to_string());
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "partition": LESSONS_MEMORY_PARTITION,
                        "stored": true,
                        "count": inner.improvement.lessons_learned.len(),
                    })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "run_redteam_eval" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(run_redteam_eval(&inner.improvement.config)),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            "list_audit_records" => {
                if !inner.improvement.config.enabled {
                    let result = self_improvement_disabled_result(&call);
                    inner.runs.record_invocation(turn_id, &call, &result.output);
                    return Ok(result);
                }
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: ToolOutput::Success(serde_json::json!({
                        "records": inner.proposals.mutation_audit_log.clone()
                    })),
                };
                inner.runs.record_invocation(turn_id, &call, &result.output);
                Ok(result)
            }
            _ => {
                let err = Self::not_found_err(&call.tool_id);
                inner.runs.record_invocation_err(turn_id, &call, &err);
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
