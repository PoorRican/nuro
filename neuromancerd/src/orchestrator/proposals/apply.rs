use std::collections::{HashMap, HashSet};

use crate::orchestrator::error::OrchestratorRuntimeError;
use crate::orchestrator::proposals::model::{ChangeProposal, ChangeProposalKind};

pub fn apply_proposal_mutation(
    subagent_ids: &HashSet<String>,
    managed_agent_ids: &HashSet<String>,
    known_skill_ids: &mut HashSet<String>,
    managed_skills: &mut HashMap<String, String>,
    managed_agents: &mut HashMap<String, serde_json::Value>,
    config_snapshot: &mut serde_json::Value,
    config_patch_history: &mut Vec<String>,
    proposal: &ChangeProposal,
) -> Result<(), OrchestratorRuntimeError> {
    match &proposal.kind {
        ChangeProposalKind::ConfigChange => {
            let patch = proposal
                .payload
                .get("patch")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    OrchestratorRuntimeError::InvalidRequest(
                        "config proposal missing patch".to_string(),
                    )
                })?
                .to_string();
            config_patch_history.push(patch.clone());
            if let Some(root) = config_snapshot.as_object_mut() {
                root.insert(
                    "last_applied_config_patch".to_string(),
                    serde_json::Value::String(patch),
                );
            }
        }
        ChangeProposalKind::SkillAdd => {
            let skill_id = proposal.target_id.as_deref().ok_or_else(|| {
                OrchestratorRuntimeError::InvalidRequest(
                    "skill add proposal missing skill_id".to_string(),
                )
            })?;
            let content = proposal
                .payload
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    OrchestratorRuntimeError::InvalidRequest(
                        "skill add proposal missing content".to_string(),
                    )
                })?
                .to_string();
            managed_skills.insert(skill_id.to_string(), content);
            known_skill_ids.insert(skill_id.to_string());
        }
        ChangeProposalKind::SkillUpdate => {
            let skill_id = proposal.target_id.as_deref().ok_or_else(|| {
                OrchestratorRuntimeError::InvalidRequest(
                    "skill update proposal missing skill_id".to_string(),
                )
            })?;
            if !known_skill_ids.contains(skill_id) && !managed_skills.contains_key(skill_id) {
                return Err(OrchestratorRuntimeError::InvalidRequest(format!(
                    "skill '{}' does not exist",
                    skill_id
                )));
            }
            let patch = proposal
                .payload
                .get("patch")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    OrchestratorRuntimeError::InvalidRequest(
                        "skill update proposal missing patch".to_string(),
                    )
                })?
                .to_string();
            managed_skills.insert(skill_id.to_string(), patch);
            known_skill_ids.insert(skill_id.to_string());
        }
        ChangeProposalKind::AgentAdd => {
            let agent_id = proposal.target_id.as_deref().ok_or_else(|| {
                OrchestratorRuntimeError::InvalidRequest(
                    "agent add proposal missing agent_id".to_string(),
                )
            })?;
            let patch = proposal
                .payload
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let parsed = serde_json::from_str::<serde_json::Value>(patch).unwrap_or_else(|_| {
                serde_json::json!({
                    "patch": patch,
                })
            });
            managed_agents.insert(agent_id.to_string(), parsed);
        }
        ChangeProposalKind::AgentUpdate => {
            let agent_id = proposal.target_id.as_deref().ok_or_else(|| {
                OrchestratorRuntimeError::InvalidRequest(
                    "agent update proposal missing agent_id".to_string(),
                )
            })?;
            if !subagent_ids.contains(agent_id) && !managed_agent_ids.contains(agent_id) {
                return Err(OrchestratorRuntimeError::InvalidRequest(format!(
                    "agent '{}' does not exist",
                    agent_id
                )));
            }
            let patch = proposal
                .payload
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let parsed = serde_json::from_str::<serde_json::Value>(patch).unwrap_or_else(|_| {
                serde_json::json!({
                    "patch": patch,
                })
            });
            managed_agents.insert(agent_id.to_string(), parsed);
        }
    }

    Ok(())
}
