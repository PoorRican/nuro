use std::collections::HashSet;

pub fn collect_secret_values() -> Vec<String> {
    std::env::vars()
        .filter(|(key, _)| is_sensitive_key(key))
        .map(|(_, value)| value)
        .filter(|value| !value.trim().is_empty())
        .collect()
}

pub fn is_sensitive_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD"]
        .iter()
        .any(|needle| upper.contains(needle))
}

pub fn sanitize_event_payload(
    event_type: &str,
    payload: serde_json::Value,
    secret_values: &[String],
) -> (serde_json::Value, bool, bool) {
    if !tool_payload_shape_allowed(event_type, &payload) {
        return (serde_json::Value::Null, false, false);
    }

    let mut redacted = false;
    let redacted_payload = sanitize_json_value(payload, secret_values, &mut redacted);
    (redacted_payload, redacted, true)
}

pub fn sanitize_json_value(
    value: serde_json::Value,
    secret_values: &[String],
    redacted: &mut bool,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let mut masked = s;
            for secret in secret_values {
                if !secret.is_empty() && masked.contains(secret) {
                    masked = masked.replace(secret, "[REDACTED]");
                    *redacted = true;
                }
            }
            serde_json::Value::String(masked)
        }
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.into_iter()
                .map(|item| sanitize_json_value(item, secret_values, redacted))
                .collect(),
        ),
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if is_sensitive_key(&key) {
                    out.insert(key, serde_json::Value::String("[REDACTED]".to_string()));
                    *redacted = true;
                    continue;
                }
                out.insert(key, sanitize_json_value(value, secret_values, redacted));
            }
            serde_json::Value::Object(out)
        }
        other => other,
    }
}

pub fn tool_payload_shape_allowed(event_type: &str, payload: &serde_json::Value) -> bool {
    if matches!(event_type, "tool_call" | "tool_result") {
        let Some(obj) = payload.as_object() else {
            return false;
        };
        let allowed: HashSet<&str> = ["call_id", "tool_id", "arguments", "status", "output"]
            .into_iter()
            .collect();
        return obj.keys().all(|key| allowed.contains(key.as_str()));
    }
    true
}
