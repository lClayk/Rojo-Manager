//! A scripted [`Backend`] for tests. It records the calls that matter to the tools' behavior.

use std::collections::VecDeque;
use std::io;
use std::sync::Mutex;
use std::time::Duration;

use rojo_manager_core::config::Place;
use rojo_manager_core::control::RequestError;
use rojo_manager_core::studio::ResolveError;
use rojo_manager_core::studio_mcp::StudioMcpError;
use serde_json::{Map, Value, json};

use crate::backend::Backend;

/// Answers given in order; the last one repeats.
pub struct Script<T>(Mutex<VecDeque<T>>);

impl<T: Clone> Script<T> {
    pub fn new(answers: impl IntoIterator<Item = T>) -> Self {
        Script(Mutex::new(answers.into_iter().collect()))
    }

    fn next(&self) -> T {
        let mut answers = self.0.lock().unwrap();
        if answers.len() > 1 { answers.pop_front().unwrap() } else { answers[0].clone() }
    }
}

pub struct Fake {
    /// GET /projects answers (lists of projects); null when the manager is not reachable.
    pub projects: Script<Value>,
    /// Answers every other control request, given its method and path parts.
    pub reply: fn(&str, &[&str]) -> Value,
    /// The get_connected_instances answer; None when robloxstudio-mcp is not running.
    pub instances: Option<Value>,
    /// What the bridge Luau returns.
    pub luau: Script<Value>,
    /// Whether Rojo answers on the project's port.
    pub answers: Script<bool>,
    /// Read with [`Fake::calls`].
    pub calls: Mutex<Vec<String>>,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            projects: Script::new([json!([])]),
            reply: |_, _| json!({"project": project("serving", true)}),
            instances: Some(json!({"instances": []})),
            luau: Script::new([json!({"status": "Connected"})]),
            answers: Script::new([true]),
            calls: Mutex::default(),
        }
    }
}

impl Fake {
    /// Control requests ("METHOD path body"), launches, bridge calls (instance and the Luau's last
    /// line), place lookups, and Studio launches, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

/// TestGame as the manager reports it, linked to place 2 when `place`.
pub fn project(state: &str, place: bool) -> Value {
    let place = if place { json!({"placeId": 2, "universeId": 1}) } else { Value::Null };
    json!({"name": "TestGame", "port": 34879, "state": state, "log": ["error: port in use"], "place": place})
}

/// A get_connected_instances answer with place 2 open in Studio.
pub fn studio_open() -> Option<Value> {
    Some(json!({"instances": [{"id": "instance:a", "placeId": 2}]}))
}

impl Backend for Fake {
    fn control(
        &self,
        method: &str,
        parts: &[&str],
        body: Option<&Value>,
        _timeout: Duration,
    ) -> Result<Value, RequestError> {
        let path = parts.join("/");
        self.record(match body {
            Some(body) => format!("{method} {path} {body}"),
            None => format!("{method} {path}"),
        });
        if method == "GET" && path == "projects" {
            let projects = self.projects.next();
            if projects.is_null() {
                return Err(RequestError::Unreachable("Rojo Manager is not running".into()));
            }
            return Ok(json!({ "projects": projects }));
        }
        Ok((self.reply)(method, parts))
    }

    fn launch_manager(&self) -> io::Result<()> {
        self.record("launch".into());
        Ok(())
    }

    fn rojo_answers(&self, _port: u16) -> bool {
        self.answers.next()
    }

    fn studio_call(&self, _tool: &str, _arguments: &Value) -> Result<Map<String, Value>, StudioMcpError> {
        match &self.instances {
            Some(Value::Object(reply)) => Ok(reply.clone()),
            _ => Err(StudioMcpError("robloxstudio-mcp is not reachable".into())),
        }
    }

    fn run_luau(&self, instance_id: &str, code: &str) -> Result<Value, StudioMcpError> {
        self.record(format!("luau {instance_id} {}", code.lines().last().unwrap_or_default()));
        Ok(self.luau.next())
    }

    fn resolve_place(&self, text: &str) -> Result<Place, ResolveError> {
        self.record(format!("resolve {text}"));
        Ok(Place { place_id: 2, universe_id: 1 })
    }

    fn open_place(&self, place: &Place) -> io::Result<()> {
        self.record(format!("open {}", place.place_id));
        Ok(())
    }

    fn sleep(&self, _duration: Duration) {}
}
