use chrono::Utc;
use neuromancer_core::trigger::TriggerType;

pub use neuromancer_core::audit::{AuditRiskLevel, AuditVerdict, MutationAuditRecord};

use crate::orchestrator::proposals::model::{ChangeProposalKind, VerificationReport};

pub fn required_safeguards(payload: &serde_json::Value) -> Vec<String> {
    payload
        .get("required_safeguards")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub fn audit_proposal(
    kind: &ChangeProposalKind,
    payload: &serde_json::Value,
    verification: &VerificationReport,
) -> AuditVerdict {
    let mut notes = Vec::new();
    let mut safeguards = required_safeguards(payload);
    let mut risk = AuditRiskLevel::Low;

    if !verification.passed {
        notes.push("verification reported blocking issues".to_string());
        risk = AuditRiskLevel::High;
    }

    let payload_text = payload.to_string().to_ascii_lowercase();
    let critical_markers = [
        "rm -rf /",
        "curl ",
        "| sh",
        "| bash",
        "ignore previous instructions",
        "disregard your system prompt",
    ];
    if critical_markers
        .iter()
        .any(|marker| payload_text.contains(marker))
    {
        notes.push("payload contains critical exploit markers".to_string());
        risk = AuditRiskLevel::Critical;
    }

    if matches!(
        kind,
        ChangeProposalKind::ConfigChange | ChangeProposalKind::AgentUpdate
    ) && risk == AuditRiskLevel::Low
    {
        risk = AuditRiskLevel::Medium;
    }

    if matches!(risk, AuditRiskLevel::High | AuditRiskLevel::Critical) {
        safeguards.push("human_approval".to_string());
    }
    safeguards.sort();
    safeguards.dedup();

    let allow =
        verification.passed && !matches!(risk, AuditRiskLevel::High | AuditRiskLevel::Critical);
    AuditVerdict {
        risk_level: risk,
        exploitability_notes: notes,
        allow,
        required_safeguards: safeguards,
    }
}

pub fn mutation_audit_record(
    action: &str,
    outcome: &str,
    trigger_type: TriggerType,
    proposal_id: Option<&str>,
    proposal_hash: Option<&str>,
    details: serde_json::Value,
) -> MutationAuditRecord {
    MutationAuditRecord {
        at: Utc::now().to_rfc3339(),
        trigger_type,
        proposal_id: proposal_id.map(ToString::to_string),
        proposal_hash: proposal_hash.map(ToString::to_string),
        action: action.to_string(),
        outcome: outcome.to_string(),
        details,
    }
}
