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

#[cfg(test)]
mod tests {
    use super::{USER_QUERY_TOKEN, expand_user_query_tokens};

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
}
