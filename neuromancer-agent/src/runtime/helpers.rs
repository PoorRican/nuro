use neuromancer_core::tool::ToolSpec;

pub(super) fn available_tool_names(specs: &[ToolSpec]) -> Vec<String> {
    let mut names: Vec<String> = specs.iter().map(|spec| spec.name.clone()).collect();
    names.sort();
    names.dedup();
    names
}

/// Convert neuromancer ToolSpecs to rig ToolDefinitions.
pub(super) fn specs_to_rig_definitions(specs: &[ToolSpec]) -> Vec<rig::completion::ToolDefinition> {
    specs
        .iter()
        .map(|s| rig::completion::ToolDefinition {
            name: s.name.clone(),
            description: s.description.clone(),
            parameters: s.parameters_schema.clone(),
        })
        .collect()
}

/// Truncate text to at most `max_len` characters for summaries.
// NOTE: [low pri] ideally this would use another utility LLM
pub(super) fn truncate_summary(text: &str, max_len: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max_len {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_len).collect();
        format!("{truncated}...")
    }
}
