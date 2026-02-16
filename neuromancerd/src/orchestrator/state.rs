use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use neuromancer_agent::runtime::AgentRuntime;
use neuromancer_agent::session::{AgentSessionId, InMemorySessionStore};
use neuromancer_core::config::SelfImprovementConfig;
use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::rpc::{
    DelegatedRun, DelegatedTask, OrchestratorOutputItem, OrchestratorToolInvocation,
};
use neuromancer_core::tool::{ToolCall, ToolOutput, ToolSource, ToolSpec};
use neuromancer_core::trigger::TriggerType;
use tokio::sync::Mutex as AsyncMutex;

use crate::orchestrator::actions::dispatch::is_self_improvement_tool;
use crate::orchestrator::proposals::lifecycle::{new_proposal, transition};
use crate::orchestrator::proposals::model::{
    ChangeProposal, ChangeProposalKind, ProposalState, SkillQualityStats,
};
use crate::orchestrator::proposals::verification::verify_proposal;
use crate::orchestrator::security::audit::{
    AuditRiskLevel, MutationAuditRecord, audit_proposal, mutation_audit_record,
};
use crate::orchestrator::security::execution_guard::ExecutionGuard;
use crate::orchestrator::tools::default_system0_tools;
use crate::orchestrator::tracing::thread_journal::{ThreadJournal, now_rfc3339};

pub(crate) const SYSTEM0_AGENT_ID: &str = "system0";
const OUTPUT_QUEUE_MAX_ITEMS: usize = 1_000;

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
    pub(crate) subagents: HashMap<String, Arc<AgentRuntime>>,
    pub(crate) config_snapshot: serde_json::Value,
    pub(crate) allowlisted_tools: HashSet<String>,
    pub(crate) known_skill_ids: HashSet<String>,
    pub(crate) self_improvement: SelfImprovementConfig,
    pub(crate) execution_guard: Arc<dyn ExecutionGuard>,
    pub(crate) session_store: InMemorySessionStore,
    pub(crate) thread_journal: ThreadJournal,
    pub(crate) current_trigger_type: TriggerType,
    pub(crate) current_turn_id: uuid::Uuid,
    pub(crate) current_turn_user_query: String,
    pub(crate) delegated_tasks_by_turn: HashMap<uuid::Uuid, Vec<DelegatedTask>>,
    pub(crate) tool_invocations_by_turn: HashMap<uuid::Uuid, Vec<OrchestratorToolInvocation>>,
    pub(crate) runs_index: HashMap<String, DelegatedRun>,
    pub(crate) runs_order: Vec<String>,
    pub(crate) thread_states: HashMap<String, SubAgentThreadState>,
    pub(crate) pending_output_queue: VecDeque<OrchestratorOutputItem>,
    pub(crate) proposals_index: HashMap<String, ChangeProposal>,
    pub(crate) proposals_order: Vec<String>,
    pub(crate) mutation_audit_log: Vec<MutationAuditRecord>,
    pub(crate) managed_skills: HashMap<String, String>,
    pub(crate) managed_agents: HashMap<String, serde_json::Value>,
    pub(crate) config_patch_history: Vec<String>,
    pub(crate) lessons_learned: Vec<String>,
    pub(crate) skill_quality_stats: HashMap<String, SkillQualityStats>,
    pub(crate) last_known_good_snapshot: serde_json::Value,
}

// TODO: there seems to be a divergence between "runs" and "tasks"
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
        let allowlisted_tools = if allowlisted_tools.is_empty() {
            default_system0_tools().into_iter().collect()
        } else {
            allowlisted_tools.iter().cloned().collect()
        };

        Self {
            inner: Arc::new(AsyncMutex::new(System0BrokerInner {
                subagents,
                config_snapshot: config_snapshot.clone(),
                allowlisted_tools,
                known_skill_ids: known_skill_ids.iter().cloned().collect(),
                self_improvement,
                execution_guard,
                session_store,
                thread_journal,
                current_trigger_type: TriggerType::Admin,
                current_turn_id: uuid::Uuid::nil(),
                current_turn_user_query: String::new(),
                delegated_tasks_by_turn: HashMap::new(),
                tool_invocations_by_turn: HashMap::new(),
                runs_index: HashMap::new(),
                runs_order: Vec::new(),
                thread_states: HashMap::new(),
                pending_output_queue: VecDeque::new(),
                proposals_index: HashMap::new(),
                proposals_order: Vec::new(),
                mutation_audit_log: Vec::new(),
                managed_skills: HashMap::new(),
                managed_agents: HashMap::new(),
                config_patch_history: Vec::new(),
                lessons_learned: Vec::new(),
                skill_quality_stats: HashMap::new(),
                last_known_good_snapshot: config_snapshot,
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
        inner.current_turn_id = turn_id;
        inner.current_trigger_type = trigger_type;
        inner.current_turn_user_query = user_query;
        inner.delegated_tasks_by_turn.remove(&turn_id);
        inner.tool_invocations_by_turn.remove(&turn_id);
    }

    pub(crate) async fn take_delegated_tasks(&self, turn_id: uuid::Uuid) -> Vec<DelegatedTask> {
        let mut inner = self.inner.lock().await;
        inner
            .delegated_tasks_by_turn
            .remove(&turn_id)
            .unwrap_or_default()
    }

    pub(crate) async fn take_tool_invocations(
        &self,
        turn_id: uuid::Uuid,
    ) -> Vec<OrchestratorToolInvocation> {
        let mut inner = self.inner.lock().await;
        inner
            .tool_invocations_by_turn
            .remove(&turn_id)
            .unwrap_or_default()
    }

    pub(crate) async fn list_runs(&self) -> Vec<DelegatedRun> {
        let inner = self.inner.lock().await;
        inner
            .runs_order
            .iter()
            .filter_map(|run_id| inner.runs_index.get(run_id).cloned())
            .collect()
    }

    pub(crate) async fn get_run(&self, run_id: &str) -> Option<DelegatedRun> {
        let inner = self.inner.lock().await;
        inner.runs_index.get(run_id).cloned()
    }

    pub(crate) async fn session_store(&self) -> InMemorySessionStore {
        let inner = self.inner.lock().await;
        inner.session_store.clone()
    }

    pub(crate) async fn runtime_for_agent(&self, agent_id: &str) -> Option<Arc<AgentRuntime>> {
        let inner = self.inner.lock().await;
        inner.subagents.get(agent_id).cloned()
    }

    pub(crate) async fn list_thread_states(&self) -> Vec<SubAgentThreadState> {
        let inner = self.inner.lock().await;
        let mut states = inner.thread_states.values().cloned().collect::<Vec<_>>();
        states.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        states
    }

    pub(crate) async fn get_thread_state(&self, thread_id: &str) -> Option<SubAgentThreadState> {
        let inner = self.inner.lock().await;
        inner.thread_states.get(thread_id).cloned()
    }

    pub(crate) async fn upsert_thread_state(&self, state: SubAgentThreadState) {
        let mut inner = self.inner.lock().await;
        inner.thread_states.insert(state.thread_id.clone(), state);
    }

    pub(crate) async fn pull_outputs(&self, limit: usize) -> (Vec<OrchestratorOutputItem>, usize) {
        let mut inner = self.inner.lock().await;
        let take = limit.max(1).min(inner.pending_output_queue.len());
        let mut outputs = Vec::with_capacity(take);
        for _ in 0..take {
            if let Some(item) = inner.pending_output_queue.pop_front() {
                outputs.push(item);
            }
        }
        let remaining = inner.pending_output_queue.len();
        (outputs, remaining)
    }

    pub(crate) async fn push_output(&self, item: OrchestratorOutputItem) {
        let mut inner = self.inner.lock().await;
        if inner.pending_output_queue.len() >= OUTPUT_QUEUE_MAX_ITEMS {
            inner.pending_output_queue.pop_front();
        }
        inner.pending_output_queue.push_back(item);
    }

    pub(crate) async fn record_subagent_turn_result(
        &self,
        thread_id: &str,
        run: DelegatedRun,
        persisted_message_count: usize,
    ) {
        let mut inner = self.inner.lock().await;
        inner.runs_index.insert(run.run_id.clone(), run.clone());
        if !inner.runs_order.iter().any(|id| id == &run.run_id) {
            inner.runs_order.push(run.run_id.clone());
        }
        if let Some(state) = inner.thread_states.get_mut(thread_id) {
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
            inner.allowlisted_tools.clone()
        } else {
            allowed_tools.iter().cloned().collect()
        };

        Self::build_tool_specs()
            .into_iter()
            .filter(|spec| {
                inner.allowlisted_tools.contains(&spec.id)
                    && allowed_from_context.contains(&spec.id)
                    && (inner.self_improvement.enabled || !is_self_improvement_tool(&spec.id))
            })
            .collect()
    }

    pub(crate) fn record_invocation(
        inner: &mut System0BrokerInner,
        turn_id: uuid::Uuid,
        call: &ToolCall,
        output: &ToolOutput,
    ) {
        let (status, rendered_output) = match output {
            ToolOutput::Success(value) => ("success".to_string(), value.clone()),
            ToolOutput::Error(err) => ("error".to_string(), serde_json::json!({ "error": err })),
        };

        inner
            .tool_invocations_by_turn
            .entry(turn_id)
            .or_default()
            .push(OrchestratorToolInvocation {
                call_id: call.id.clone(),
                tool_id: call.tool_id.clone(),
                arguments: call.arguments.clone(),
                status,
                output: rendered_output,
            });
    }

    pub(crate) fn record_invocation_err(
        inner: &mut System0BrokerInner,
        turn_id: uuid::Uuid,
        call: &ToolCall,
        err: &NeuromancerError,
    ) {
        Self::record_invocation(inner, turn_id, call, &ToolOutput::Error(err.to_string()));
    }

    pub(crate) fn record_mutation_audit(
        inner: &mut System0BrokerInner,
        action: &str,
        outcome: &str,
        trigger_type: TriggerType,
        proposal: Option<&ChangeProposal>,
        details: serde_json::Value,
    ) {
        inner.mutation_audit_log.push(mutation_audit_record(
            action,
            outcome,
            trigger_type,
            proposal.as_ref().map(|p| p.proposal_id.as_str()),
            proposal.as_ref().map(|p| p.proposal_hash.as_str()),
            details,
        ));
    }

    pub(crate) fn create_proposal(
        inner: &mut System0BrokerInner,
        kind: ChangeProposalKind,
        target_id: Option<String>,
        payload: serde_json::Value,
    ) -> ChangeProposal {
        let mut proposal = new_proposal(kind.clone(), target_id, payload);

        let subagent_ids = inner.subagents.keys().cloned().collect::<HashSet<_>>();
        let managed_agent_ids = inner.managed_agents.keys().cloned().collect::<HashSet<_>>();
        let verification = verify_proposal(
            &kind,
            proposal.target_id.as_deref(),
            &proposal.payload,
            &inner.known_skill_ids,
            &inner.managed_skills,
            &inner.config_snapshot,
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

        if let Err(err) = inner.execution_guard.pre_verify_proposal(&proposal) {
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
