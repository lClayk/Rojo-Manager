//! MCP over stdio: newline-delimited JSON-RPC 2.0.

use std::any::Any;
use std::io::{BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Mutex, PoisonError};
use std::thread;

use rojo_manager_core::VERSION;
use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::tools;

/// Supported MCP versions, newest first; the newest is offered when a client asks for another.
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const INSTRUCTIONS: &str = "Rojo Manager runs the Rojo servers for the user's Roblox projects.
- list_projects shows each project's server state, linked place, and whether that place is open in Studio.
- rojo_server starts, stops, or restarts a project's Rojo server.
- open_place opens a project's place in Roblox Studio; connect_rojo=true also starts the server and connects Rojo.
  Passing both project and place links that place to the project first.
- rojo_session connects or disconnects the Rojo plugin in an open place. When it reports Confirming, Rojo is
  waiting for the initial sync to be approved: use accept to apply the project to Studio, or abort.
- manage_project adds project folders, changes a project's name, port, autostart, auto_connect, or linked place,
  or removes a project from the list (its files are kept).
- server_logs reads a project's recent Rojo output, such as sync errors or a broken project file.
- updates checks whether Rojo Manager and its Studio bridge are up to date, and installs either when the user asks.
Server states: off; starting; serving (running, no Studio connected); connected (a Studio is syncing);
external (running but started outside Rojo Manager, still controllable); blocked (another app owns the port).
Studio editing tools come from the separate robloxstudio MCP server; pass it the instanceId returned here.";

/// Read requests until `input` closes. Tool calls run on their own threads so pings stay responsive;
/// finished ones are dropped, and those still running at the end are waited for.
pub fn serve(input: impl BufRead, output: impl Write + Send, backend: &dyn Backend) {
    let output = Mutex::new(output);
    let write = |response: &Value| {
        let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
        // When stdout is gone there is no one left to tell.
        let _ = writeln!(output, "{response}").and_then(|()| output.flush());
    };
    let reply = |message: Map<String, Value>| {
        // Never let one bad request end the session.
        let response = panic::catch_unwind(AssertUnwindSafe(|| handle(backend, &message))).unwrap_or_else(|panic| {
            let id = message.get("id")?.clone();
            Some(error(id, -32603, &panic_message(&*panic)))
        });
        if let Some(response) = response {
            write(&response);
        }
    };
    let reply = &reply;
    thread::scope(|scope| {
        for line in input.split(b'\n') {
            let Ok(line) = line else { break };
            if line.trim_ascii().is_empty() {
                continue;
            }
            match serde_json::from_slice(&line) {
                Ok(Value::Object(message)) if message.get("method").and_then(Value::as_str) == Some("tools/call") => {
                    scope.spawn(move || reply(message));
                }
                Ok(Value::Object(message)) => reply(message),
                // Batches included: no MCP client sends them, and they need not hang a client that does.
                Ok(_) => write(&error(Value::Null, -32600, "Invalid Request")),
                Err(_) => write(&error(Value::Null, -32700, "Parse error")),
            }
        }
    });
}

/// Response for one JSON-RPC request, or None for notifications.
fn handle(backend: &dyn Backend, message: &Map<String, Value>) -> Option<Value> {
    let id = message.get("id")?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or_default();
    let no_params = Map::new();
    let params = message.get("params").and_then(Value::as_object).unwrap_or(&no_params);
    let result = match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = PROTOCOL_VERSIONS.into_iter().find(|version| Some(*version) == requested);
            json!({
                "protocolVersion": version.unwrap_or(PROTOCOL_VERSIONS[0]),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rojo-manager", "version": VERSION},
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::specs() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let no_args = Map::new();
            let args = params.get("arguments").and_then(Value::as_object).unwrap_or(&no_args);
            call_tool(backend, name, args)
        }
        _ => return Some(error(id.clone(), -32601, &format!("Method not found: {method}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

/// An MCP tools/call result. Failures become isError results the agent can read.
fn call_tool(backend: &dyn Backend, name: &str, args: &tools::Args) -> Value {
    // Compact text only: structuredContent would make some clients show the agent the result twice.
    match tools::run(backend, name, args) {
        Ok(text) => json!({"content": [{"type": "text", "text": text}]}),
        Err(message) => json!({"content": [{"type": "text", "text": message}], "isError": true}),
    }
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    match (panic.downcast_ref::<&str>(), panic.downcast_ref::<String>()) {
        (Some(message), _) => (*message).to_string(),
        (_, Some(message)) => message.clone(),
        _ => "Internal error".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::fake::Fake;

    #[test]
    fn initialize_list_and_call_over_stdio() {
        let messages = [
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "list_projects"}}),
            json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}),
            json!({"jsonrpc": "2.0", "id": 5, "method": "initialize", "params": {"protocolVersion": "2099-01-01"}}),
            json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {"name": "delete_everything"}}),
        ];
        let mut input: String = messages.iter().map(|message| format!("{message}\n")).collect();
        input.push_str("\nnot json\n");
        let mut output = Vec::new();
        serve(input.as_bytes(), &mut output, &Fake::default());

        let responses: Vec<Value> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 7); // the notification and the blank line get no reply
        let by_id: HashMap<String, &Value> =
            responses.iter().map(|response| (response["id"].to_string(), response)).collect();

        let initialized = &by_id["1"]["result"];
        assert_eq!(initialized["protocolVersion"], "2025-03-26");
        assert_eq!(initialized["serverInfo"], json!({"name": "rojo-manager", "version": VERSION}));
        assert_eq!(initialized["instructions"], INSTRUCTIONS);
        let names: HashSet<&str> = by_id["2"]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        let expected = [
            "list_projects",
            "rojo_server",
            "open_place",
            "rojo_session",
            "server_logs",
            "manage_project",
            "updates",
            "app_settings",
        ];
        assert_eq!(names, HashSet::from(expected));
        assert_eq!(by_id["3"]["result"], json!({"content": [{"type": "text", "text": r#"{"projects":[]}"#}]}));
        assert_eq!(by_id["4"]["error"], json!({"code": -32601, "message": "Method not found: resources/list"}));
        assert_eq!(by_id["5"]["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(
            by_id["6"]["result"],
            json!({"content": [{"type": "text", "text": "Unknown tool delete_everything"}], "isError": true})
        );
        assert_eq!(by_id["null"]["error"], json!({"code": -32700, "message": "Parse error"}));
    }
}
