use std::collections::{HashMap, HashSet};

use neuromancer_skills::{LintSeverity, SkillLinter};

use crate::orchestrator::proposals::model::{ChangeProposalKind, VerificationReport};

pub fn known_skills(
    known_skill_ids: &HashSet<String>,
    managed_skills: &HashMap<String, String>,
    config_snapshot: &serde_json::Value,
) -> HashSet<String> {
    let mut skills = known_skill_ids.clone();
    skills.extend(managed_skills.keys().cloned());

    if let Some(agents) = config_snapshot.get("agents").and_then(|v| v.as_object()) {
        for agent_cfg in agents.values() {
            if let Some(agent_skills) = agent_cfg
                .get("capabilities")
                .and_then(|cap| cap.get("skills"))
                .and_then(|skills| skills.as_array())
            {
                for skill in agent_skills.iter().filter_map(|v| v.as_str()) {
                    skills.insert(skill.to_string());
                }
            }
        }
    }

    skills
}

pub fn verify_proposal(
    kind: &ChangeProposalKind,
    target_id: Option<&str>,
    payload: &serde_json::Value,
    known_skill_ids: &HashSet<String>,
    managed_skills: &HashMap<String, String>,
    config_snapshot: &serde_json::Value,
    subagent_ids: &HashSet<String>,
    managed_agent_ids: &HashSet<String>,
) -> VerificationReport {
    let mut issues = Vec::new();

    match kind {
        ChangeProposalKind::ConfigChange => {
            let patch = payload
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if patch.is_empty() {
                issues.push("config patch must not be empty".to_string());
            }
        }
        ChangeProposalKind::SkillAdd => {
            let skill_id = target_id.unwrap_or_default();
            if known_skill_ids.contains(skill_id) {
                issues.push(format!("skill '{}' already exists", skill_id));
            }
            let content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if content.trim().is_empty() {
                issues.push("skill content must not be empty".to_string());
            } else {
                let linter = SkillLinter::new();
                let lint = linter.lint(skill_id, content);
                for warning in lint.warnings {
                    if warning.severity == LintSeverity::Error {
                        issues.push(format!(
                            "skill lint error (line {:?}): {}",
                            warning.line, warning.description
                        ));
                    }
                }
            }
        }
        ChangeProposalKind::SkillUpdate => {
            let skill_id = target_id.unwrap_or_default();
            if !known_skills(known_skill_ids, managed_skills, config_snapshot).contains(skill_id) {
                issues.push(format!("skill '{}' does not exist", skill_id));
            }
            let patch = payload
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if patch.trim().is_empty() {
                issues.push("skill patch must not be empty".to_string());
            } else {
                let linter = SkillLinter::new();
                let lint = linter.lint(skill_id, patch);
                for warning in lint.warnings {
                    if warning.severity == LintSeverity::Error {
                        issues.push(format!(
                            "skill lint error (line {:?}): {}",
                            warning.line, warning.description
                        ));
                    }
                }
            }
        }
        ChangeProposalKind::AgentAdd => {
            let agent_id = target_id.unwrap_or_default();
            if subagent_ids.contains(agent_id) || managed_agent_ids.contains(agent_id) {
                issues.push(format!("agent '{}' already exists", agent_id));
            }
            let patch = payload
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if patch.trim().is_empty() {
                issues.push("agent patch must not be empty".to_string());
            }
            validate_agent_patch_skills(
                patch,
                &known_skills(known_skill_ids, managed_skills, config_snapshot),
                &mut issues,
            );
        }
        ChangeProposalKind::AgentUpdate => {
            let agent_id = target_id.unwrap_or_default();
            if !subagent_ids.contains(agent_id) && !managed_agent_ids.contains(agent_id) {
                issues.push(format!("agent '{}' does not exist", agent_id));
            }
            let patch = payload
                .get("patch")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if patch.trim().is_empty() {
                issues.push("agent patch must not be empty".to_string());
            }
            validate_agent_patch_skills(
                patch,
                &known_skills(known_skill_ids, managed_skills, config_snapshot),
                &mut issues,
            );
        }
    }

    VerificationReport {
        passed: issues.is_empty(),
        issues,
        blocked_by_guard: false,
    }
}

pub fn validate_agent_patch_skills(
    patch: &str,
    known_skills: &HashSet<String>,
    issues: &mut Vec<String>,
) {
    let Ok(patch_value) = serde_json::from_str::<serde_json::Value>(patch) else {
        return;
    };

    let Some(skills) = patch_value
        .get("capabilities")
        .and_then(|c| c.get("skills"))
        .and_then(|s| s.as_array())
    else {
        return;
    };

    for skill in skills.iter().filter_map(|v| v.as_str()) {
        if !known_skills.contains(skill) {
            issues.push(format!(
                "agent patch references unknown skill '{}' (central config scope violation)",
                skill
            ));
        }
    }
}
