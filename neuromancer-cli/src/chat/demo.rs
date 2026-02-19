use super::SYSTEM_THREAD_ID;
use super::app::{ThreadKind, ThreadView};
use super::timeline::{MessageRoleTag, TimelineItem, TimelineRenderMeta};

const PLANNER_THREAD_ID: &str = "thread-planner-abc12345";
const RESEARCHER_THREAD_ID: &str = "thread-researcher-def67890";

fn meta(seq: u64, minute: u32) -> Option<TimelineRenderMeta> {
    Some(TimelineRenderMeta {
        seq: Some(seq),
        ts: Some(format!("2026-02-15T10:{minute:02}:00Z")),
        redaction_applied: Some(false),
    })
}

pub(super) fn build_demo_threads() -> (Vec<ThreadView>, usize) {
    let system0 = build_system0_thread();
    let planner = build_planner_thread();
    let researcher = build_researcher_thread();
    (vec![system0, planner, researcher], 0)
}

fn build_system0_thread() -> ThreadView {
    let items = vec![
        // 1. System prompt (multi-line, collapsible)
        TimelineItem::text(
            MessageRoleTag::System,
            "You are System0, the orchestrator agent for Neuromancer.\n\
             Your role is to mediate user/admin turns, enforce policy, and delegate to sub-agents.\n\
             Available tools: list_agents, read_config, delegate_to_agent, propose_config_change,\n\
             authorize_proposal, apply_authorized_proposal, modify_skill, score_skills,\n\
             analyze_failures, adapt_routing, record_lesson, run_redteam_eval, list_audit_records.\n\
             \n\
             Security constraints:\n\
             - All mutating lifecycle actions require admin privilege (TriggerType::Admin).\n\
             - Proposals follow: created -> verified -> audited -> authorized -> applied -> promoted.\n\
             - High/critical risk proposals default to blocked unless explicitly forced.\n\
             - ExecutionGuard hooks fail closed with blocked_missing_sandbox.",
        ),
        // 2. Short user message
        TimelineItem::text(
            MessageRoleTag::User,
            "List all available agents and their current status.",
        ),
        // 3. list_agents tool invocation
        TimelineItem::ToolInvocation {
            call_id: "call-demo-001".to_string(),
            tool_id: "list_agents".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({}),
            output: serde_json::json!({
                "agents": [
                    {
                        "agent_id": "planner",
                        "state": "running",
                        "thread_id": PLANNER_THREAD_ID,
                        "run_id": "run-planner-001"
                    },
                    {
                        "agent_id": "researcher",
                        "state": "completed",
                        "thread_id": RESEARCHER_THREAD_ID,
                        "run_id": "run-researcher-001"
                    },
                    {
                        "agent_id": "auditor",
                        "state": "idle",
                        "thread_id": "thread-auditor-ghi34567",
                        "run_id": null
                    }
                ]
            }),
            meta: meta(1, 0),
            expanded: false,
        },
        // 4. Assistant summary
        TimelineItem::text(
            MessageRoleTag::Assistant,
            "Found 3 agents: planner (running), researcher (completed), auditor (idle).",
        ),
        // 5. Long collapsible user message
        TimelineItem::text(
            MessageRoleTag::User,
            "I need a comprehensive audit of the current configuration. Please read the config,\n\
             check for any security concerns, propose any necessary changes, and coordinate with\n\
             the planner agent to develop a remediation timeline.\n\
             \n\
             Specifically, I want to:\n\
             1. Review all model provider settings and verify API key environment variables.\n\
             2. Check the self-improvement configuration thresholds.\n\
             3. Ensure the MCP server pool is correctly sized.\n\
             4. Validate that the orchestrator capabilities allowlist matches current policy.\n\
             5. Verify secret store ACLs are not overly permissive.",
        ),
        // 6. read_config tool
        TimelineItem::ToolInvocation {
            call_id: "call-demo-002".to_string(),
            tool_id: "read_config".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({}),
            output: serde_json::json!({
                "global": {"log_level": "info"},
                "models": {
                    "executor": {"provider": "openai", "model": "gpt-4"},
                    "fast": {"provider": "groq", "model": "llama-70b"}
                },
                "orchestrator": {
                    "model_slot": "executor",
                    "max_iterations": 25,
                    "self_improvement": {"enabled": true}
                },
                "secrets": {"sensitive_payload": "REDACTED"},
                "mcp_servers": {"pool_size": 3},
                "admin_api": {"bind": "127.0.0.1:9090"}
            }),
            meta: meta(2, 1),
            expanded: false,
        },
        // 7. propose_config_change
        TimelineItem::ToolInvocation {
            call_id: "call-demo-003".to_string(),
            tool_id: "propose_config_change".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({
                "section": "orchestrator.self_improvement",
                "changes": {"audit_threshold": 0.85}
            }),
            output: serde_json::json!({
                "proposal_id": "prop-demo-001",
                "state": "verification_passed"
            }),
            meta: meta(3, 2),
            expanded: false,
        },
        // 8. authorize_proposal
        TimelineItem::ToolInvocation {
            call_id: "call-demo-004".to_string(),
            tool_id: "authorize_proposal".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({"proposal_id": "prop-demo-001"}),
            output: serde_json::json!({
                "proposal_id": "prop-demo-001",
                "state": "authorized"
            }),
            meta: meta(4, 3),
            expanded: false,
        },
        // 9. Delegate to planner
        TimelineItem::DelegateInvocation {
            call_id: "call-demo-005".to_string(),
            status: "success".to_string(),
            target_agent: Some("planner".to_string()),
            thread_id: Some(PLANNER_THREAD_ID.to_string()),
            run_id: Some("run-planner-001".to_string()),
            instruction: Some(
                "Create a remediation timeline for the configuration audit findings. \
                 Address the self-improvement threshold change and MCP pool sizing."
                    .to_string(),
            ),
            summary: Some(
                "Planner produced a 3-phase remediation plan: immediate config fixes, \
                 staged rollout of threshold changes, and MCP pool capacity review."
                    .to_string(),
            ),
            error: None,
            arguments: serde_json::json!({
                "agent_id": "planner",
                "instruction": "Create a remediation timeline..."
            }),
            output: serde_json::json!({
                "thread_id": PLANNER_THREAD_ID,
                "run_id": "run-planner-001",
                "summary": "3-phase remediation plan completed"
            }),
            meta: meta(5, 4),
            expanded: false,
        },
        // 10. Long assistant response
        TimelineItem::text(
            MessageRoleTag::Assistant,
            "Configuration audit complete. Here is a summary of findings:\n\
             \n\
             1. Model providers: Both openai and groq are configured correctly with expected\n\
                API key env vars (OPENAI_API_KEY, GROQ_API_KEY).\n\
             2. Self-improvement: Threshold was 0.75, proposed change to 0.85 has been authorized.\n\
                The planner agent has created a staged rollout plan.\n\
             3. MCP server pool: Current size of 3 is adequate for current load, but the planner\n\
                recommends a capacity review if agent count exceeds 5.\n\
             4. Orchestrator capabilities: Allowlist matches policy. No unauthorized tools detected.\n\
             5. Secret store ACLs: One entry has an overly broad ACL (sensitive_payload visible to\n\
                all agents). Recommend restricting to system0 only.\n\
             \n\
             The planner agent has produced a 3-phase remediation timeline. Use Enter on the\n\
             delegate card above to review the full plan in the planner's thread.",
        ),
        // 11. modify_skill
        TimelineItem::ToolInvocation {
            call_id: "call-demo-006".to_string(),
            tool_id: "modify_skill".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({
                "skill_id": "config_audit",
                "changes": {"schedule": "weekly"}
            }),
            output: serde_json::json!({
                "status": "applied",
                "reason": "Skill config_audit updated to run on weekly schedule."
            }),
            meta: meta(6, 5),
            expanded: false,
        },
        // 12. score_skills
        TimelineItem::ToolInvocation {
            call_id: "call-demo-007".to_string(),
            tool_id: "score_skills".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({}),
            output: serde_json::json!({
                "scores": [
                    {"skill": "config_audit", "score": 0.92},
                    {"skill": "delegation", "score": 0.88},
                    {"skill": "secret_rotation", "score": 0.71}
                ]
            }),
            meta: meta(7, 6),
            expanded: false,
        },
        // 13. Generic tool with error status
        TimelineItem::ToolInvocation {
            call_id: "call-demo-008".to_string(),
            tool_id: "external_webhook".to_string(),
            status: "error".to_string(),
            arguments: serde_json::json!({
                "url": "https://hooks.example.com/notify",
                "payload": {"event": "audit_complete"}
            }),
            output: serde_json::json!({
                "error": "Connection refused: https://hooks.example.com/notify"
            }),
            meta: meta(8, 7),
            expanded: false,
        },
        // 14. Failed delegate to researcher
        TimelineItem::DelegateInvocation {
            call_id: "call-demo-009".to_string(),
            status: "error".to_string(),
            target_agent: Some("researcher".to_string()),
            thread_id: Some(RESEARCHER_THREAD_ID.to_string()),
            run_id: Some("run-researcher-001".to_string()),
            instruction: Some(
                "Research best practices for MCP server pool sizing under high concurrency."
                    .to_string(),
            ),
            summary: None,
            error: Some(
                "Agent exceeded max iterations (25) without producing a final answer.".to_string(),
            ),
            arguments: serde_json::json!({
                "agent_id": "researcher",
                "instruction": "Research best practices for MCP server pool sizing..."
            }),
            output: serde_json::json!({
                "thread_id": RESEARCHER_THREAD_ID,
                "run_id": "run-researcher-001",
                "error": "Agent exceeded max iterations (25) without producing a final answer."
            }),
            meta: meta(9, 8),
            expanded: false,
        },
        // 15. Run state changed system message
        TimelineItem::text(
            MessageRoleTag::System,
            "Run state changed: completed | summary: Config audit turn finished with 1 error in external_webhook and 1 failed delegation.",
        ),
        // 16. Short closing user message
        TimelineItem::text(
            MessageRoleTag::User,
            "Thanks. Apply the authorized proposal when ready.",
        ),
        // 17. Short closing assistant response
        TimelineItem::text(
            MessageRoleTag::Assistant,
            "Acknowledged. The authorized proposal (prop-demo-001) will be applied in the next maintenance window.",
        ),
    ];

    ThreadView {
        id: SYSTEM_THREAD_ID.to_string(),
        title: "System0".to_string(),
        kind: ThreadKind::System,
        agent_id: Some(SYSTEM_THREAD_ID.to_string()),
        state: "active".to_string(),
        active: true,
        resurrected: false,
        read_only: false,
        items,
        render_revision: 0,
    }
}

fn build_planner_thread() -> ThreadView {
    let items = vec![
        TimelineItem::text(
            MessageRoleTag::System,
            "Thread created for sub-agent 'planner'",
        ),
        TimelineItem::text(
            MessageRoleTag::User,
            "Create a remediation timeline for the configuration audit findings. \
             Address the self-improvement threshold change and MCP pool sizing.",
        ),
        TimelineItem::text(
            MessageRoleTag::Assistant,
            "Analyzing findings. The audit identified 3 areas requiring action:\n\
             - Self-improvement threshold: change from 0.75 to 0.85\n\
             - MCP pool capacity: adequate now, review if agents > 5\n\
             - Secret store ACL: restrict sensitive_payload to system0 only",
        ),
        TimelineItem::ToolInvocation {
            call_id: "call-planner-001".to_string(),
            tool_id: "record_lesson".to_string(),
            status: "success".to_string(),
            arguments: serde_json::json!({
                "lesson": "MCP pool sizing should scale linearly with active agent count"
            }),
            output: serde_json::json!({"recorded": true}),
            meta: meta(10, 10),
            expanded: false,
        },
        TimelineItem::text(
            MessageRoleTag::Assistant,
            "Remediation plan:\n\
             Phase 1 (immediate): Apply authorized config proposal prop-demo-001.\n\
             Phase 2 (this week): Restrict secret store ACL for sensitive_payload.\n\
             Phase 3 (next sprint): Implement automatic MCP pool scaling based on agent count.\n\
             \n\
             All changes should go through the proposal lifecycle for audit trail.",
        ),
        TimelineItem::text(
            MessageRoleTag::System,
            "Run state changed: completed | summary: Planner produced 3-phase remediation plan.",
        ),
    ];

    ThreadView {
        id: PLANNER_THREAD_ID.to_string(),
        title: format!("planner ({})", &PLANNER_THREAD_ID[..8]),
        kind: ThreadKind::Subagent,
        agent_id: Some("planner".to_string()),
        state: "completed".to_string(),
        active: false,
        resurrected: false,
        read_only: false,
        items,
        render_revision: 0,
    }
}

fn build_researcher_thread() -> ThreadView {
    let items = vec![
        TimelineItem::text(
            MessageRoleTag::System,
            "Saved sub-agent thread. Press Enter to resurrect and continue.",
        ),
        TimelineItem::text(
            MessageRoleTag::System,
            "Thread created for sub-agent 'researcher'",
        ),
        TimelineItem::text(
            MessageRoleTag::User,
            "Research best practices for MCP server pool sizing under high concurrency.",
        ),
        TimelineItem::text(
            MessageRoleTag::Assistant,
            "Beginning research on MCP pool sizing. Initial findings suggest that pool size \
             should be proportional to...",
        ),
        TimelineItem::text(
            MessageRoleTag::System,
            "Run state changed: failed | error: Agent exceeded max iterations (25) without producing a final answer.",
        ),
    ];

    ThreadView {
        id: RESEARCHER_THREAD_ID.to_string(),
        title: format!("researcher ({})", &RESEARCHER_THREAD_ID[..8]),
        kind: ThreadKind::Subagent,
        agent_id: Some("researcher".to_string()),
        state: "failed".to_string(),
        active: false,
        resurrected: false,
        read_only: true,
        items,
        render_revision: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::super::timeline::TimelineItem;
    use super::*;

    #[test]
    fn demo_threads_cover_all_visual_components() {
        let (threads, active) = build_demo_threads();

        // At least 3 threads with System0 first
        assert!(threads.len() >= 3);
        assert_eq!(threads[0].kind, ThreadKind::System);
        assert_eq!(active, 0);

        let all_items: Vec<&TimelineItem> = threads.iter().flat_map(|t| &t.items).collect();

        // All 3 TimelineItem variants present
        assert!(
            all_items
                .iter()
                .any(|i| matches!(i, TimelineItem::Text { .. }))
        );
        assert!(
            all_items
                .iter()
                .any(|i| matches!(i, TimelineItem::ToolInvocation { .. }))
        );
        assert!(
            all_items
                .iter()
                .any(|i| matches!(i, TimelineItem::DelegateInvocation { .. }))
        );

        // All 3 MessageRoleTag roles present
        assert!(all_items.iter().any(|i| matches!(
            i,
            TimelineItem::Text {
                role: MessageRoleTag::System,
                ..
            }
        )));
        assert!(all_items.iter().any(|i| matches!(
            i,
            TimelineItem::Text {
                role: MessageRoleTag::User,
                ..
            }
        )));
        assert!(all_items.iter().any(|i| matches!(
            i,
            TimelineItem::Text {
                role: MessageRoleTag::Assistant,
                ..
            }
        )));

        // Tool badge categories covered
        let tool_ids: Vec<&str> = all_items
            .iter()
            .filter_map(|i| match i {
                TimelineItem::ToolInvocation { tool_id, .. } => Some(tool_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(tool_ids.contains(&"list_agents"), "missing list_agents");
        assert!(tool_ids.contains(&"read_config"), "missing read_config");
        assert!(tool_ids.contains(&"modify_skill"), "missing modify_skill");
        assert!(
            tool_ids.contains(&"propose_config_change"),
            "missing propose_config_change"
        );
        assert!(
            tool_ids.contains(&"authorize_proposal"),
            "missing authorize_proposal"
        );
        assert!(tool_ids.contains(&"score_skills"), "missing score_skills");

        // At least one error status
        let has_error = all_items.iter().any(|i| match i {
            TimelineItem::ToolInvocation { status, .. } => status == "error",
            TimelineItem::DelegateInvocation { status, error, .. } => {
                status == "error" || error.is_some()
            }
            _ => false,
        });
        assert!(has_error, "should have at least one error status");

        // At least one read-only subagent thread
        assert!(
            threads
                .iter()
                .any(|t| t.kind == ThreadKind::Subagent && t.read_only),
            "should have at least one read-only subagent thread"
        );
    }
}
