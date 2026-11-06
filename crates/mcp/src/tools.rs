//! The tools agents call. Rojo processes stay owned by the Rojo Manager app, reached through its
//! control API (it is started hidden when it is not running). Studio is reached through
//! robloxstudio-mcp's HTTP bridge and the Rojo Manager bridge patched into the Rojo plugin.
//!
//! Results are typed structs serialized in field order, so agents read the same compact JSON as
//! before. Failures are plain messages, reported to the agent as tool errors.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rojo_manager_core::config::Place;
use rojo_manager_core::control::{ControlError, RequestError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::backend::Backend;

/// A tool's arguments, as the agent sent them.
pub type Args = Map<String, Value>;

const RUNNING: [&str; 3] = ["serving", "connected", "external"];
const BRIDGE_MISSING: &str = "The Rojo Manager bridge is not loaded in this Studio. Install it from Rojo Manager \
    (Settings > Agents), disable the Creator Store Rojo plugin, then restart Studio.";
/// control.request's default, used for GET /projects.
const PROJECTS_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// The tools/list entries, in the order agents see them.
pub fn specs() -> Value {
    json!([
        {
            "name": "list_projects",
            "description": "List Rojo Manager projects with server state, port, linked Roblox place, \
                and, when the place is open in Studio, its instanceId and Rojo plugin status.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        },
        {
            "name": "rojo_server",
            "description": "Start, stop, or restart a project's Rojo server and wait for the result. A Studio \
                connected to the project is disconnected cleanly on stop and reconnected on restart.",
            "inputSchema": {"type": "object", "required": ["project", "action"], "additionalProperties": false,
                "properties": {"project": {"type": "string", "description": "Project name"},
                    "action": {"type": "string", "enum": ["start", "stop", "restart"]}}},
        },
        {
            "name": "open_place",
            "description": "Open a Roblox place in Studio and wait until robloxstudio-mcp sees it. Pass a project \
                to use its linked place, or a place ID or link. Passing both links that place to the \
                project. connect_rojo also starts the project's server and connects the Rojo plugin.",
            "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
                "project": {"type": "string", "description": "Project name"},
                "place": {"type": "string", "description": "Place ID, game link, or Creator Hub link"},
                "connect_rojo": {"type": "boolean", "default": false}}},
        },
        {
            "name": "rojo_session",
            "description": "Control the Rojo plugin in the Studio window that has a project's place open: \
                connect (starts the server if needed), disconnect, status, or accept/abort \
                Rojo's initial sync confirmation.",
            "inputSchema": {"type": "object", "required": ["project", "action"], "additionalProperties": false,
                "properties": {"project": {"type": "string", "description": "Project name"},
                    "action": {"type": "string", "enum": ["connect", "disconnect", "status", "accept", "abort"]}}},
        },
        {
            "name": "server_logs",
            "description": "Read a project's recent Rojo server output, oldest first: sync errors, a broken \
                project file, or why the server stopped.",
            "inputSchema": {"type": "object", "required": ["project"], "additionalProperties": false,
                "properties": {"project": {"type": "string", "description": "Project name"},
                    "lines": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100,
                        "description": "How many of the newest lines to return"}}},
        },
        {
            "name": "manage_project",
            "description": "Add, change, or remove Rojo Manager projects. add takes an absolute folder path: a Rojo \
                project, or a parent folder whose project subfolders are all added on free ports. update \
                changes any of name, port, autostart, auto_connect (connect Rojo when the linked place \
                opens in Studio while the server runs), and place (a place ID or link; \"\" unlinks). \
                Renaming or changing the port needs the server stopped. remove drops the project from \
                the list, stopping its server; its files are kept.",
            "inputSchema": {"type": "object", "required": ["action"], "additionalProperties": false, "properties": {
                "action": {"type": "string", "enum": ["add", "update", "remove"]},
                "project": {"type": "string", "description": "Project name, for update and remove"},
                "path": {"type": "string", "description": "Absolute folder path, for add"},
                "name": {"type": "string"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                "autostart": {"type": "boolean", "description": "Start the server when Rojo Manager opens"},
                "auto_connect": {"type": "boolean"},
                "place": {"type": "string"}}},
        },
        {
            "name": "app_settings",
            "description": "Rojo Manager's own preferences, such as launching at Windows startup, the tray, update \
                checks, and which install buttons projects show. Returns them; pass changes to set some. \
                Only change these when the user asks.",
            "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
                "changes": {"type": "object", "additionalProperties": {"type": "boolean"},
                    "description": "Setting name to true or false"}}},
        },
    ])
}

/// Run one tool: its result as compact JSON text, or an error message for the agent.
pub fn run(backend: &dyn Backend, name: &str, args: &Args) -> Result<String, String> {
    match name {
        "list_projects" => text(&list_projects(backend)?),
        "rojo_server" => {
            let (project, action) = project_action(backend, args, &["start", "stop", "restart"])?;
            text(&server_tool(backend, &project, action)?)
        }
        "open_place" => {
            let project = optional_string(args, "project")?;
            let place = optional_string(args, "place")?;
            text(&open_place(backend, project, place, args.get("connect_rojo") == Some(&Value::Bool(true)))?)
        }
        "rojo_session" => {
            let choices = ["connect", "disconnect", "status", "accept", "abort"];
            let (project, action) = project_action(backend, args, &choices)?;
            text(&run_session(backend, &project, action)?)
        }
        "server_logs" => text(&server_logs(backend, args)?),
        "manage_project" => text(&manage_project(backend, args)?),
        "app_settings" => text(&app_settings(backend, args)?),
        _ => Err(format!("Unknown tool {name}")),
    }
}

fn text(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

// --------------------------------------------------------------- manager

/// A project as the manager reports it to agents. Fields agents do not need are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Project {
    name: String,
    state: String,
    port: u16,
    #[serde(default)]
    place: Option<Place>,
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    auto_connect: bool,
    /// Its last output lines.
    #[serde(default)]
    log: Vec<String>,
}

impl Project {
    fn running(&self) -> bool {
        RUNNING.contains(&self.state.as_str())
    }

    fn stopped(&self) -> bool {
        matches!(self.state.as_str(), "off" | "blocked")
    }
}

/// What agents need about a project; paths and logs only appear in errors.
#[derive(Debug, Serialize)]
struct Brief {
    name: String,
    state: String,
    port: u16,
    #[serde(rename = "placeId", skip_serializing_if = "Option::is_none")]
    place_id: Option<u64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    autostart: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    auto_connect: bool,
    /// Only in list_projects, when the linked place is open in Studio.
    #[serde(skip_serializing_if = "Option::is_none")]
    studio: Option<StudioPlace>,
}

impl From<Project> for Brief {
    fn from(project: Project) -> Self {
        Brief {
            name: project.name,
            state: project.state,
            port: project.port,
            place_id: project.place.map(|place| place.place_id),
            autostart: project.autostart,
            auto_connect: project.auto_connect,
            studio: None,
        }
    }
}

/// One field of a manager reply, decoded.
fn field<T: DeserializeOwned>(mut reply: Value, key: &str) -> Result<T, String> {
    let value = reply.get_mut(key).map(Value::take).unwrap_or_default();
    serde_json::from_value(value).map_err(|error| format!("Unexpected reply from Rojo Manager: {error}"))
}

/// Projects from the running manager, starting it hidden if needed.
fn manager_projects(backend: &dyn Backend) -> Result<Vec<Project>, String> {
    let refused = |error: ControlError| format!("Rojo Manager refused the request: {error}");
    let reply = match backend.control("GET", &["projects"], None, PROJECTS_TIMEOUT) {
        Ok(reply) => reply,
        Err(RequestError::Refused(error)) => return Err(refused(error)),
        Err(RequestError::Unreachable(_)) => {
            backend.launch_manager().map_err(|error| format!("Could not start Rojo Manager: {error}"))?;
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if Instant::now() >= deadline {
                    return Err("Rojo Manager did not start. Open it once, or update it to a version with agent \
                        control."
                        .into());
                }
                backend.sleep(Duration::from_millis(200));
                match backend.control("GET", &["projects"], None, PROJECTS_TIMEOUT) {
                    Ok(reply) => break reply,
                    Err(RequestError::Unreachable(_)) => continue,
                    Err(RequestError::Refused(error)) => return Err(refused(error)),
                }
            }
        }
    };
    field(reply, "projects")
}

/// The project with this name, matched exactly or else ignoring case.
fn find_project(backend: &dyn Backend, name: &str) -> Result<Project, String> {
    let mut projects = manager_projects(backend)?;
    let folded = name.to_lowercase();
    let index = projects
        .iter()
        .position(|project| project.name == name)
        .or_else(|| projects.iter().position(|project| project.name.to_lowercase() == folded));
    match index {
        Some(index) => Ok(projects.swap_remove(index)),
        None => {
            let names: Vec<&str> = projects.iter().map(|project| project.name.as_str()).collect();
            let names = if names.is_empty() { "none".to_string() } else { names.join(", ") };
            Err(format!("No project named {name}. Projects: {names}"))
        }
    }
}

fn call(
    backend: &dyn Backend,
    method: &str,
    parts: &[&str],
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value, String> {
    backend.control(method, parts, body, timeout).map_err(|error| match error {
        RequestError::Refused(error) => error.message,
        RequestError::Unreachable(message) => format!("Rojo Manager is not reachable: {message}"),
    })
}

/// A project action (start, update, link-place, ...); returns the project afterwards.
fn act(backend: &dyn Backend, name: &str, action: &str, body: Option<&Value>) -> Result<Project, String> {
    field(call(backend, "POST", &["projects", name, action], body, CALL_TIMEOUT)?, "project")
}

/// Poll the project every 0.1 s until `done` or the timeout; returns the last state seen.
fn wait_for_state(
    backend: &dyn Backend,
    name: &str,
    done: impl Fn(&Project) -> bool,
    timeout: Duration,
) -> Result<Project, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let project = find_project(backend, name)?;
        if done(&project) || Instant::now() >= deadline {
            return Ok(project);
        }
        backend.sleep(Duration::from_millis(100));
    }
}

/// Start, stop, or restart, then wait for the result the agent asked for.
fn run_server(backend: &dyn Backend, name: &str, action: &str) -> Result<Project, String> {
    act(backend, name, action, None)?;
    let stop = action == "stop";
    let project = if stop {
        wait_for_state(backend, name, Project::stopped, Duration::from_secs(10))?
    } else {
        let ready = |project: &Project| project.stopped() || (project.running() && backend.rojo_answers(project.port));
        wait_for_state(backend, name, ready, Duration::from_secs(30))?
    };
    let expected = if stop { project.state == "off" } else { project.running() };
    if !expected {
        return Err(format!("{name} is {} after {action}. Recent output:\n{}", project.state, project.log.join("\n")));
    }
    Ok(project)
}

// --------------------------------------------------------------- studio

/// The Rojo plugin's state as the Rojo Manager bridge in Studio reports it. Serializes only what agents
/// need, leaving out empty fields; others, such as host, are ignored.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Rojo {
    #[serde(skip_serializing_if = "blank")]
    status: Option<String>,
    #[serde(skip_serializing_if = "blank")]
    address: Option<String>,
    #[serde(skip_serializing_if = "blank")]
    error: Option<String>,
    /// The bridge is not loaded in this Studio.
    #[serde(default, skip_serializing)]
    missing: bool,
}

impl Rojo {
    fn status(&self) -> &str {
        self.status.as_deref().unwrap_or_default()
    }
}

fn blank(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(str::is_empty)
}

/// A Studio window with a project's place open.
#[derive(Debug, Serialize)]
struct StudioPlace {
    #[serde(rename = "instanceId")]
    instance_id: String,
    rojo: Rojo,
}

/// placeId -> instanceId for open Studio places, or None when robloxstudio-mcp is not running.
fn studio_instances(backend: &dyn Backend) -> Option<HashMap<u64, String>> {
    let reply = backend.studio_call("get_connected_instances", &json!({})).ok()?;
    let instances = reply.get("instances").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let entry = |item: &Value| Some((item.get("placeId")?.as_u64()?, item.get("id")?.as_str()?.to_string()));
    Some(instances.iter().filter_map(entry).collect())
}

/// The Luau that invokes the Rojo Manager bridge. Arguments are validated literals.
fn bridge_code(action: &str, port: Option<u16>) -> String {
    let args = match port {
        Some(port) if action == "connect" => format!(", \"localhost\", \"{port}\""),
        _ => String::new(),
    };
    format!(
        "local bridge = game:GetService(\"CoreGui\"):FindFirstChild(\"RojoManagerBridge\")\n\
         if not bridge then return {{ missing = true }} end\n\
         return bridge:Invoke(\"{action}\"{args})"
    )
}

/// Invoke the Rojo Manager bridge in one Studio instance.
fn bridge(backend: &dyn Backend, instance_id: &str, action: &str, port: Option<u16>) -> Result<Rojo, String> {
    let result = backend.run_luau(instance_id, &bridge_code(action, port)).map_err(|error| error.0)?;
    let unexpected = || format!("Unexpected bridge response: {result}");
    if !result.is_object() {
        return Err(unexpected());
    }
    let rojo = Rojo::deserialize(&result).map_err(|_| unexpected())?;
    if rojo.missing {
        return Err(BRIDGE_MISSING.into());
    }
    Ok(rojo)
}

/// The Studio instance that has the project's linked place open.
fn place_instance(backend: &dyn Backend, project: &Project) -> Result<String, String> {
    let Some(place) = project.place else {
        return Err(format!("{} has no linked place. Link one with open_place (project and place).", project.name));
    };
    let instances = studio_instances(backend).ok_or("robloxstudio-mcp is not running, so Studio cannot be reached.")?;
    instances
        .get(&place.place_id)
        .cloned()
        .ok_or_else(|| format!("Place {} is not open in Studio. Use open_place first.", place.place_id))
}

#[derive(Debug, Serialize)]
struct Session {
    #[serde(rename = "instanceId")]
    instance_id: String,
    rojo: Rojo,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<&'static str>,
}

/// rojo_session: connect (starting the server if needed and waiting for the plugin), or pass any
/// other action straight to the bridge.
fn run_session(backend: &dyn Backend, name: &str, action: &str) -> Result<Session, String> {
    let mut project = find_project(backend, name)?;
    let instance_id = place_instance(backend, &project)?;
    if action != "connect" {
        let rojo = bridge(backend, &instance_id, action, None)?;
        return Ok(Session { instance_id, rojo, next: None });
    }
    if !project.running() {
        project = run_server(backend, name, "start")?;
    }
    let mut rojo = bridge(backend, &instance_id, "connect", Some(project.port))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !matches!(rojo.status(), "Connected" | "Confirming" | "Error") && Instant::now() < deadline {
        backend.sleep(Duration::from_millis(250));
        rojo = bridge(backend, &instance_id, "status", None)?;
    }
    let next = match rojo.status() {
        "Confirming" => Some("Rojo is waiting for the initial sync to be approved. Use action accept or abort."),
        "Connected" => None,
        status => {
            let reason = rojo.error.as_deref().filter(|error| !error.is_empty()).unwrap_or(status);
            return Err(format!("Rojo did not connect: {reason}"));
        }
    };
    Ok(Session { instance_id, rojo, next })
}

/// The Studio instance whose Rojo plugin is connected to this project, if any.
fn connected_instance(backend: &dyn Backend, project: &Project) -> Option<String> {
    let instance_id = place_instance(backend, project).ok()?;
    let rojo = bridge(backend, &instance_id, "status", None).ok()?;
    (rojo.status() == "Connected").then_some(instance_id)
}

/// A project after a change, plus the reconnected Rojo plugin after a restart.
#[derive(Debug, Serialize)]
struct ProjectResult {
    project: Brief,
    #[serde(skip_serializing_if = "Option::is_none")]
    rojo: Option<Rojo>,
}

/// rojo_server: keeps a connected Studio in step. Stopping disconnects Rojo cleanly first, and
/// restarting reconnects it, instead of leaving the plugin in an error state.
fn server_tool(backend: &dyn Backend, name: &str, action: &str) -> Result<ProjectResult, String> {
    let instance_id = match action {
        "stop" | "restart" => connected_instance(backend, &find_project(backend, name)?),
        _ => None,
    };
    if let Some(instance_id) = &instance_id
        && action == "stop"
    {
        bridge(backend, instance_id, "disconnect", None)?;
    }
    let project = run_server(backend, name, action)?.into();
    let rojo = match instance_id {
        Some(_) if action == "restart" => Some(run_session(backend, name, "connect")?.rojo),
        _ => None,
    };
    Ok(ProjectResult { project, rojo })
}

#[derive(Debug, Serialize)]
struct Opened {
    #[serde(rename = "placeId")]
    place_id: u64,
    #[serde(rename = "instanceId", skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    opened: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rojo: Option<Rojo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<&'static str>,
}

/// open_place: reuses a Studio that already has the place open, otherwise opens it and waits up to
/// 3 minutes for robloxstudio-mcp to see it.
fn open_place(
    backend: &dyn Backend,
    project_name: Option<String>,
    place_text: Option<String>,
    connect_rojo: bool,
) -> Result<Opened, String> {
    let mut project = project_name.map(|name| find_project(backend, &name)).transpose()?;
    if let (Some(found), Some(text)) = (&project, &place_text) {
        let linked = act(backend, &found.name, "link-place", Some(&json!({ "place": text })))?;
        project = Some(linked);
    }
    let place = match (&project, &place_text) {
        (Some(project), _) => {
            project.place.ok_or_else(|| format!("{} has no linked place. Pass place to link one.", project.name))?
        }
        (None, Some(text)) => backend.resolve_place(text).map_err(|error| error.to_string())?,
        (None, None) => return Err("Pass a project name or a place ID or link.".into()),
    };
    if connect_rojo && project.is_none() {
        return Err("connect_rojo needs a project, so Rojo Manager knows which server to use.".into());
    }

    let instances = studio_instances(backend);
    let mut instance_id = instances.as_ref().and_then(|instances| instances.get(&place.place_id).cloned());
    let opened = instance_id.is_none();
    if opened {
        backend.open_place(&place).map_err(|error| format!("Could not open Roblox Studio: {error}"))?;
        if instances.is_none() {
            let mut note = "robloxstudio-mcp is not running, so the opened place cannot be confirmed.";
            if let Some(project) = project.filter(|_| connect_rojo) {
                run_server(backend, &project.name, "start")?;
                note = "robloxstudio-mcp is not running, so the opened place cannot be confirmed and Rojo was \
                        not connected. The Rojo server is running; connect it from the Rojo plugin in Studio.";
            }
            return Ok(Opened {
                place_id: place.place_id,
                instance_id: None,
                opened,
                note: Some(note),
                rojo: None,
                next: None,
            });
        }
        let deadline = Instant::now() + Duration::from_secs(180);
        while instance_id.is_none() && Instant::now() < deadline {
            backend.sleep(Duration::from_secs(2));
            instance_id = studio_instances(backend).and_then(|mut instances| instances.remove(&place.place_id));
        }
        if instance_id.is_none() {
            return Err(format!(
                "Studio was asked to open place {}, but it did not connect to robloxstudio-mcp within 3 minutes.",
                place.place_id
            ));
        }
    }
    let mut result = Opened { place_id: place.place_id, instance_id, opened, note: None, rojo: None, next: None };
    if let Some(project) = project.filter(|_| connect_rojo) {
        let session = run_session(backend, &project.name, "connect")?;
        result.instance_id = Some(session.instance_id);
        result.rojo = Some(session.rojo);
        result.next = session.next;
    }
    Ok(result)
}

#[derive(Debug, Serialize)]
struct ProjectList {
    projects: Vec<Brief>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
}

fn list_projects(backend: &dyn Backend) -> Result<ProjectList, String> {
    let instances = studio_instances(backend);
    let projects = manager_projects(backend)?
        .into_iter()
        .map(|project| {
            let mut item = Brief::from(project);
            let instance_id = item.place_id.and_then(|place_id| instances.as_ref()?.get(&place_id).cloned());
            item.studio = instance_id.map(|instance_id| {
                let rojo = bridge(backend, &instance_id, "status", None)
                    .unwrap_or_else(|error| Rojo { error: Some(error), ..Rojo::default() });
                StudioPlace { instance_id, rojo }
            });
            item
        })
        .collect();
    let note = instances.is_none().then_some("robloxstudio-mcp is not running, so Studio state is unknown.");
    Ok(ProjectList { projects, note })
}

// --------------------------------------------------------------- logs

#[derive(Debug, Deserialize, Serialize)]
struct Logs {
    project: String,
    state: String,
    lines: Vec<String>,
}

/// server_logs: the newest `lines` lines (100 by default) of a project's Rojo output.
fn server_logs(backend: &dyn Backend, args: &Args) -> Result<Logs, String> {
    let count = match args.get("lines") {
        None | Some(Value::Null) => 100,
        Some(value) => value.as_u64().filter(|lines| (1..=500).contains(lines)).ok_or("lines must be 1 to 500")?,
    };
    let name = find_project(backend, &string(args, "project")?)?.name;
    let reply = call(backend, "GET", &["projects", &name, "logs"], None, CALL_TIMEOUT)?;
    let mut logs: Logs =
        serde_json::from_value(reply).map_err(|error| format!("Unexpected reply from Rojo Manager: {error}"))?;
    let older = logs.lines.len().saturating_sub(count as usize);
    logs.lines.drain(..older);
    Ok(logs)
}

// --------------------------------------------------------------- settings

/// Tool argument names, mapped to the fields Rojo Manager stores. Sorted by stored name, so request
/// bodies are identical whether or not serde_json's `preserve_order` is unified in (GPUI enables it).
const PROJECT_FIELDS: [(&str, &str); 5] = [
    ("auto_connect", "autoConnect"),
    ("autostart", "autostart"),
    ("name", "name"),
    ("place", "place"),
    ("port", "port"),
];

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Managed {
    Added {
        added: Vec<Brief>,
    },
    Updated(Box<ProjectResult>),
    /// The manager's reply, passed on.
    Removed(Value),
}

fn manage_project(backend: &dyn Backend, args: &Args) -> Result<Managed, String> {
    let action = choice(args, "action", &["add", "update", "remove"])?;
    if action == "add" {
        let path = string(args, "path")?;
        manager_projects(backend)?; // starts the manager if needed
        let body = json!({ "path": path });
        let reply = call(backend, "POST", &["projects"], Some(&body), Duration::from_secs(30))?;
        let added: Vec<Project> = field(reply, "added")?;
        return Ok(Managed::Added { added: added.into_iter().map(Brief::from).collect() });
    }
    let name = find_project(backend, &string(args, "project")?)?.name;
    if action == "remove" {
        return Ok(Managed::Removed(call(backend, "POST", &["projects", &name, "remove"], None, CALL_TIMEOUT)?));
    }
    let changes: Map<String, Value> =
        PROJECT_FIELDS.iter().filter_map(|(key, stored)| Some((stored.to_string(), args.get(*key)?.clone()))).collect();
    if changes.is_empty() {
        let keys: Vec<&str> = PROJECT_FIELDS.iter().map(|(key, _)| *key).collect();
        return Err(format!("Pass at least one of: {}", keys.join(", ")));
    }
    let project = act(backend, &name, "update", Some(&Value::Object(changes)))?;
    Ok(Managed::Updated(Box::new(ProjectResult { project: project.into(), rojo: None })))
}

fn app_settings(backend: &dyn Backend, args: &Args) -> Result<Value, String> {
    let changes = match args.get("changes") {
        None | Some(Value::Null) => None,
        Some(Value::Object(changes)) => Some(changes),
        Some(_) => return Err("changes must be an object of setting names to true or false".into()),
    };
    manager_projects(backend)?; // starts the manager if needed
    let reply = match changes {
        Some(changes) if !changes.is_empty() => {
            call(backend, "POST", &["settings"], Some(&Value::Object(changes.clone())), CALL_TIMEOUT)?
        }
        _ => call(backend, "GET", &["settings"], None, CALL_TIMEOUT)?,
    };
    field(reply, "settings")
}

// --------------------------------------------------------------- arguments

/// A required nonempty string argument, trimmed.
fn string(args: &Args, key: &str) -> Result<String, String> {
    optional_string(args, key)?.ok_or_else(|| format!("{key} must be a nonempty string"))
}

/// A nonempty string argument, trimmed; None when it is missing or null.
fn optional_string(args: &Args, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() => Ok(Some(value.trim().to_string())),
        Some(_) => Err(format!("{key} must be a nonempty string")),
    }
}

fn choice(args: &Args, key: &str, choices: &[&'static str]) -> Result<&'static str, String> {
    let value = args.get(key).and_then(Value::as_str);
    choices
        .iter()
        .copied()
        .find(|choice| Some(*choice) == value)
        .ok_or_else(|| format!("{key} must be one of: {}", choices.join(", ")))
}

/// Validate the action before contacting Rojo Manager, then resolve the project name.
fn project_action(
    backend: &dyn Backend,
    args: &Args,
    choices: &[&'static str],
) -> Result<(String, &'static str), String> {
    let action = choice(args, "action", choices)?;
    Ok((find_project(backend, &string(args, "project")?)?.name, action))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{Fake, Script, project, studio_open};

    fn args(value: Value) -> Args {
        match value {
            Value::Object(args) => args,
            _ => Args::new(),
        }
    }

    fn json_text(value: &impl Serialize) -> String {
        serde_json::to_string(value).unwrap()
    }

    #[test]
    fn invalid_arguments_are_tool_errors_without_contacting_the_manager() {
        let fake = Fake::default();
        let error = run(&fake, "rojo_server", &args(json!({"project": "TestGame", "action": "delete"})));
        assert_eq!(error.unwrap_err(), "action must be one of: start, stop, restart");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn an_unreachable_manager_is_started_hidden_then_polled() {
        let fake = Fake {
            projects: Script::new([Value::Null, Value::Null, json!([project("off", true)])]),
            ..Fake::default()
        };
        assert_eq!(manager_projects(&fake).unwrap()[0].name, "TestGame");
        assert_eq!(fake.calls(), ["GET projects", "launch", "GET projects", "GET projects"]);
    }

    #[test]
    fn manage_project_maps_arguments_to_manager_requests() {
        let fake = Fake {
            projects: Script::new([json!([project("off", true)])]),
            reply: |_, parts| match parts {
                ["projects"] => {
                    let mut lobby = project("off", false);
                    lobby["name"] = json!("Lobby");
                    json!({"added": [lobby]})
                }
                [.., "update"] => {
                    let mut updated = project("off", true);
                    updated["autoConnect"] = json!(true);
                    json!({"project": updated})
                }
                _ => json!({"removed": parts[1]}),
            },
            ..Fake::default()
        };
        let added = manage_project(&fake, &args(json!({"action": "add", "path": "C:/Games/Lobby"}))).unwrap();
        let update = json!({"action": "update", "project": "testgame", "auto_connect": true, "port": 34900});
        let updated = manage_project(&fake, &args(update)).unwrap();
        let removed = manage_project(&fake, &args(json!({"action": "remove", "project": "TestGame"}))).unwrap();
        let error = manage_project(&fake, &args(json!({"action": "update", "project": "TestGame"}))).unwrap_err();

        assert_eq!(json_text(&added), r#"{"added":[{"name":"Lobby","state":"off","port":34879}]}"#);
        assert_eq!(
            json_text(&updated),
            r#"{"project":{"name":"TestGame","state":"off","port":34879,"placeId":2,"auto_connect":true}}"#
        );
        assert_eq!(json_text(&removed), r#"{"removed":"TestGame"}"#);
        assert_eq!(error, "Pass at least one of: auto_connect, autostart, name, place, port");
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                r#"POST projects {"path":"C:/Games/Lobby"}"#,
                "GET projects",
                r#"POST projects/TestGame/update {"autoConnect":true,"port":34900}"#,
                "GET projects",
                "POST projects/TestGame/remove",
                "GET projects",
            ]
        );
    }

    #[test]
    fn server_logs_returns_the_newest_lines() {
        let fake = Fake {
            projects: Script::new([json!([project("serving", false)])]),
            reply: |_, _| json!({"project": "TestGame", "state": "serving", "lines": ["a", "b", "c"]}),
            ..Fake::default()
        };
        let logs = server_logs(&fake, &args(json!({"project": "testgame", "lines": 2}))).unwrap();
        assert_eq!(json_text(&logs), r#"{"project":"TestGame","state":"serving","lines":["b","c"]}"#);
        assert_eq!(fake.calls(), ["GET projects", "GET projects/TestGame/logs"]);
        let error = server_logs(&fake, &args(json!({"project": "TestGame", "lines": 0}))).unwrap_err();
        assert_eq!(error, "lines must be 1 to 500");
    }

    #[test]
    fn app_settings_reads_or_changes() {
        let fake = Fake { reply: |_, _| json!({"settings": {"tray": false}}), ..Fake::default() };
        assert_eq!(app_settings(&fake, &Args::new()).unwrap(), json!({"tray": false}));
        app_settings(&fake, &args(json!({"changes": {"tray": false}}))).unwrap();
        let error = app_settings(&fake, &args(json!({"changes": ["tray"]}))).unwrap_err();
        assert_eq!(error, "changes must be an object of setting names to true or false");
        assert_eq!(fake.calls(), ["GET projects", "GET settings", "GET projects", r#"POST settings {"tray":false}"#]);
    }

    #[test]
    fn start_waits_for_rojo_to_answer_and_reports_failures_with_logs() {
        let fake = Fake {
            projects: Script::new([json!([project("starting", true)]), json!([project("serving", true)])]),
            answers: Script::new([false, true]),
            ..Fake::default()
        };
        assert_eq!(run_server(&fake, "TestGame", "start").unwrap().state, "serving");
        assert_eq!(fake.calls(), ["POST projects/TestGame/start", "GET projects", "GET projects", "GET projects"]);

        let fake = Fake { projects: Script::new([json!([project("off", true)])]), ..Fake::default() };
        let error = run_server(&fake, "TestGame", "start").unwrap_err();
        assert_eq!(error, "TestGame is off after start. Recent output:\nerror: port in use");
    }

    #[test]
    fn connect_starts_the_server_and_waits_for_confirmation() {
        let fake = Fake {
            projects: Script::new([json!([project("off", true)]), json!([project("serving", true)])]),
            instances: studio_open(),
            luau: Script::new([
                json!({"status": "Connecting"}),
                json!({"status": "Connecting"}),
                json!({"status": "Confirming"}),
            ]),
            ..Fake::default()
        };
        let session = run_session(&fake, "TestGame", "connect").unwrap();
        assert_eq!(
            json_text(&session),
            r#"{"instanceId":"instance:a","rojo":{"status":"Confirming"},"next":"Rojo is waiting for the initial sync to be approved. Use action accept or abort."}"#
        );
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                "POST projects/TestGame/start",
                "GET projects",
                r#"luau instance:a return bridge:Invoke("connect", "localhost", "34879")"#,
                r#"luau instance:a return bridge:Invoke("status")"#,
                r#"luau instance:a return bridge:Invoke("status")"#,
            ]
        );
    }

    #[test]
    fn stop_disconnects_and_restart_reconnects_a_connected_studio() {
        let fake = Fake {
            projects: Script::new([json!([project("serving", true)]), json!([project("off", true)])]),
            instances: studio_open(),
            ..Fake::default()
        };
        let stopped = server_tool(&fake, "TestGame", "stop").unwrap();
        assert_eq!(json_text(&stopped), r#"{"project":{"name":"TestGame","state":"off","port":34879,"placeId":2}}"#);
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                r#"luau instance:a return bridge:Invoke("status")"#,
                r#"luau instance:a return bridge:Invoke("disconnect")"#,
                "POST projects/TestGame/stop",
                "GET projects",
            ]
        );

        let fake = Fake {
            projects: Script::new([json!([project("serving", true)])]),
            instances: studio_open(),
            ..Fake::default()
        };
        let restarted = server_tool(&fake, "TestGame", "restart").unwrap();
        assert_eq!(
            json_text(&restarted),
            r#"{"project":{"name":"TestGame","state":"serving","port":34879,"placeId":2},"rojo":{"status":"Connected"}}"#
        );
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                r#"luau instance:a return bridge:Invoke("status")"#,
                "POST projects/TestGame/restart",
                "GET projects",
                "GET projects",
                r#"luau instance:a return bridge:Invoke("connect", "localhost", "34879")"#,
            ]
        );
    }

    #[test]
    fn list_is_compact_and_shows_rojo_only_for_open_places() {
        let mut test_game = project("serving", true);
        test_game["path"] = json!("C:/y");
        let mut lobby = project("off", false);
        lobby["name"] = json!("Lobby");
        lobby["projectFile"] = json!("default.project.json");
        let fake = Fake {
            projects: Script::new([json!([test_game, lobby])]),
            instances: studio_open(),
            luau: Script::new([
                json!({"status": "Connected", "address": "localhost:34879", "host": "localhost", "bridge": 1}),
            ]),
            ..Fake::default()
        };
        assert_eq!(
            json_text(&list_projects(&fake).unwrap()),
            r#"{"projects":[{"name":"TestGame","state":"serving","port":34879,"placeId":2,"studio":{"instanceId":"instance:a","rojo":{"status":"Connected","address":"localhost:34879"}}},{"name":"Lobby","state":"off","port":34879}]}"#
        );

        let fake = Fake { instances: None, ..Fake::default() };
        assert_eq!(
            json_text(&list_projects(&fake).unwrap()),
            r#"{"projects":[],"note":"robloxstudio-mcp is not running, so Studio state is unknown."}"#
        );
    }

    #[test]
    fn open_place_with_project_and_place_links_it_first() {
        let fake = Fake {
            projects: Script::new([json!([project("off", false)])]),
            reply: |_, _| json!({"project": project("off", true)}),
            instances: studio_open(),
            ..Fake::default()
        };
        let opened = open_place(&fake, Some("TestGame".into()), Some("https://www.roblox.com/games/2".into()), false);
        assert_eq!(json_text(&opened.unwrap()), r#"{"placeId":2,"instanceId":"instance:a"}"#);
        assert_eq!(
            fake.calls(),
            ["GET projects", r#"POST projects/TestGame/link-place {"place":"https://www.roblox.com/games/2"}"#]
        );
    }

    #[test]
    fn open_place_opens_studio_when_the_place_is_not_open() {
        let fake = Fake { instances: None, ..Fake::default() };
        let opened = open_place(&fake, None, Some("2".into()), false).unwrap();
        assert_eq!(
            json_text(&opened),
            r#"{"placeId":2,"opened":true,"note":"robloxstudio-mcp is not running, so the opened place cannot be confirmed."}"#
        );
        assert_eq!(fake.calls(), ["resolve 2", "open 2"]);
        let error = open_place(&fake, None, Some("2".into()), true).unwrap_err();
        assert_eq!(error, "connect_rojo needs a project, so Rojo Manager knows which server to use.");

        // connect_rojo without robloxstudio-mcp still starts the server, and says Rojo is not connected.
        let fake = Fake {
            projects: Script::new([json!([project("off", true)]), json!([project("serving", true)])]),
            instances: None,
            ..Fake::default()
        };
        let opened = open_place(&fake, Some("TestGame".into()), None, true).unwrap();
        assert!(opened.opened && opened.note.unwrap().contains("Rojo was not connected"));
        assert!(fake.calls().contains(&"POST projects/TestGame/start".to_string()));
    }

    #[test]
    fn sessions_need_a_linked_place_that_is_open() {
        let fake = Fake { projects: Script::new([json!([project("off", false)])]), ..Fake::default() };
        let error = run_session(&fake, "TestGame", "status").unwrap_err();
        assert_eq!(error, "TestGame has no linked place. Link one with open_place (project and place).");

        let fake = Fake { projects: Script::new([json!([project("off", true)])]), ..Fake::default() };
        let error = run_session(&fake, "TestGame", "status").unwrap_err();
        assert_eq!(error, "Place 2 is not open in Studio. Use open_place first.");
    }

    #[test]
    fn open_place_reuses_an_open_studio_and_reports_a_missing_bridge() {
        let fake = Fake {
            projects: Script::new([json!([project("off", true)])]),
            instances: studio_open(),
            luau: Script::new([json!({"missing": true})]),
            ..Fake::default()
        };
        let opened = open_place(&fake, Some("TestGame".into()), None, false).unwrap();
        assert_eq!(json_text(&opened), r#"{"placeId":2,"instanceId":"instance:a"}"#);
        assert_eq!(fake.calls(), ["GET projects"]);
        assert_eq!(bridge(&fake, "instance:a", "status", None).unwrap_err(), BRIDGE_MISSING);
    }

    #[test]
    fn bridge_code_is_unchanged() {
        assert_eq!(
            bridge_code("connect", Some(34879)),
            "local bridge = game:GetService(\"CoreGui\"):FindFirstChild(\"RojoManagerBridge\")\n\
             if not bridge then return { missing = true } end\n\
             return bridge:Invoke(\"connect\", \"localhost\", \"34879\")"
        );
        assert!(bridge_code("status", Some(34879)).ends_with("\nreturn bridge:Invoke(\"status\")"));
    }

    #[test]
    fn projects_match_exactly_then_ignoring_case() {
        let mut other = project("off", true);
        other["name"] = json!("testgame");
        let fake = Fake { projects: Script::new([json!([other, project("serving", true)])]), ..Fake::default() };
        assert_eq!(find_project(&fake, "TestGame").unwrap().state, "serving");
        assert_eq!(find_project(&fake, "TESTGAME").unwrap().state, "off");
        assert_eq!(find_project(&fake, "Lobby").unwrap_err(), "No project named Lobby. Projects: testgame, TestGame");
    }
}
