use neuromancer_core::trigger::TriggerType;
use serde::{Deserialize, Serialize};

use crate::orchestrator::security::audit::AuditVerdict;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeProposalKind {
    ConfigChange,
    SkillAdd,
    SkillUpdate,
    AgentAdd,
    AgentUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    ProposalCreated,
    VerificationPassed,
    VerificationFailed,
    AuditPassed,
    AuditBlocked,
    AwaitingAdminMessage,
    Authorized,
    AppliedCanary,
    Promoted,
    RolledBack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub passed: bool,
    pub issues: Vec<String>,
    pub blocked_by_guard: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalAuthorization {
    pub authorized: bool,
    pub force: bool,
    pub authorized_at: Option<String>,
    pub authorized_trigger_type: Option<TriggerType>,
}

impl Default for ProposalAuthorization {
    fn default() -> Self {
        Self {
            authorized: false,
            force: false,
            authorized_at: None,
            authorized_trigger_type: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalApplyResult {
    pub promoted: bool,
    pub rolled_back: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeProposal {
    pub proposal_id: String,
    pub proposal_hash: String,
    pub kind: ChangeProposalKind,
    pub target_id: Option<String>,
    pub payload: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
    pub state: ProposalState,
    pub lifecycle: Vec<ProposalState>,
    pub verification_report: VerificationReport,
    pub audit_verdict: AuditVerdict,
    pub authorization: ProposalAuthorization,
    pub apply_result: Option<ProposalApplyResult>,
}

impl ChangeProposal {
    pub fn required_safeguards(&self) -> &[String] {
        &self.audit_verdict.required_safeguards
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillQualityStats {
    pub invocations: u64,
    pub failures: u64,
}
