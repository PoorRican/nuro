use chrono::Utc;

use crate::orchestrator::proposals::model::{
    ChangeProposal, ChangeProposalKind, ProposalAuthorization, ProposalState,
};
use crate::orchestrator::security::audit::{AuditRiskLevel, AuditVerdict};

pub fn transition(proposal: &mut ChangeProposal, state: ProposalState) {
    proposal.updated_at = Utc::now().to_rfc3339();
    proposal.state = state.clone();
    proposal.lifecycle.push(state);
}

pub fn proposal_hash(
    kind: &ChangeProposalKind,
    target_id: Option<&str>,
    payload: &serde_json::Value,
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    format!("{kind:?}").hash(&mut hasher);
    target_id.unwrap_or_default().hash(&mut hasher);
    payload.to_string().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub fn new_proposal(
    kind: ChangeProposalKind,
    target_id: Option<String>,
    payload: serde_json::Value,
) -> ChangeProposal {
    let now = Utc::now().to_rfc3339();
    let proposal_id = uuid::Uuid::new_v4().to_string();
    let proposal_hash = proposal_hash(&kind, target_id.as_deref(), &payload);

    ChangeProposal {
        proposal_id,
        proposal_hash,
        kind,
        target_id,
        payload,
        created_at: now.clone(),
        updated_at: now,
        state: ProposalState::ProposalCreated,
        lifecycle: vec![ProposalState::ProposalCreated],
        verification_report: crate::orchestrator::proposals::model::VerificationReport {
            passed: true,
            issues: Vec::new(),
            blocked_by_guard: false,
        },
        audit_verdict: AuditVerdict {
            risk_level: AuditRiskLevel::Low,
            exploitability_notes: Vec::new(),
            allow: true,
            required_safeguards: Vec::new(),
        },
        authorization: ProposalAuthorization::default(),
        apply_result: None,
    }
}
