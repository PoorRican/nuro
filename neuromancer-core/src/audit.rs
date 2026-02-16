use serde::{Deserialize, Serialize};

use crate::trigger::TriggerType;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditRiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditVerdict {
    pub risk_level: AuditRiskLevel,
    pub exploitability_notes: Vec<String>,
    pub allow: bool,
    pub required_safeguards: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationAuditRecord {
    pub at: String,
    pub trigger_type: TriggerType,
    pub proposal_id: Option<String>,
    pub proposal_hash: Option<String>,
    pub action: String,
    pub outcome: String,
    pub details: serde_json::Value,
}
