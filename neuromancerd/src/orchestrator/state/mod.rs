//! Broker state: tool specs, sub-agent refs, runs, threads, proposals, self-improvement config.

pub(crate) mod agent_registry;
pub(crate) mod proposal_store;
pub(crate) mod run_tracker;
pub(crate) mod self_improvement;
pub(crate) mod thread_registry;
pub(crate) mod turn_context;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::config::SelfImprovementConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{
    DelegatedRun, TurnDelegation, OrchestratorOutputItem, OrchestratorToolInvocation,
};
use neuromancer_core::tool::{ToolSource, ToolSpec};
use neuromancer_core::trigger::TriggerType;
use tokio::sync::Mutex as AsyncMutex;

use crate::orchestrator::tool_id::System0ToolId;
use crate::orchestrator::proposals::lifecycle::{new_proposal, transition};
use crate::orchestrator::proposals::model::{
    ChangeProposal, ChangeProposalKind, ProposalState,
};
use crate::orchestrator::proposals::verification::verify_proposal;
use crate::orchestrator::security::audit::{AuditRiskLevel, audit_proposal};
use crate::orchestrator::security::execution_guard::ExecutionGuard;
use crate::orchestrator::tools::default_system0_tools;
use crate::orchestrator::tracing::thread_journal::{ThreadJournal, now_rfc3339};

pub(crate) use agent_registry::AgentRegistry;
pub(crate) use proposal_store::ProposalStore;
pub(crate) use run_tracker::RunTracker;
pub(crate) use self_improvement::SelfImprovementState;
pub(crate) use thread_registry::ThreadRegistry;
pub(crate) use turn_context::TurnContext;

pub(crate) const SYSTEM0_AGENT_ID: &str = "system0";

#[derive(Debug, Clone)]
pub(crate) struct SubAgentThreadState {
    pub(crate) thread_id: String,
    pub(crate) agent_id: String,
    pub(crate) session_id: AgentSessionId,
    pub(crate) latest_run_id: Option<String>,
    pub(crate) state: String,
    pub(crate) summary: Option<String>,
    pub(crate) initial_instruction: Option<String>,
    pub(crate) resurrected: bool,
    pub(crate) active: bool,
    pub(crate) updated_at: String,
    pub(crate) persisted_message_count: usize,
}

#[derive(Clone)]
pub(crate) struct System0ToolBroker {
    pub(crate) inner: Arc<AsyncMutex<System0BrokerInner>>,
}

pub(crate) struct System0BrokerInner {
    pub(crate) turn: TurnContext,
    pub(crate) agents: AgentRegistry,
    pub(crate) runs: RunTracker,
    pub(crate) threads: ThreadRegistry,
    pub(crate) proposals: ProposalStore,
    pub(crate) improvement: SelfImprovementState,
}

impl System0ToolBroker {
    pub(crate) fn new(
        subagents: HashMap<String, Arc<AgentRuntime>>,
        config_snapshot: serde_json::Value,
        allowlisted_tools: &[String],
        session_store: InMemorySessionStore,
        thread_journal: ThreadJournal,
        self_improvement: SelfImprovementConfig,
        known_skill_ids: &[String],
        execution_guard: Arc<dyn ExecutionGuard>,
    ) -> Self {
        let allowlisted_tools: HashSet<String> = if allowlisted_tools.is_empty() {
            default_system0_tools().into_iter().collect()
        } else {
            allowlisted_tools.iter().cloned().collect()
        };

        Self {
            inner: Arc::new(AsyncMutex::new(System0BrokerInner {
                turn: TurnContext::new(),
                agents: AgentRegistry {
                    subagents,
                    session_store,
                    execution_guard,
                },
                runs: RunTracker::new(),
                threads: ThreadRegistry {
                    thread_states: HashMap::new(),
                    thread_journal,
                },
                proposals: ProposalStore::new(),
                improvement: SelfImprovementState::new(
                    self_improvement,
                    allowlisted_tools,
                    known_skill_ids.iter().cloned().collect(),
                    config_snapshot,
                ),
            })),
        }
    }

    pub(crate) async fn set_turn_context(
        &self,
        turn_id: uuid::Uuid,
        trigger_type: TriggerType,
        user_query: String,
    ) {
        let mut inner = self.inner.lock().await;
        inner.turn.current_turn_id = turn_id;
        inner.turn.current_trigger_type = trigger_type;
        inner.turn.current_turn_user_query = user_query;
        inner.runs.delegations_by_turn.remove(&turn_id);
        inner.runs.tool_invocations_by_turn.remove(&turn_id);
    }

    pub(crate) async fn take_delegations(&self, turn_id: uuid::Uuid) -> Vec<TurnDelegation> {
        let mut inner = self.inner.lock().await;
        inner
            .runs
            .delegations_by_turn
            .remove(&turn_id)
            .unwrap_or_default()
    }

    pub(crate) async fn take_tool_invocations(
        &self,
        turn_id: uuid::Uuid,
    ) -> Vec<OrchestratorToolInvocation> {
        let mut inner = self.inner.lock().await;
        inner
            .runs
            .tool_invocations_by_turn
            .remove(&turn_id)
            .unwrap_or_default()
    }

    pub(crate) async fn list_runs(&self) -> Vec<DelegatedRun> {
        let inner = self.inner.lock().await;
        inner
            .runs
            .runs_order
            .iter()
            .filter_map(|run_id| inner.runs.runs_index.get(run_id).cloned())
            .collect()
    }

    pub(crate) async fn get_run(&self, run_id: &str) -> Option<DelegatedRun> {
        let inner = self.inner.lock().await;
        inner.runs.runs_index.get(run_id).cloned()
    }

    pub(crate) async fn session_store(&self) -> InMemorySessionStore {
        let inner = self.inner.lock().await;
        inner.agents.session_store.clone()
    }

    pub(crate) async fn runtime_for_agent(&self, agent_id: &str) -> Option<Arc<AgentRuntime>> {
        let inner = self.inner.lock().await;
        inner.agents.subagents.get(agent_id).cloned()
    }

    pub(crate) async fn list_thread_states(&self) -> Vec<SubAgentThreadState> {
        let inner = self.inner.lock().await;
        let mut states = inner
            .threads
            .thread_states
            .values()
            .cloned()
            .collect::<Vec<_>>();
        states.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        states
    }

    pub(crate) async fn get_thread_state(&self, thread_id: &str) -> Option<SubAgentThreadState> {
        let inner = self.inner.lock().await;
        inner.threads.thread_states.get(thread_id).cloned()
    }

    pub(crate) async fn upsert_thread_state(&self, state: SubAgentThreadState) {
        let mut inner = self.inner.lock().await;
        inner
            .threads
            .thread_states
            .insert(state.thread_id.clone(), state);
    }

    pub(crate) async fn pull_outputs(&self, limit: usize) -> (Vec<OrchestratorOutputItem>, usize) {
        let mut inner = self.inner.lock().await;
        let take = limit.max(1).min(inner.runs.pending_output_queue.len());
        let mut outputs = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(item) = inner.runs.pending_output_queue.pop_front() {
                outputs.push(item);
            }
        }
        let remaining = inner.runs.pending_output_queue.len();
        (outputs, remaining)
    }

    pub(crate) async fn push_output(&self, item: OrchestratorOutputItem) {
        let mut inner = self.inner.lock().await;
        inner.runs.push_output(item);
    }

    pub(crate) async fn record_subagent_turn_result(
        &self,
        thread_id: &str,
        run: DelegatedRun,
        persisted_message_count: usize,
    ) {
        let mut inner = self.inner.lock().await;
        inner
            .runs
            .runs_index
            .insert(run.run_id.clone(), run.clone());
        if !inner.runs.runs_order.iter().any(|id| id == &run.run_id) {
            inner.runs.runs_order.push(run.run_id.clone());
        }
        if let Some(state) = inner.threads.thread_states.get_mut(thread_id) {
            state.latest_run_id = Some(run.run_id.clone());
            state.state = run.state.clone();
            state.summary = run.summary.clone();
            if state.initial_instruction.is_none() {
                state.initial_instruction = run.initial_instruction.clone();
            }
            state.active = true;
            state.updated_at = now_rfc3339();
            state.persisted_message_count = persisted_message_count;
        }
    }

    pub(crate) fn build_tool_specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                id: "delegate_to_agent".to_string(),
                name: "delegate_to_agent".to_string(),
                description:
                    "Delegate an instruction to a configured sub-agent. args: {agent_id, instruction}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "instruction": {"type": "string"}
                    },
                    "required": ["agent_id", "instruction"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "list_agents".to_string(),
                name: "list_agents".to_string(),
                description: "List configured sub-agents.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "read_config".to_string(),
                name: "read_config".to_string(),
                description: "Read orchestrator configuration snapshot.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "queue_status".to_string(),
                name: "queue_status".to_string(),
                description: "Get delegation queue/run status and pending output counts.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "list_proposals".to_string(),
                name: "list_proposals".to_string(),
                description: "List self-improvement change proposals and state.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "get_proposal".to_string(),
                name: "get_proposal".to_string(),
                description: "Get one proposal by id.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "proposal_id": {"type": "string"}
                    },
                    "required": ["proposal_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "propose_config_change".to_string(),
                name: "propose_config_change".to_string(),
                description:
                    "Create a config change proposal. args: {patch, required_safeguards?}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "patch": {"type": "string"},
                        "simulate_regression": {"type": "boolean"},
                        "required_safeguards": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    },
                    "required": ["patch"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "propose_skill_add".to_string(),
                name: "propose_skill_add".to_string(),
                description: "Create a skill add proposal. args: {skill_id, content, required_safeguards?}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string"},
                        "content": {"type": "string"},
                        "simulate_regression": {"type": "boolean"},
                        "required_safeguards": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    },
                    "required": ["skill_id", "content"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "propose_skill_update".to_string(),
                name: "propose_skill_update".to_string(),
                description: "Create a skill update proposal. args: {skill_id, patch, required_safeguards?}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string"},
                        "patch": {"type": "string"},
                        "simulate_regression": {"type": "boolean"},
                        "required_safeguards": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    },
                    "required": ["skill_id", "patch"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "propose_agent_add".to_string(),
                name: "propose_agent_add".to_string(),
                description: "Create an agent add proposal. args: {agent_id, patch, required_safeguards?}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "patch": {"type": "string"},
                        "simulate_regression": {"type": "boolean"},
                        "required_safeguards": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    },
                    "required": ["agent_id", "patch"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "propose_agent_update".to_string(),
                name: "propose_agent_update".to_string(),
                description: "Create an agent update proposal. args: {agent_id, patch, required_safeguards?}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string"},
                        "patch": {"type": "string"},
                        "simulate_regression": {"type": "boolean"},
                        "required_safeguards": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    },
                    "required": ["agent_id", "patch"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "authorize_proposal".to_string(),
                name: "authorize_proposal".to_string(),
                description: "Authorize a proposal (admin trigger required). args: {proposal_id, force?}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "proposal_id": {"type": "string"},
                        "force": {"type": "boolean"}
                    },
                    "required": ["proposal_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "apply_authorized_proposal".to_string(),
                name: "apply_authorized_proposal".to_string(),
                description: "Apply an authorized proposal with canary+rollback (admin trigger required). args: {proposal_id}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "proposal_id": {"type": "string"}
                    },
                    "required": ["proposal_id"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "analyze_failures".to_string(),
                name: "analyze_failures".to_string(),
                description: "Cluster recent delegated run failures for self-improvement.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "score_skills".to_string(),
                name: "score_skills".to_string(),
                description: "Score skill quality for stale/poor performers.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "adapt_routing".to_string(),
                name: "adapt_routing".to_string(),
                description: "Recommend policy-bounded routing updates from observed outcomes.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "record_lesson".to_string(),
                name: "record_lesson".to_string(),
                description: "Record a lesson-learned item into the system lessons partition.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "lesson": {"type": "string"}
                    },
                    "required": ["lesson"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "run_redteam_eval".to_string(),
                name: "run_redteam_eval".to_string(),
                description: "Run a lightweight continuous red-team evaluation report.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "list_audit_records".to_string(),
                name: "list_audit_records".to_string(),
                description: "List mutation audit records.".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
            ToolSpec {
                id: "modify_skill".to_string(),
                name: "modify_skill".to_string(),
                description:
                    "Modify a managed skill definition (admin trigger required). args: {skill_id, patch}".to_string(),
                parameters_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string"},
                        "patch": {"type": "string"}
                    },
                    "required": ["skill_id", "patch"],
                    "additionalProperties": false
                }),
                source: ToolSource::Builtin,
            },
        ]
    }

    pub(crate) fn visible_tool_specs(
        inner: &System0BrokerInner,
        allowed_tools: &[String],
    ) -> Vec<ToolSpec> {
        let allowed_from_context: HashSet<String> = if allowed_tools.is_empty() {
            inner.improvement.allowlisted_tools.clone()
        } else {
            allowed_tools.iter().cloned().collect()
        };

        Self::build_tool_specs()
            .into_iter()
            .filter(|spec| {
                inner.improvement.allowlisted_tools.contains(&spec.id)
                    && allowed_from_context.contains(&spec.id)
                    && (inner.improvement.config.enabled
                        || !System0ToolId::try_from(spec.id.as_str())
                            .is_ok_and(|id| id.is_self_improvement()))
            })
            .collect()
    }

    pub(crate) fn create_proposal(
        inner: &mut System0BrokerInner,
        kind: ChangeProposalKind,
        target_id: Option<String>,
        payload: serde_json::Value,
    ) -> ChangeProposal {
        let mut proposal = new_proposal(kind.clone(), target_id, payload);

        let subagent_ids = inner.agents.subagents.keys().cloned().collect::<HashSet<_>>();
        let managed_agent_ids = inner
            .improvement
            .managed_agents
            .keys()
            .cloned()
            .collect::<HashSet<_>>();
        let verification = verify_proposal(
            &kind,
            proposal.target_id.as_deref(),
            &proposal.payload,
            &inner.improvement.known_skill_ids,
            &inner.improvement.managed_skills,
            &inner.improvement.config_snapshot,
            &subagent_ids,
            &managed_agent_ids,
        );
        proposal.verification_report = verification.clone();
        if verification.passed {
            transition(&mut proposal, ProposalState::VerificationPassed);
        } else {
            transition(&mut proposal, ProposalState::VerificationFailed);
        }

        proposal.audit_verdict = audit_proposal(&kind, &proposal.payload, &verification);
        if proposal.audit_verdict.allow {
            transition(&mut proposal, ProposalState::AuditPassed);
        } else {
            transition(&mut proposal, ProposalState::AuditBlocked);
        }

        if let Err(err) = inner.agents.execution_guard.pre_verify_proposal(&proposal) {
            proposal.verification_report.passed = false;
            proposal.verification_report.blocked_by_guard = true;
            proposal.verification_report.issues.push(err.to_string());
            proposal.audit_verdict.allow = false;
            proposal.audit_verdict.risk_level = AuditRiskLevel::Critical;
            proposal
                .audit_verdict
                .exploitability_notes
                .push("execution guard blocked proposal".to_string());
            transition(&mut proposal, ProposalState::VerificationFailed);
            transition(&mut proposal, ProposalState::AuditBlocked);
        }

        if proposal.verification_report.passed && proposal.audit_verdict.allow {
            transition(&mut proposal, ProposalState::AwaitingAdminMessage);
        }

        if matches!(
            &proposal.kind,
            ChangeProposalKind::SkillAdd | ChangeProposalKind::SkillUpdate
        ) && let Some(skill_id) = proposal.target_id.as_deref()
        {
            let stat = inner
                .improvement
                .skill_quality_stats
                .entry(skill_id.to_string())
                .or_default();
            stat.invocations += 1;
            if !proposal.verification_report.passed || !proposal.audit_verdict.allow {
                stat.failures += 1;
            }
        }

        proposal
    }

    pub(crate) fn not_found_err(tool_id: &str) -> NeuromancerError {
        NeuromancerError::Tool(ToolError::NotFound {
            tool_id: tool_id.to_string(),
        })
    }
}
