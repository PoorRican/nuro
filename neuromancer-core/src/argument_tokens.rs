use std::collections::HashMap;

pub const USER_QUERY_TOKEN: &str = "{{USER_QUERY}}";

/// Expand all recognized argument tokens in a JSON value.
///
/// Strings support interpolation (token can appear anywhere in the string).
/// Objects/arrays are traversed recursively.
pub fn expand_tokens(value: &serde_json::Value, tokens: &HashMap<&str, &str>) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            let mut out = text.clone();
            for (token, replacement) in tokens {
                out = out.replace(token, replacement);
            }
            serde_json::Value::String(out)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| expand_tokens(item, tokens))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let expanded = map
                .iter()
                .map(|(k, v)| (k.clone(), expand_tokens(v, tokens)))
                .collect();
            serde_json::Value::Object(expanded)
        }
        _ => value.clone(),
    }
}

pub fn expand_user_query_tokens(value: &serde_json::Value, user_query: &str) -> serde_json::Value {
    let mut tokens = HashMap::new();
    tokens.insert(USER_QUERY_TOKEN, user_query);
    expand_tokens(value, &tokens)
}

/// Extract all `{{HANDLE}}` references from a JSON value's string fields.
///
/// Returns deduplicated handle names (without the `{{` `}}` delimiters).
/// Used by tool brokers to resolve secret handles before tool execution.
pub fn extract_handle_refs(value: &serde_json::Value) -> Vec<String> {
    let mut refs = Vec::new();
    collect_handle_refs(value, &mut refs);
    refs.sort();
    refs.dedup();
    refs
}

fn collect_handle_refs(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text) => {
            let mut remaining = text.as_str();
            while let Some(start) = remaining.find("{{") {
                let after_start = &remaining[start + 2..];
                if let Some(end) = after_start.find("}}") {
                    let handle = &after_start[..end];
                    // Skip the built-in USER_QUERY token
                    if handle != "USER_QUERY" && !handle.is_empty() {
                        out.push(handle.to_string());
                    }
                    remaining = &after_start[end + 2..];
                } else {
                    break;
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_handle_refs(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values() {
                collect_handle_refs(v, out);
            }
        }
        _ => {}
    }
}

/// Expand resolved secret handles in tool call arguments.
///
/// Takes a map of handle_name → plaintext_value and replaces all `{{handle_name}}`
/// occurrences in the JSON value. The plaintext appears only in the tool call
/// arguments, never in conversation context.
pub fn expand_secret_handles(
    value: &serde_json::Value,
    resolved: &HashMap<String, String>,
) -> serde_json::Value {
    let tokens: HashMap<&str, &str> = resolved
        .iter()
        .map(|(k, v)| {
            // Build the token key with delimiters for expand_tokens
            // We can't use expand_tokens directly because it expects &str keys
            (k.as_str(), v.as_str())
        })
        .collect();

    // Build full token strings with {{}} delimiters
    let mut full_tokens = HashMap::new();
    for (handle, value) in &tokens {
        let token = format!("{{{{{handle}}}}}");
        full_tokens.insert(token, value.to_string());
    }

    // Manually expand since we have owned keys
    expand_owned_tokens(value, &full_tokens)
}

fn expand_owned_tokens(
    value: &serde_json::Value,
    tokens: &HashMap<String, String>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            let mut out = text.clone();
            for (token, replacement) in tokens {
                out = out.replace(token.as_str(), replacement);
            }
            serde_json::Value::String(out)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| expand_owned_tokens(item, tokens))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let expanded = map
                .iter()
                .map(|(k, v)| (k.clone(), expand_owned_tokens(v, tokens)))
                .collect();
            serde_json::Value::Object(expanded)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_exact_user_query_token() {
        let input = serde_json::json!({
            "instruction": USER_QUERY_TOKEN
        });
        let expanded = expand_user_query_tokens(&input, "summarize this");
        assert_eq!(
            expanded,
            serde_json::json!({
                "instruction": "summarize this"
            })
        );
    }

    #[test]
    fn expands_interpolated_user_query_token() {
        let input = serde_json::json!({
            "instruction": "Please do: {{USER_QUERY}}"
        });
        let expanded = expand_user_query_tokens(&input, "review this repo");
        assert_eq!(
            expanded,
            serde_json::json!({
                "instruction": "Please do: review this repo"
            })
        );
    }

    #[test]
    fn expands_nested_tokens_and_keeps_unknown_tokens() {
        let input = serde_json::json!({
            "instruction": "{{USER_QUERY}}",
            "meta": {
                "nested": ["start", "task={{USER_QUERY}}", "{{UNKNOWN_TOKEN}}"]
            }
        });
        let expanded = expand_user_query_tokens(&input, "build a test plan");
        assert_eq!(
            expanded,
            serde_json::json!({
                "instruction": "build a test plan",
                "meta": {
                    "nested": ["start", "task=build a test plan", "{{UNKNOWN_TOKEN}}"]
                }
            })
        );
    }

    #[test]
    fn extract_handle_refs_finds_handles() {
        let input = serde_json::json!({
            "api_key": "{{MY_API_KEY}}",
            "nested": {
                "token": "Bearer {{AUTH_TOKEN}}"
            },
            "list": ["{{DB_PASSWORD}}", "plain text"]
        });
        let refs = extract_handle_refs(&input);
        assert_eq!(refs, vec!["AUTH_TOKEN", "DB_PASSWORD", "MY_API_KEY"]);
    }

    #[test]
    fn extract_handle_refs_skips_user_query() {
        let input = serde_json::json!({
            "query": "{{USER_QUERY}}",
            "key": "{{SECRET_KEY}}"
        });
        let refs = extract_handle_refs(&input);
        assert_eq!(refs, vec!["SECRET_KEY"]);
    }

    #[test]
    fn extract_handle_refs_deduplicates() {
        let input = serde_json::json!({
            "a": "{{TOKEN}}",
            "b": "{{TOKEN}}",
            "c": "{{OTHER}}"
        });
        let refs = extract_handle_refs(&input);
        assert_eq!(refs, vec!["OTHER", "TOKEN"]);
    }

    #[test]
    fn extract_handle_refs_empty_on_no_handles() {
        let input = serde_json::json!({"plain": "no handles here", "num": 42});
        assert!(extract_handle_refs(&input).is_empty());
    }

    #[test]
    fn extract_handle_refs_skips_empty_handles() {
        let input = serde_json::json!({"x": "{{}}"});
        assert!(extract_handle_refs(&input).is_empty());
    }

    #[test]
    fn expand_secret_handles_replaces() {
        let input = serde_json::json!({
            "api_key": "{{MY_KEY}}",
            "url": "https://api.example.com?token={{MY_KEY}}"
        });
        let mut resolved = HashMap::new();
        resolved.insert("MY_KEY".to_string(), "sk-12345".to_string());
        let expanded = expand_secret_handles(&input, &resolved);
        assert_eq!(
            expanded,
            serde_json::json!({
                "api_key": "sk-12345",
                "url": "https://api.example.com?token=sk-12345"
            })
        );
    }

    #[test]
    fn expand_secret_handles_leaves_unresolved() {
        let input = serde_json::json!({"key": "{{UNRESOLVED}}"});
        let resolved = HashMap::new();
        let expanded = expand_secret_handles(&input, &resolved);
        assert_eq!(expanded, serde_json::json!({"key": "{{UNRESOLVED}}"}));
    }

    #[test]
    fn expand_secret_handles_nested() {
        let input = serde_json::json!({
            "config": {
                "items": ["{{A}}", "prefix-{{B}}-suffix"]
            }
        });
        let mut resolved = HashMap::new();
        resolved.insert("A".to_string(), "val_a".to_string());
        resolved.insert("B".to_string(), "val_b".to_string());
        let expanded = expand_secret_handles(&input, &resolved);
        assert_eq!(
            expanded,
            serde_json::json!({
                "config": {
                    "items": ["val_a", "prefix-val_b-suffix"]
                }
            })
        );
    }
}
