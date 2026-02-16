use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use neuromancer_core::error::{NeuromancerError, ToolError};
use neuromancer_core::security::ExecutionGuard;
use neuromancer_core::tool::{
    AgentContext, ToolBroker, ToolCall, ToolOutput, ToolResult, ToolSource, ToolSpec,
};

use crate::aliases::build_skill_tool_aliases;
use crate::csv::parse_csv_content;
use crate::path_policy::{resolve_local_data_path, resolve_skill_script_path};
use crate::script_runner::run_skill_script;
use crate::{Skill, SkillError, SkillRegistry};

const DEFAULT_SKILL_SCRIPT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct SkillToolBroker {
    tools: HashMap<String, SkillTool>,
    tool_aliases: HashMap<String, String>,
    aliases_by_tool: HashMap<String, Vec<String>>,
    local_root: PathBuf,
    execution_guard: Arc<dyn ExecutionGuard>,
}

#[derive(Clone)]
struct SkillTool {
    description: String,
    skill_root: PathBuf,
    markdown_paths: Vec<String>,
    csv_paths: Vec<String>,
    script_path: Option<String>,
    script_timeout: Duration,
    required_safeguards: Vec<String>,
}

impl SkillToolBroker {
    pub fn new(
        agent_id: &str,
        allowed_skills: &[String],
        skill_registry: &SkillRegistry,
        local_root: PathBuf,
        execution_guard: Arc<dyn ExecutionGuard>,
    ) -> Result<Self, SkillError> {
        let mut tools = HashMap::new();
        for skill_name in allowed_skills {
            let skill = skill_registry.get(skill_name).ok_or_else(|| {
                SkillError::Config(format!(
                    "agent '{}' references missing skill '{}'",
                    agent_id, skill_name
                ))
            })?;

            tools.insert(skill_name.clone(), skill_tool_from_skill(skill));
        }

        let (tool_aliases, aliases_by_tool) = build_skill_tool_aliases(allowed_skills);

        Ok(Self {
            tools,
            tool_aliases,
            aliases_by_tool,
            local_root,
            execution_guard,
        })
    }

    fn resolve_tool_id(&self, ctx: &AgentContext, requested_tool_id: &str) -> Option<String> {
        let is_allowed = |tool_id: &str| ctx.allowed_tools.iter().any(|allowed| allowed == tool_id);
        if is_allowed(requested_tool_id) && self.tools.contains_key(requested_tool_id) {
            return Some(requested_tool_id.to_string());
        }

        let canonical = self.tool_aliases.get(requested_tool_id)?;
        if is_allowed(canonical) && self.tools.contains_key(canonical) {
            Some(canonical.clone())
        } else {
            None
        }
    }
}

#[async_trait::async_trait]
impl ToolBroker for SkillToolBroker {
    async fn list_tools(&self, ctx: &AgentContext) -> Vec<ToolSpec> {
        let mut specs = Vec::new();
        for name in &ctx.allowed_tools {
            let Some(tool) = self.tools.get(name) else {
                continue;
            };

            specs.push(skill_tool_spec(name, name, &tool.description));

            if let Some(aliases) = self.aliases_by_tool.get(name) {
                for alias in aliases {
                    specs.push(skill_tool_spec(
                        alias,
                        name,
                        &format!("Alias for '{name}'. {}", tool.description),
                    ));
                }
            }
        }

        specs
    }

    async fn call_tool(
        &self,
        ctx: &AgentContext,
        call: ToolCall,
    ) -> Result<ToolResult, NeuromancerError> {
        let started_at = Instant::now();
        let requested_tool_id = call.tool_id.clone();
        tracing::info!(
            agent_id = %ctx.agent_id,
            task_id = %ctx.task_id,
            tool_id = %requested_tool_id,
            call_id = %call.id,
            "skill_tool_started"
        );

        let Some(canonical_tool_id) = self.resolve_tool_id(ctx, &requested_tool_id) else {
            return Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: requested_tool_id,
            }));
        };

        if canonical_tool_id != requested_tool_id {
            tracing::warn!(
                agent_id = %ctx.agent_id,
                task_id = %ctx.task_id,
                requested_tool_id = %requested_tool_id,
                canonical_tool_id = %canonical_tool_id,
                "skill_tool_alias_used"
            );
        }

        let Some(tool) = self.tools.get(&canonical_tool_id) else {
            return Err(NeuromancerError::Tool(ToolError::NotFound {
                tool_id: canonical_tool_id,
            }));
        };

        let mut markdown_docs = Vec::new();
        for raw in &tool.markdown_paths {
            let path = resolve_local_data_path(&self.local_root, raw)
                .map_err(map_tool_err(&canonical_tool_id))?;
            let content = fs::read_to_string(&path).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: canonical_tool_id.clone(),
                    message: err.to_string(),
                })
            })?;
            markdown_docs.push(serde_json::json!({
                "path": raw,
                "content": content,
            }));
        }

        let mut csv_docs = Vec::new();
        for raw in &tool.csv_paths {
            let path = resolve_local_data_path(&self.local_root, raw)
                .map_err(map_tool_err(&canonical_tool_id))?;
            let content = fs::read_to_string(&path).map_err(|err| {
                NeuromancerError::Tool(ToolError::ExecutionFailed {
                    tool_id: canonical_tool_id.clone(),
                    message: err.to_string(),
                })
            })?;
            let parsed =
                parse_csv_content(&path, &content).map_err(map_tool_err(&canonical_tool_id))?;
            csv_docs.push(serde_json::json!({
                "path": raw,
                "headers": parsed.headers,
                "rows": parsed.rows,
                "summary": {
                    "row_count": parsed.row_count,
                    "total_balance": parsed.total_balance,
                }
            }));
        }

        let script_result = if let Some(relative_script_path) = tool.script_path.as_deref() {
            self.execution_guard
                .pre_skill_script_execution(&canonical_tool_id, &tool.required_safeguards)
                .map_err(map_tool_err(&canonical_tool_id))?;

            let script_path = resolve_skill_script_path(&tool.skill_root, relative_script_path)
                .map_err(map_tool_err(&canonical_tool_id))?;

            let payload = serde_json::json!({
                "local_root": self.local_root.display().to_string(),
                "skill": canonical_tool_id.clone(),
                "data_sources": {
                    "markdown": tool.markdown_paths.clone(),
                    "csv": tool.csv_paths.clone(),
                },
                "arguments": call.arguments.clone(),
            });

            Some(
                run_skill_script(
                    &script_path,
                    &payload,
                    tool.script_timeout,
                    &ctx.agent_id,
                    &ctx.task_id.to_string(),
                    &canonical_tool_id,
                )
                .await
                .map_err(map_tool_err(&canonical_tool_id))?,
            )
        } else {
            None
        };

        tracing::info!(
            agent_id = %ctx.agent_id,
            task_id = %ctx.task_id,
            tool_id = %requested_tool_id,
            skill_id = %canonical_tool_id,
            call_id = %call.id,
            duration_ms = started_at.elapsed().as_millis(),
            "skill_tool_finished"
        );

        Ok(ToolResult {
            call_id: call.id,
            output: ToolOutput::Success(serde_json::json!({
                "skill": canonical_tool_id,
                "markdown": markdown_docs,
                "csv": csv_docs,
                "script_result": script_result,
            })),
        })
    }
}

fn map_tool_err<E: std::fmt::Display>(tool_id: &str) -> impl Fn(E) -> NeuromancerError + '_ {
    move |err| {
        NeuromancerError::Tool(ToolError::ExecutionFailed {
            tool_id: tool_id.to_string(),
            message: err.to_string(),
        })
    }
}

fn skill_tool_from_skill(skill: &Skill) -> SkillTool {
    let metadata = &skill.metadata;
    SkillTool {
        description: if metadata.description.trim().is_empty() {
            format!("Skill {}", metadata.name)
        } else {
            metadata.description.clone()
        },
        skill_root: skill.path.clone(),
        markdown_paths: metadata.data_sources.markdown.clone(),
        csv_paths: metadata.data_sources.csv.clone(),
        script_path: metadata.execution.script.clone(),
        script_timeout: metadata
            .execution
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_SKILL_SCRIPT_TIMEOUT),
        required_safeguards: metadata.safeguards.human_approval.clone(),
    }
}

fn skill_tool_spec(name: &str, skill_id: &str, description: &str) -> ToolSpec {
    ToolSpec {
        id: name.to_string(),
        name: name.to_string(),
        description: description.to_string(),
        parameters_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        source: ToolSource::Skill {
            skill_id: skill_id.to_string(),
        },
    }
}
