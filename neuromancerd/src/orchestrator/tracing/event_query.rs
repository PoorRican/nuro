use neuromancer_core::rpc::{OrchestratorEventsQueryParams, ThreadEvent};

pub(crate) fn event_matches_query(
    event: &ThreadEvent,
    params: &OrchestratorEventsQueryParams,
) -> bool {
    if let Some(thread_id) = params.thread_id.as_deref()
        && event.thread_id != thread_id
    {
        return false;
    }

    if let Some(run_id) = params.run_id.as_deref()
        && event.run_id.as_deref() != Some(run_id)
    {
        return false;
    }

    if let Some(agent_id) = params.agent_id.as_deref()
        && event.agent_id.as_deref() != Some(agent_id)
    {
        return false;
    }

    if let Some(event_type) = params.event_type.as_deref()
        && event.event_type != event_type
    {
        return false;
    }

    if let Some(tool_id) = params.tool_id.as_deref()
        && event_tool_id(event).as_deref() != Some(tool_id)
    {
        return false;
    }

    if let Some(error_contains) = params.error_contains.as_deref() {
        let needle = error_contains.to_ascii_lowercase();
        let haystack = event_error_text(event)
            .unwrap_or_else(|| event.payload.to_string())
            .to_ascii_lowercase();
        if !haystack.contains(&needle) {
            return false;
        }
    }

    true
}

pub(crate) fn event_tool_id(event: &ThreadEvent) -> Option<String> {
    event
        .payload
        .get("tool_id")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
}

pub(crate) fn event_error_text(event: &ThreadEvent) -> Option<String> {
    event
        .payload
        .get("error")
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            event
                .payload
                .as_str()
                .map(ToString::to_string)
                .filter(|value| !value.trim().is_empty())
        })
}
