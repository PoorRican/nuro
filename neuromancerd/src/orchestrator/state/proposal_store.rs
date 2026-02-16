use std::collections::HashMap;

use neuromancer_core::trigger::TriggerType;

use crate::orchestrator::proposals::model::ChangeProposal;
use crate::orchestrator::security::audit::{MutationAuditRecord, mutation_audit_record};

pub(crate) struct ProposalStore {
    pub(crate) proposals_index: HashMap<String, ChangeProposal>,
    pub(crate) proposals_order: Vec<String>,
    pub(crate) mutation_audit_log: Vec<MutationAuditRecord>,
}

impl ProposalStore {
    pub(crate) fn new() -> Self {
        Self {
            proposals_index: HashMap::new(),
            proposals_order: Vec::new(),
            mutation_audit_log: Vec::new(),
        }
    }

    pub(crate) fn record_mutation_audit(
        &mut self,
        action: &str,
        outcome: &str,
        trigger_type: TriggerType,
        proposal: Option<&ChangeProposal>,
        details: serde_json::Value,
    ) {
        self.mutation_audit_log.push(mutation_audit_record(
            action,
            outcome,
            trigger_type,
            proposal.as_ref().map(|p| p.proposal_id.as_str()),
            proposal.as_ref().map(|p| p.proposal_hash.as_str()),
            details,
        ));
    }
}
