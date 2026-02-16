use std::collections::HashSet;

use crate::orchestrator::error::System0Error;
use crate::orchestrator::proposals::model::{ChangeProposal, ChangeProposalKind};
use crate::orchestrator::state::SelfImprovementState;

pub fn apply_proposal_mutation(
    subagent_ids: &HashSet<String>,
    improvement: &mut SelfImprovementState,
    proposal: &ChangeProposal,
) -> Result<(), System0Error> {
    let managed_agent_ids: HashSet<String> =
        improvement.managed_agents.keys().cloned().collect();

    match &proposal.kind {
        ChangeProposalKind::ConfigChange => {
            let patch = proposal
                .payload
                .get("patch")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    System0Error::InvalidRequest(
                        "config proposal missing patch".to_string(),
                    )
                })?
                .to_string();
            improvement.config_patch_history.push(patch.clone());
            if let Some(root) = improvement.config_snapshot.as_object_mut() {
                root.insert(
                    "last_applied_config_patch".to_string(),
                    serde_json::Value::String(patch),
                );
            }
        }
        ChangeProposalKind::SkillAdd => {
            let skill_id = proposal.target_id.as_deref().ok_or_else(|| {
                System0Error::InvalidRequest(
                    "skill add proposal missing skill_id".to_string(),
                )
            })?;
            let content = proposal
                .payload
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    System0Error::InvalidRequest(
                        "skill add proposal missing content".to_string(),
                    )
                })?
                .to_string();
            improvement.managed_skills.insert(skill_id.to_string(), content);
            improvement.known_skill_ids.insert(skill_id.to_string());
        }
        ChangeProposalKind::SkillUpdate => {
            let skill_id = proposal.target_id.as_deref().ok_or_else(|| {
                System0Error::InvalidRequest(
                    "skill update proposal missing skill_id".to_string(),
                )
            })?;
            if !improvement.known_skill_ids.contains(skill_id)
                && !improvement.managed_skills.contains_key(skill_id)
            {
                return Err(System0Error::InvalidRequest(format!(
                    "skill '{}' does not exist",
                    skill_id
                )));
            }
            let patch = proposal
                .payload
                .get("patch")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    System0Error::InvalidRequest(
                        "skill update proposal missing patch".to_string(),
                    )
                })?
                .to_string();
            improvement.managed_skills.insert(skill_id.to_string(), patch);
            improvement.known_skill_ids.insert(skill_id.to_string());
        }
        ChangeProposalKind::AgentAdd => {
            let agent_id = proposal.target_id.as_deref().ok_or_else(|| {
                System0Error::InvalidRequest(
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
            improvement.managed_agents.insert(agent_id.to_string(), parsed);
        }
        ChangeProposalKind::AgentUpdate => {
            let agent_id = proposal.target_id.as_deref().ok_or_else(|| {
                System0Error::InvalidRequest(
                    "agent update proposal missing agent_id".to_string(),
                )
            })?;
            if !subagent_ids.contains(agent_id) && !managed_agent_ids.contains(agent_id) {
                return Err(System0Error::InvalidRequest(format!(
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
            improvement.managed_agents.insert(agent_id.to_string(), parsed);
        }
    }

    Ok(())
}
