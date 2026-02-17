//! Single source of truth for System0 tool identifiers.
//!
//! Replaces the redundant `RUNTIME_TOOL_IDS`, `ADAPTIVE_TOOL_IDS`,
//! `AUTHENTICATED_ADAPTIVE_TOOL_IDS` constants, `ToolClass` enum, and
//! `classify_tool()`/`is_self_improvement_tool()` free functions that
//! previously lived in `dispatch.rs`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeToolId {
    DelegateToAgent,
    ListAgents,
    ReadConfig,
    QueueStatus,
}

impl RuntimeToolId {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DelegateToAgent => "delegate_to_agent",
            Self::ListAgents => "list_agents",
            Self::ReadConfig => "read_config",
            Self::QueueStatus => "queue_status",
        }
    }

    const ALL: &[Self] = &[
        Self::DelegateToAgent,
        Self::ListAgents,
        Self::ReadConfig,
        Self::QueueStatus,
    ];
}

impl TryFrom<&str> for RuntimeToolId {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        match s {
            "delegate_to_agent" => Ok(Self::DelegateToAgent),
            "list_agents" => Ok(Self::ListAgents),
            "read_config" => Ok(Self::ReadConfig),
            "queue_status" => Ok(Self::QueueStatus),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdaptiveToolId {
    ListProposals,
    GetProposal,
    ProposeConfigChange,
    ProposeSkillAdd,
    ProposeSkillUpdate,
    ProposeAgentAdd,
    ProposeAgentUpdate,
    AnalyzeFailures,
    ScoreSkills,
    AdaptRouting,
    RecordLesson,
    RunRedteamEval,
    ListAuditRecords,
}

impl AdaptiveToolId {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ListProposals => "list_proposals",
            Self::GetProposal => "get_proposal",
            Self::ProposeConfigChange => "propose_config_change",
            Self::ProposeSkillAdd => "propose_skill_add",
            Self::ProposeSkillUpdate => "propose_skill_update",
            Self::ProposeAgentAdd => "propose_agent_add",
            Self::ProposeAgentUpdate => "propose_agent_update",
            Self::AnalyzeFailures => "analyze_failures",
            Self::ScoreSkills => "score_skills",
            Self::AdaptRouting => "adapt_routing",
            Self::RecordLesson => "record_lesson",
            Self::RunRedteamEval => "run_redteam_eval",
            Self::ListAuditRecords => "list_audit_records",
        }
    }

    const ALL: &[Self] = &[
        Self::ListProposals,
        Self::GetProposal,
        Self::ProposeConfigChange,
        Self::ProposeSkillAdd,
        Self::ProposeSkillUpdate,
        Self::ProposeAgentAdd,
        Self::ProposeAgentUpdate,
        Self::AnalyzeFailures,
        Self::ScoreSkills,
        Self::AdaptRouting,
        Self::RecordLesson,
        Self::RunRedteamEval,
        Self::ListAuditRecords,
    ];
}

impl TryFrom<&str> for AdaptiveToolId {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        match s {
            "list_proposals" => Ok(Self::ListProposals),
            "get_proposal" => Ok(Self::GetProposal),
            "propose_config_change" => Ok(Self::ProposeConfigChange),
            "propose_skill_add" => Ok(Self::ProposeSkillAdd),
            "propose_skill_update" => Ok(Self::ProposeSkillUpdate),
            "propose_agent_add" => Ok(Self::ProposeAgentAdd),
            "propose_agent_update" => Ok(Self::ProposeAgentUpdate),
            "analyze_failures" => Ok(Self::AnalyzeFailures),
            "score_skills" => Ok(Self::ScoreSkills),
            "adapt_routing" => Ok(Self::AdaptRouting),
            "record_lesson" => Ok(Self::RecordLesson),
            "run_redteam_eval" => Ok(Self::RunRedteamEval),
            "list_audit_records" => Ok(Self::ListAuditRecords),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthenticatedAdaptiveToolId {
    AuthorizeProposal,
    ApplyAuthorizedProposal,
    ModifySkill,
}

impl AuthenticatedAdaptiveToolId {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizeProposal => "authorize_proposal",
            Self::ApplyAuthorizedProposal => "apply_authorized_proposal",
            Self::ModifySkill => "modify_skill",
        }
    }

    const ALL: &[Self] = &[
        Self::AuthorizeProposal,
        Self::ApplyAuthorizedProposal,
        Self::ModifySkill,
    ];
}

impl TryFrom<&str> for AuthenticatedAdaptiveToolId {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        match s {
            "authorize_proposal" => Ok(Self::AuthorizeProposal),
            "apply_authorized_proposal" => Ok(Self::ApplyAuthorizedProposal),
            "modify_skill" => Ok(Self::ModifySkill),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum System0ToolId {
    Runtime(RuntimeToolId),
    Adaptive(AdaptiveToolId),
    AuthenticatedAdaptive(AuthenticatedAdaptiveToolId),
}

impl System0ToolId {
    pub(crate) fn all() -> Vec<String> {
        let mut ids = Vec::new();
        for id in RuntimeToolId::ALL {
            ids.push(id.as_str().to_string());
        }
        for id in AdaptiveToolId::ALL {
            ids.push(id.as_str().to_string());
        }
        for id in AuthenticatedAdaptiveToolId::ALL {
            ids.push(id.as_str().to_string());
        }
        ids
    }

    pub(crate) fn is_self_improvement(self) -> bool {
        matches!(self, Self::Adaptive(_) | Self::AuthenticatedAdaptive(_))
    }
}

impl TryFrom<&str> for System0ToolId {
    type Error = ();
    fn try_from(s: &str) -> Result<Self, ()> {
        if let Ok(id) = RuntimeToolId::try_from(s) {
            return Ok(Self::Runtime(id));
        }
        if let Ok(id) = AdaptiveToolId::try_from(s) {
            return Ok(Self::Adaptive(id));
        }
        if let Ok(id) = AuthenticatedAdaptiveToolId::try_from(s) {
            return Ok(Self::AuthenticatedAdaptive(id));
        }
        Err(())
    }
}
