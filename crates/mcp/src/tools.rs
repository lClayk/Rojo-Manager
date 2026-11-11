//! The tools agents call. Rojo processes stay owned by the Rojo Manager app, reached through its
//! control API (it is started hidden when it is not running). Studio is reached through the same API:
//! the Rojo Manager bridge patched into the Rojo plugin polls the app for commands.
//!
//! Results are typed structs serialized in field order, so agents read the same compact JSON as
//! before. Failures are plain messages, reported to the agent as tool errors.

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
const BRIDGE_MISSING: &str = "The Rojo Manager bridge is not installed. Install it from Rojo Manager \
    (Settings > Agents), turn off the Creator Store Rojo plugin, then restart Studio.";
/// Tools the studio_tools setting hides, for people whose other Studio MCP has them.
const STUDIO_TOOLS: [&str; 2] = ["get_connected_instances", "execute_luau"];
/// Longer than the manager waits for Luau to finish in Studio.
const LUAU_CALL_TIMEOUT: Duration = Duration::from_secs(70);
/// control.request's default, used for GET /projects.
const PROJECTS_TIMEOUT: Duration = Duration::from_secs(15);
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// The tools/list entries, in the order agents see them; `studio_tools` adds the Studio ones.
pub fn specs(studio_tools: bool) -> Value {
    let mut specs = json!([
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
            "description": "Open a Roblox place in Studio and wait until its Rojo Manager bridge reports in. Pass a project \
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
                opens in Studio while the server runs), sourcemap (keep sourcemap.json current with rojo \
                sourcemap --watch while the server runs), and place (a place ID or link; \"\" unlinks). \
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
                "sourcemap": {"type": "boolean"},
                "place": {"type": "string"}}},
        },
        {
            "name": "updates",
            "description": "Check whether Rojo Manager and its Studio bridge plugin are up to date, or install an \
                update. install app downloads the newest release and restarts Rojo Manager; its running servers \
                restart with it, so reconnect Rojo afterwards. install bridge rebuilds the bridge for the \
                installed Rojo and waits for it; Studio needs a restart to load it. Install when the user asks.",
            "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
                "install": {"type": "string", "enum": ["app", "bridge"]}}},
        },
        {
            "name": "app_settings",
            "description": "Rojo Manager's own preferences, such as launching at sign-in, the tray, update \
                checks, and which install buttons projects show. Returns them; pass changes to set some. \
                Only change these when the user asks.",
            "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
                "changes": {"type": "object", "additionalProperties": {"type": "boolean"},
                    "description": "Setting name to true or false"}}},
        },
        {
            "name": "get_connected_instances",
            "description": "Use to discover the Studio windows Rojo Manager's bridge reaches, with each one's \
                instanceId, placeId, linked project, and Rojo plugin status.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
        },
        {
            "name": "execute_luau",
            "description": "Use for custom Studio traversal, edits, or inspection. Runs with plugin permissions \
                in Edit mode and returns the value it returns and what it prints.",
            "inputSchema": {"type": "object", "required": ["code"], "additionalProperties": false, "properties": {
                "code": {"type": "string", "description": "Luau code to execute."},
                "instance_id": {"type": "string", "description": "Studio instance ID when ambiguous."}}},
        },
    ]);
    if !studio_tools && let Some(list) = specs.as_array_mut() {
        list.retain(|spec| !STUDIO_TOOLS.contains(&spec["name"].as_str().unwrap_or_default()));
    }
    specs
}

/// Run one tool: its result as compact JSON text, or an error message for the agent.
pub fn run(backend: &dyn Backend, name: &str, args: &Args) -> Result<String, String> {
    if STUDIO_TOOLS.contains(&name) && !backend.studio_tools() {
        return Err(format!("Unknown tool {name}"));
    }
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
        "updates" => text(&updates(backend, args)?),
        "app_settings" => text(&app_settings(backend, args)?),
        "get_connected_instances" => text(&connected_instances(backend)?),
        "execute_luau" => text(&execute_luau(backend, args)?),
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
    /// The linked place's experience name, once Rojo Manager looked it up.
    #[serde(default)]
    experience: Option<String>,
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    auto_connect: bool,
    #[serde(default)]
    sourcemap: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    experience: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    autostart: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    auto_connect: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    sourcemap: bool,
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
            experience: project.experience,
            autostart: project.autostart,
            auto_connect: project.auto_connect,
            sourcemap: project.sourcemap,
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

/// A Studio window whose Rojo Manager bridge reports to the manager.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instance {
    instance_id: String,
    place_id: u64,
    #[serde(default)]
    rojo: Rojo,
}

/// The Studio windows the bridge reports from, or None when the bridge is not installed.
fn studio_instances(backend: &dyn Backend) -> Result<Option<Vec<Instance>>, String> {
    let reply = call(backend, "GET", &["studio"], None, CALL_TIMEOUT)?;
    let installed = reply.get("bridge") == Some(&Value::Bool(true));
    let instances: Vec<Instance> = field(reply, "instances")?;
    Ok((installed || !instances.is_empty()).then_some(instances))
}

/// The open Studio window with this place, if any.
fn find_instance(instances: &[Instance], place_id: u64) -> Option<String> {
    instances.iter().find(|instance| instance.place_id == place_id).map(|instance| instance.instance_id.clone())
}

/// Run a Rojo plugin action through the bridge in one Studio window.
fn bridge(backend: &dyn Backend, instance_id: &str, action: &str, port: Option<u16>) -> Result<Rojo, String> {
    let body = match port {
        Some(port) => json!({ "action": action, "port": port }),
        None => json!({ "action": action }),
    };
    let result = call(backend, "POST", &["studio", instance_id], Some(&body), CALL_TIMEOUT)?;
    Rojo::deserialize(&result).map_err(|_| format!("Unexpected bridge response: {result}"))
}

/// The Studio instance that has the project's linked place open.
fn place_instance(backend: &dyn Backend, project: &Project) -> Result<String, String> {
    let Some(place) = project.place else {
        return Err(format!("{} has no linked place. Link one with open_place (project and place).", project.name));
    };
    let instances = studio_instances(backend)?.ok_or(BRIDGE_MISSING)?;
    find_instance(&instances, place.place_id).ok_or_else(|| {
        format!(
            "Place {} is not open in Studio, or its Rojo Manager bridge is not loaded. Use open_place first.",
            place.place_id
        )
    })
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
/// 3 minutes for its bridge to report in.
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
    if project.is_none() {
        manager_projects(backend)?; // starts the manager if needed
    }

    let instances = studio_instances(backend)?;
    let mut instance_id = instances.as_deref().and_then(|instances| find_instance(instances, place.place_id));
    let opened = instance_id.is_none();
    if opened {
        backend.open_place(&place).map_err(|error| format!("Could not open Roblox Studio: {error}"))?;
        if instances.is_none() {
            let mut note = "The Rojo Manager bridge is not installed, so the opened place cannot be confirmed.";
            if let Some(project) = project.filter(|_| connect_rojo) {
                run_server(backend, &project.name, "start")?;
                note = "The Rojo Manager bridge is not installed, so the opened place cannot be confirmed and \
                        Rojo was not connected. The Rojo server is running; connect it from the Rojo plugin in Studio.";
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
            let instances = studio_instances(backend)?.unwrap_or_default();
            instance_id = find_instance(&instances, place.place_id);
        }
        if instance_id.is_none() {
            return Err(format!(
                "Studio was asked to open place {}, but its Rojo Manager bridge did not report in within 3 \
                 minutes. Check that the Creator Store Rojo is off, so the bridge's Rojo loads.",
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
    let projects = manager_projects(backend)?;
    let mut instances = studio_instances(backend)?;
    let projects = projects
        .into_iter()
        .map(|project| {
            let mut item = Brief::from(project);
            let open = item.place_id.and_then(|place_id| {
                let instances = instances.as_mut()?;
                let index = instances.iter().position(|instance| instance.place_id == place_id)?;
                Some(instances.swap_remove(index))
            });
            item.studio = open.map(|instance| StudioPlace { instance_id: instance.instance_id, rojo: instance.rojo });
            item
        })
        .collect();
    let note = instances.is_none().then_some("The Rojo Manager bridge is not installed, so Studio state is unknown.");
    Ok(ProjectList { projects, note })
}

#[derive(Debug, Serialize)]
struct ConnectedInstance {
    #[serde(rename = "instanceId")]
    instance_id: String,
    #[serde(rename = "placeId")]
    place_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    rojo: Rojo,
}

/// get_connected_instances: every Studio window the bridge reports from, with its linked project.
fn connected_instances(backend: &dyn Backend) -> Result<Value, String> {
    let projects = manager_projects(backend)?;
    let instances = studio_instances(backend)?.ok_or(BRIDGE_MISSING)?;
    let linked = |place_id| projects.iter().find(|p| p.place.is_some_and(|place| place.place_id == place_id));
    let instances: Vec<ConnectedInstance> = instances
        .into_iter()
        .map(|instance| ConnectedInstance {
            project: linked(instance.place_id).map(|project| project.name.clone()),
            instance_id: instance.instance_id,
            place_id: instance.place_id,
            rojo: instance.rojo,
        })
        .collect();
    Ok(json!({ "instances": instances }))
}

/// execute_luau: run code in the one open Studio, or the one `instance_id` names.
fn execute_luau(backend: &dyn Backend, args: &Args) -> Result<Value, String> {
    let code = match args.get("code") {
        Some(Value::String(code)) if !code.trim().is_empty() => code.clone(),
        _ => return Err("code must be a nonempty string".into()),
    };
    let instance_id = match optional_string(args, "instance_id")? {
        Some(instance_id) => instance_id,
        None => {
            manager_projects(backend)?; // starts the manager if needed
            let instances = studio_instances(backend)?.ok_or(BRIDGE_MISSING)?;
            match instances.as_slice() {
                [only] => only.instance_id.clone(),
                [] => return Err("No Studio window with the Rojo Manager bridge is open.".into()),
                several => {
                    let ids: Vec<String> =
                        several.iter().map(|i| format!("{} (place {})", i.instance_id, i.place_id)).collect();
                    return Err(format!("Several Studio windows are open; pass instance_id: {}", ids.join(", ")));
                }
            }
        }
    };
    let body = json!({ "action": "luau", "code": code });
    call(backend, "POST", &["studio", &instance_id], Some(&body), LUAU_CALL_TIMEOUT)
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
const PROJECT_FIELDS: [(&str, &str); 6] = [
    ("auto_connect", "autoConnect"),
    ("autostart", "autostart"),
    ("name", "name"),
    ("place", "place"),
    ("port", "port"),
    ("sourcemap", "sourcemap"),
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

/// Installing waits for the download, or for the bridge installer and any prompt in its terminal.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

fn updates(backend: &dyn Backend, args: &Args) -> Result<Value, String> {
    let install = optional_string(args, "install")?;
    manager_projects(backend)?; // starts the manager if needed
    match install.as_deref() {
        None => call(backend, "GET", &["updates"], None, PROJECTS_TIMEOUT),
        Some(target @ ("app" | "bridge")) => call(backend, "POST", &["updates", target], None, INSTALL_TIMEOUT),
        Some(_) => Err("install must be app or bridge".into()),
    }
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
    use crate::fake::{Fake, Script, no_bridge, project, studio_open};

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
        assert_eq!(error, "Pass at least one of: auto_connect, autostart, name, place, port, sourcemap");
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
    fn updates_check_or_install_one() {
        let fake = Fake { reply: |_, _| json!({"message": "done"}), ..Fake::default() };
        updates(&fake, &Args::new()).unwrap();
        assert_eq!(updates(&fake, &args(json!({"install": "bridge"}))).unwrap(), json!({"message": "done"}));
        assert_eq!(updates(&fake, &args(json!({"install": "rojo"}))).unwrap_err(), "install must be app or bridge");
        assert_eq!(
            fake.calls(),
            ["GET projects", "GET updates", "GET projects", "POST updates/bridge", "GET projects"]
        );
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
            studio: Script::new([
                json!({"status": "Connecting"}),
                json!({"status": "Connecting"}),
                json!({"status": "Confirming"}),
            ]),
            ..Fake::default()
        };
        let session = run_session(&fake, "TestGame", "connect").unwrap();
        assert_eq!(
            json_text(&session),
            r#"{"instanceId":"7","rojo":{"status":"Confirming"},"next":"Rojo is waiting for the initial sync to be approved. Use action accept or abort."}"#
        );
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                "POST projects/TestGame/start",
                "GET projects",
                r#"POST studio/7 {"action":"connect","port":34879}"#,
                r#"POST studio/7 {"action":"status"}"#,
                r#"POST studio/7 {"action":"status"}"#,
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
                r#"POST studio/7 {"action":"status"}"#,
                r#"POST studio/7 {"action":"disconnect"}"#,
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
                r#"POST studio/7 {"action":"status"}"#,
                "POST projects/TestGame/restart",
                "GET projects",
                "GET projects",
                r#"POST studio/7 {"action":"connect","port":34879}"#,
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
        let rojo = json!({"status": "Connected", "address": "localhost:34879", "host": "localhost", "bridge": 7});
        let fake = Fake {
            projects: Script::new([json!([test_game, lobby])]),
            instances: json!({"bridge": true, "instances": [{"instanceId": "7", "placeId": 2, "rojo": rojo}]}),
            ..Fake::default()
        };
        assert_eq!(
            json_text(&list_projects(&fake).unwrap()),
            r#"{"projects":[{"name":"TestGame","state":"serving","port":34879,"placeId":2,"studio":{"instanceId":"7","rojo":{"status":"Connected","address":"localhost:34879"}}},{"name":"Lobby","state":"off","port":34879}]}"#
        );
        assert_eq!(fake.calls(), ["GET projects"]);

        let fake = Fake { instances: no_bridge(), ..Fake::default() };
        assert_eq!(
            json_text(&list_projects(&fake).unwrap()),
            r#"{"projects":[],"note":"The Rojo Manager bridge is not installed, so Studio state is unknown."}"#
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
        assert_eq!(json_text(&opened.unwrap()), r#"{"placeId":2,"instanceId":"7"}"#);
        assert_eq!(
            fake.calls(),
            ["GET projects", r#"POST projects/TestGame/link-place {"place":"https://www.roblox.com/games/2"}"#]
        );
    }

    #[test]
    fn open_place_opens_studio_when_the_place_is_not_open() {
        let fake = Fake { instances: no_bridge(), ..Fake::default() };
        let opened = open_place(&fake, None, Some("2".into()), false).unwrap();
        assert_eq!(
            json_text(&opened),
            r#"{"placeId":2,"opened":true,"note":"The Rojo Manager bridge is not installed, so the opened place cannot be confirmed."}"#
        );
        assert_eq!(fake.calls(), ["resolve 2", "GET projects", "open 2"]);
        let error = open_place(&fake, None, Some("2".into()), true).unwrap_err();
        assert_eq!(error, "connect_rojo needs a project, so Rojo Manager knows which server to use.");

        // connect_rojo without the bridge still starts the server, and says Rojo is not connected.
        let fake = Fake {
            projects: Script::new([json!([project("off", true)]), json!([project("serving", true)])]),
            instances: no_bridge(),
            ..Fake::default()
        };
        let opened = open_place(&fake, Some("TestGame".into()), None, true).unwrap();
        assert!(opened.opened && opened.note.unwrap().contains("Rojo was not connected"));
        assert!(fake.calls().contains(&"POST projects/TestGame/start".to_string()));
    }

    #[test]
    fn sessions_need_a_linked_place_open_with_the_bridge() {
        let fake = Fake { projects: Script::new([json!([project("off", false)])]), ..Fake::default() };
        let error = run_session(&fake, "TestGame", "status").unwrap_err();
        assert_eq!(error, "TestGame has no linked place. Link one with open_place (project and place).");

        let fake = Fake { projects: Script::new([json!([project("off", true)])]), ..Fake::default() };
        let error = run_session(&fake, "TestGame", "status").unwrap_err();
        assert_eq!(
            error,
            "Place 2 is not open in Studio, or its Rojo Manager bridge is not loaded. Use open_place first."
        );

        let fake =
            Fake { projects: Script::new([json!([project("off", true)])]), instances: no_bridge(), ..Fake::default() };
        assert_eq!(run_session(&fake, "TestGame", "status").unwrap_err(), BRIDGE_MISSING);
    }

    #[test]
    fn open_place_reuses_an_open_studio() {
        let fake = Fake {
            projects: Script::new([json!([project("off", true)])]),
            instances: studio_open(),
            ..Fake::default()
        };
        let opened = open_place(&fake, Some("TestGame".into()), None, false).unwrap();
        assert_eq!(json_text(&opened), r#"{"placeId":2,"instanceId":"7"}"#);
        assert_eq!(fake.calls(), ["GET projects"]);
    }

    #[test]
    fn studio_tools_list_instances_and_run_luau_unless_turned_off() {
        let fake = Fake {
            projects: Script::new([json!([project("serving", true)])]),
            instances: studio_open(),
            studio: Script::new([json!({"success": true, "returnValue": 2, "output": []})]),
            ..Fake::default()
        };
        assert_eq!(
            json_text(&connected_instances(&fake).unwrap()),
            r#"{"instances":[{"instanceId":"7","placeId":2,"project":"TestGame","rojo":{"status":"Connected"}}]}"#
        );
        let result = execute_luau(&fake, &args(json!({"code": "return 1 + 1"}))).unwrap();
        assert_eq!(result["returnValue"], 2);
        assert_eq!(
            fake.calls(),
            ["GET projects", "GET projects", r#"POST studio/7 {"action":"luau","code":"return 1 + 1"}"#]
        );
        assert_eq!(execute_luau(&fake, &args(json!({"code": " "}))).unwrap_err(), "code must be a nonempty string");

        let hidden = Fake { studio_tools: false, ..Fake::default() };
        let error = run(&hidden, "execute_luau", &args(json!({"code": "return 1"}))).unwrap_err();
        assert_eq!(error, "Unknown tool execute_luau");
        let listed = |studio_tools| {
            let specs = specs(studio_tools);
            let names: Vec<&str> = specs.as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
            STUDIO_TOOLS.iter().filter(|tool| names.contains(*tool)).count()
        };
        assert_eq!((listed(true), listed(false)), (2, 0));
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
