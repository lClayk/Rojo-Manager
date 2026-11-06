//! Client for robloxstudio-mcp's local HTTP bridge (https://github.com/Chrrxs/robloxstudio-mcp).
//! The bridge runs while an agent has robloxstudio-mcp open. Tool calls
//! need the token robloxstudio-mcp writes to ~/.robloxstudio-mcp/auth-token.

use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::localnet;

#[derive(Debug, Clone)]
pub struct StudioMcpError(pub String);

impl std::fmt::Display for StudioMcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How long a tool call may take; Luau in Studio can be slow.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// $ROBLOX_STUDIO_PORT, default 58741.
pub fn bridge_port() -> u16 {
    std::env::var("ROBLOX_STUDIO_PORT").ok().and_then(|port| port.trim().parse().ok()).unwrap_or(58741)
}

/// The first of `keys` with a meaningful value, as text.
fn message(reply: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| match reply.get(*key)? {
        Value::Null | Value::Bool(false) => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    })
}

fn open(
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<Value, StudioMcpError> {
    let response = localnet::request(bridge_port(), method, path, body, headers, timeout, None).map_err(|error| {
        StudioMcpError(format!(
            "robloxstudio-mcp is not reachable. It runs while an agent has the robloxstudio MCP server open. ({error})"
        ))
    })?;
    match serde_json::from_slice::<Value>(&response.body).ok() {
        Some(result) if (200..300).contains(&response.status) && !result.is_null() => Ok(result),
        result => {
            let detail =
                result.as_ref().and_then(Value::as_object).and_then(|reply| message(reply, &["message", "error"]));
            let detail = detail.unwrap_or_else(|| format!("HTTP {}", response.status));
            Err(StudioMcpError(format!("robloxstudio-mcp: {detail}")))
        }
    }
}

/// Passive status, readable without the token: version, connected instances.
pub fn health(timeout: Duration) -> Result<Map<String, Value>, StudioMcpError> {
    Ok(match open("GET", "/health", None, &[], timeout)? {
        Value::Object(status) => status,
        _ => Map::new(),
    })
}

/// POST /mcp/<tool> with the token from ~/.robloxstudio-mcp/auth-token.
pub fn call(tool: &str, arguments: &Value, timeout: Duration) -> Result<Map<String, Value>, StudioMcpError> {
    let path = std::env::home_dir().unwrap_or_default().join(".robloxstudio-mcp").join("auth-token");
    let token = std::fs::read_to_string(path)
        .map_err(|_| StudioMcpError("robloxstudio-mcp has not created its auth token yet. Start it once.".into()))?;
    match open("POST", &format!("/mcp/{tool}"), Some(arguments), &[("X-MCP-Auth", token.trim())], timeout)? {
        Value::Object(result) => Ok(result),
        _ => Err(StudioMcpError(format!("robloxstudio-mcp returned an unexpected {tool} response"))),
    }
}

/// The Edit-mode Studio instance that has this place open, if any.
pub fn find_instance(place_id: u64) -> Result<Option<String>, StudioMcpError> {
    Ok(instance_with_place(&call("get_connected_instances", &json!({}), CALL_TIMEOUT)?, place_id))
}

fn instance_with_place(reply: &Map<String, Value>, place_id: u64) -> Option<String> {
    let instances = reply.get("instances")?.as_array()?;
    instances.iter().find_map(|instance| {
        let id = instance.get("id")?.as_str()?;
        (instance.get("placeId")?.as_u64()? == place_id).then(|| id.to_string())
    })
}

/// Run plugin-level Luau in one Studio instance and return its JSON-decoded return value.
pub fn run_luau(instance_id: &str, code: &str) -> Result<Value, StudioMcpError> {
    luau_result(call("execute_luau", &json!({"instance_id": instance_id, "code": code}), CALL_TIMEOUT)?)
}

/// The return value of an execute_luau reply, JSON-decoded when it is JSON text.
fn luau_result(mut reply: Map<String, Value>) -> Result<Value, StudioMcpError> {
    if reply.get("success") != Some(&Value::Bool(true)) {
        let detail = message(&reply, &["error", "message"]);
        return Err(StudioMcpError(detail.unwrap_or_else(|| "Luau execution failed".into())));
    }
    Ok(match reply.remove("returnValue").unwrap_or_default() {
        Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
        value => value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn instances_are_found_by_place_id() {
        let reply = object(json!({"instances": [
            {"id": "instance:a", "placeId": 1}, {"id": "instance:b", "placeId": 2}, {"placeId": 3}
        ]}));
        assert_eq!(instance_with_place(&reply, 2).as_deref(), Some("instance:b"));
        assert_eq!(instance_with_place(&reply, 3), None);
    }

    #[test]
    fn luau_results_are_decoded_and_failures_are_errors() {
        let ok = object(json!({"success": true, "returnValue": "{\"status\":\"Connected\"}"}));
        assert_eq!(luau_result(ok).unwrap(), json!({"status": "Connected"}));
        assert_eq!(luau_result(object(json!({"success": true, "returnValue": "plain"}))).unwrap(), json!("plain"));
        let failed = luau_result(object(json!({"success": false, "error": "boom"}))).unwrap_err();
        assert_eq!(failed.0, "boom");
    }
}
