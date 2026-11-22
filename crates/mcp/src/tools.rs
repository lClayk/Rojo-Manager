//! The tools agents call. Rojo processes stay owned by the Rojo Manager app, reached through its
//! control API (it is started hidden when it is not running). Studio is reached through the same API:
//! the Rojo Manager bridge patched into the Rojo plugin polls the app for commands.
//!
//! Results are typed structs serialized in field order, so agents read the same compact JSON as
//! before. Failures are plain messages, reported to the agent as tool errors.

use std::time::{Duration, Instant};

use rojo_manager_core::config::{Lane, PlaceLink};
use rojo_manager_core::control::{ControlError, RequestError};
use rojo_manager_core::manager::SyncSummary;
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
/// Longer than Rojo Manager waits for a server to start (30 s) or the bridge for Rojo to connect.
const WAIT_TIMEOUT: Duration = Duration::from_secs(45);
/// The first bridge version that waits for Rojo to connect; older ones are polled.
const WAIT_BRIDGE: u64 = 11;
/// Opening a local place may build it first.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

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
                connected to the project is disconnected cleanly on stop and reconnected on restart. With wait \
                false, it returns right after the request.",
            "inputSchema": {"type": "object", "required": ["project", "action"], "additionalProperties": false,
                "properties": {"project": {"type": "string", "description": "Project name"},
                    "action": {"type": "string", "enum": ["start", "stop", "restart"]},
                    "wait": {"type": "boolean", "default": true,
                        "description": "Return once the server answers or is gone, up to 30 seconds"}}},
        },
        {
            "name": "open_place",
            "description": "Open a Roblox place in Studio and wait until its Rojo Manager bridge reports in. Pass a project \
                to use its linked place, or a place ID or link. Passing both links that place to the \
                project. connect_rojo also starts the project's server and connects the Rojo plugin. hidden true \
                opens the project's place without showing a Studio window (Windows only), for work the user does \
                not need to watch; on a place that is already open, hidden true hides its window and false shows \
                it. idle_close sets how many minutes of nothing happening in that Studio close it again.",
            "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
                "project": {"type": "string", "description": "Project name"},
                "place": {"type": "string", "description": "Place ID, game link, Creator Hub link, or local"},
                "connect_rojo": {"type": "boolean", "default": false},
                "hidden": {"type": "boolean", "description": "Hide or show the Studio window; needs project. \
                    Omit to open visible and leave an open Studio as it is."},
                "idle_close": {"type": "integer", "minimum": 0, "maximum": 10080, "description": "Minutes; saved on \
                    the project, 0 turns it off. Change it later with manage_project update."}}},
        },
        {
            "name": "rojo_session",
            "description": "Control the Rojo plugin in the Studio window that has a project's place open: \
                connect (starts the server if needed), disconnect, status, or accept/abort \
                Rojo's initial sync confirmation. Before Rojo removes instances that exist only in Studio, \
                Rojo Manager backs them up: backups lists them, newest first, and restore puts one back \
                (the newest, or backup) under its old parents in the open place. Rojo removes restored \
                instances again on its next sync unless the project ignores them. pull goes the other way: it \
                writes what Studio has for the folders the project syncs into the project's files with rojo \
                syncback, and answers the files written and removed; preview true only lists them. Use it \
                after edits made in Studio, such as UI built there. Studio's version wins: files for \
                instances Studio does not have are removed, so preview first when the files may be newer \
                than Studio, such as while Rojo waits to confirm its sync. The project file itself is not \
                changed. A script with children is listed when something under it changed, even if its own \
                source did not. While files are written, the project's server is stopped and Rojo in that \
                Studio disconnected; both come back after, and rojo in the answer is Rojo's state when that \
                is not Connected.",
            "inputSchema": {"type": "object", "required": ["project", "action"], "additionalProperties": false,
                "properties": {"project": {"type": "string", "description": "Project name"},
                    "action": {"type": "string",
                        "enum": ["connect", "disconnect", "status", "accept", "abort", "backups", "restore", "pull"]},
                    "backup": {"type": "string", "description": "For restore: a backup id from backups"},
                    "preview": {"type": "boolean", "default": false, "description": "For pull: list the changes \
                        without writing them"},
                    "wait": {"type": "boolean", "default": true, "description": "For connect: return once Rojo \
                        connects, asks to confirm the initial sync, or fails, up to 30 seconds"}}},
        },
        {
            "name": "lane",
            "description": "An isolated Studio for parallel work, such as an agent working in a git worktree. create \
                takes path, an existing copy of the project from (make the worktree first; this does not copy \
                anything), adds it with a local place of its own built from the same project file, opens that in \
                Studio, starts its server, connects Rojo, and accepts the first sync. A local place has no \
                DataStores and none of the published place's Studio-built content. A lane stays across Rojo \
                Manager restarts until remove, which force-closes its Studio (unsaved changes there are lost), \
                stops the server, and deletes the place, or until its folder is deleted. hidden and idle_close \
                work as in open_place.",
            "inputSchema": {"type": "object", "required": ["action"], "additionalProperties": false, "properties": {
                "action": {"type": "string", "enum": ["create", "remove"]},
                "from": {"type": "string", "description": "Project to make the lane from, for create"},
                "path": {"type": "string", "description": "Absolute folder of the lane's copy, for create"},
                "lane": {"type": "string", "description": "Lane name, for remove"},
                "holder": {"type": "string", "description": "A label shown in Rojo Manager, such as \"lane B\", for create"},
                "hidden": {"type": "boolean", "description": "For create: open its Studio without a window (Windows only)"},
                "idle_close": {"type": "integer", "minimum": 0, "maximum": 10080, "description": "For create: minutes \
                    of nothing happening in its Studio after which it closes; the lane stays"}}},
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
            "description": "Add, make, change, or remove Rojo Manager projects. add takes an absolute folder path: \
                a Rojo project, or a parent folder whose project subfolders are all added on free ports. create \
                makes a new project with rojo init in path, a new or empty folder. convert makes one in path from \
                file, a .rbxl place: its code services (ReplicatedStorage, ServerScriptService, StarterGui, \
                StarterPlayer scripts and the like) become files with rojo syncback, and a copy of the place, map \
                and all, becomes its local place. Both add the project with a local place and auto_connect. update \
                changes any of name, port, autostart, auto_connect (connect Rojo when the linked place \
                opens in Studio while the server runs), sourcemap (keep sourcemap.json current with rojo \
                sourcemap --watch while the server runs), idle_close (minutes of nothing happening in the place's \
                Studio after which Rojo Manager closes it, losing unsaved changes there, and stops the server \
                unless it autostarts; 0 turns it off), and place (a place ID or link, local for an unpublished \
                place file built from the project, or \"\" to unlink). \
                Renaming or changing the port needs the server stopped. remove drops the project from \
                the list, stopping its server; its files are kept, except a local place file.",
            "inputSchema": {"type": "object", "required": ["action"], "additionalProperties": false, "properties": {
                "action": {"type": "string", "enum": ["add", "create", "convert", "update", "remove"]},
                "project": {"type": "string", "description": "Project name, for update and remove"},
                "path": {"type": "string", "description": "Absolute folder path, for add, create, and convert"},
                "file": {"type": "string", "description": "Absolute .rbxl place file path, for convert"},
                "name": {"type": "string"},
                "port": {"type": "integer", "minimum": 1, "maximum": 65535},
                "autostart": {"type": "boolean", "description": "Start the server when Rojo Manager opens"},
                "auto_connect": {"type": "boolean"},
                "sourcemap": {"type": "boolean"},
                "idle_close": {"type": "integer", "minimum": 0, "maximum": 10080},
                "place": {"type": "string"}}},
        },
        {
            "name": "studio_output",
            "description": "Read Studio's Output for an open place, oldest first: prints, warnings, and errors from Edit \
                mode and from a playtest's server (a playtest's client cannot send its Output), each with its \
                context and kind. Pass a project to \
                use its linked place, or instance_id. Pass since, the next value of the previous read, to get \
                only newer lines; level warning or error leaves out plain prints.",
            "inputSchema": {"type": "object", "additionalProperties": false, "properties": {
                "project": {"type": "string", "description": "Project name"},
                "instance_id": {"type": "string", "description": "Studio instance ID when ambiguous."},
                "since": {"type": "integer", "minimum": 0, "description": "next from the previous read"},
                "lines": {"type": "integer", "minimum": 1, "maximum": 500, "default": 100,
                    "description": "How many of the newest matching lines to return"},
                "level": {"type": "string", "enum": ["all", "warning", "error"], "default": "all"}}},
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
            text(&server_tool(backend, &project, action, wait(args)?)?)
        }
        "open_place" => {
            let project = optional_string(args, "project")?;
            let place = optional_string(args, "place")?;
            let connect_rojo = args.get("connect_rojo") == Some(&Value::Bool(true));
            text(&open_place(backend, project, place, connect_rojo, StudioOptions::read(args)?)?)
        }
        "rojo_session" => {
            let choices = ["connect", "disconnect", "status", "accept", "abort", "backups", "restore", "pull"];
            let (project, action) = project_action(backend, args, &choices)?;
            match action {
                "pull" => {
                    let body = json!({ "preview": args.get("preview") == Some(&Value::Bool(true)) });
                    // syncback reads the whole place
                    let timeout = Duration::from_secs(300);
                    text(&call(backend, "POST", &["projects", &project, "pull"], Some(&body), timeout)?)
                }
                "backups" => text(&call(backend, "GET", &["projects", &project, "backups"], None, CALL_TIMEOUT)?),
                "restore" => {
                    let body = match optional_string(args, "backup")? {
                        Some(id) => json!({ "backup": id }),
                        None => json!({}),
                    };
                    let timeout = Duration::from_secs(90);
                    text(&call(backend, "POST", &["projects", &project, "restore"], Some(&body), timeout)?)
                }
                _ => text(&run_session(backend, &project, action, wait(args)?)?),
            }
        }
        "server_logs" => text(&server_logs(backend, args)?),
        "manage_project" => text(&manage_project(backend, args)?),
        "lane" => text(&lane(backend, args)?),
        "updates" => text(&updates(backend, args)?),
        "app_settings" => text(&app_settings(backend, args)?),
        "get_connected_instances" => text(&connected_instances(backend)?),
        "execute_luau" => text(&execute_luau(backend, args)?),
        "studio_output" => text(&studio_output(backend, args)?),
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
    project_file: String,
    #[serde(default)]
    place: Option<PlaceLink>,
    /// The linked place's experience name, once Rojo Manager looked it up.
    #[serde(default)]
    experience: Option<String>,
    #[serde(default)]
    autostart: bool,
    #[serde(default)]
    auto_connect: bool,
    #[serde(default)]
    sourcemap: bool,
    #[serde(default)]
    lane: Option<Lane>,
    #[serde(default)]
    idle_close: Option<u32>,
    /// Its last output lines.
    #[serde(default)]
    log: Vec<String>,
}

impl Project {
    fn running(&self) -> bool {
        RUNNING.contains(&self.state.as_str())
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
    /// A local place's file, which Studio shows as the place's name.
    #[serde(rename = "placeFile", skip_serializing_if = "Option::is_none")]
    place_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    experience: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    autostart: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    auto_connect: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    sourcemap: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    lane: Option<Lane>,
    /// Minutes of nothing happening in its Studio after which Rojo Manager closes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_close: Option<u32>,
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
            place_id: project.place.as_ref().and_then(PlaceLink::published).map(|place| place.place_id),
            place_file: match &project.place {
                Some(PlaceLink::Local(local)) => Some(local.file.clone()),
                _ => None,
            },
            experience: project.experience,
            autostart: project.autostart,
            auto_connect: project.auto_connect,
            sourcemap: project.sourcemap,
            lane: project.lane,
            idle_close: project.idle_close,
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

/// Start, stop, or restart. With `wait`, Rojo Manager answers once the server answers or is gone,
/// and anything but what the agent asked for is an error with the server's recent output.
fn run_server(backend: &dyn Backend, name: &str, action: &str, wait: bool) -> Result<Project, String> {
    let body = wait.then(|| json!({ "wait": true }));
    let reply = call(backend, "POST", &["projects", name, action], body.as_ref(), WAIT_TIMEOUT)?;
    let project: Project = field(reply, "project")?;
    let expected = match action {
        _ if !wait => true,
        "stop" => project.state == "off",
        _ => project.running(),
    };
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
    /// The bridge's version.
    #[serde(default, skip_serializing)]
    bridge: u64,
    /// While Confirming: what accepting would do (bridge 10 and later).
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "summary")]
    pending: Option<SyncSummary>,
}

/// A pending sync summary, or None when the bridge sent something else.
fn summary<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<SyncSummary>, D::Error> {
    Ok(SyncSummary::deserialize(Value::deserialize(deserializer)?).ok())
}

impl Rojo {
    fn status(&self) -> &str {
        self.status.as_deref().unwrap_or_default()
    }

    /// Rojo connected, asks to confirm the initial sync, or failed.
    fn settled(&self) -> bool {
        matches!(self.status(), "Connected" | "Confirming" | "Error")
    }

    /// What to do next while Rojo waits for the initial sync to be confirmed, warning first about
    /// instances accepting would delete.
    fn next(&self) -> Option<String> {
        if self.status() != "Confirming" {
            return None;
        }
        let ask = "Rojo is waiting for the initial sync to be approved. Use action accept or abort.";
        Some(match self.pending.as_ref().map(|pending| pending.removed.count) {
            Some(removed) if removed > 0 => format!(
                "{ask} Accepting deletes {removed} instance{} found only in Studio (pending.removed); abort \
                 keeps them.",
                if removed == 1 { "" } else { "s" }
            ),
            _ => ask.to_string(),
        })
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
    /// Its window is hidden; open_place with hidden false shows it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
    rojo: Rojo,
}

/// A Studio window whose Rojo Manager bridge reports to the manager.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Instance {
    instance_id: String,
    place_id: u64,
    /// The name of an unpublished place, a local place's file name, from bridges since version 9.
    #[serde(default)]
    place_name: Option<String>,
    /// Its Studio window is hidden.
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    rojo: Rojo,
}

impl Instance {
    fn has(&self, place: &PlaceLink) -> bool {
        place.matches(self.place_id, self.place_name.as_deref())
    }

    /// "place 123", or the name of an unpublished place.
    fn describe(&self) -> String {
        match &self.place_name {
            Some(name) if self.place_id == 0 => name.clone(),
            _ => format!("place {}", self.place_id),
        }
    }
}

/// The Studio windows the bridge reports from, or None when the bridge is not installed.
fn studio_instances(backend: &dyn Backend) -> Result<Option<Vec<Instance>>, String> {
    let reply = call(backend, "GET", &["studio"], None, CALL_TIMEOUT)?;
    let installed = reply.get("bridge") == Some(&Value::Bool(true));
    let instances: Vec<Instance> = field(reply, "instances")?;
    Ok((installed || !instances.is_empty()).then_some(instances))
}

/// The open Studio window with this place, if any: its instanceId, and whether its window is hidden.
fn find_instance(instances: &[Instance], place: &PlaceLink) -> Option<(String, bool)> {
    instances
        .iter()
        .find(|instance| instance.has(place))
        .map(|instance| (instance.instance_id.clone(), instance.hidden))
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

/// Connect Rojo in one Studio window to the server on `port`. With `wait`, the bridge answers once Rojo
/// has settled; bridges before WAIT_BRIDGE answer at once, so they are polled here for 30 seconds.
fn connect(backend: &dyn Backend, instance_id: &str, port: u16, wait: bool) -> Result<Rojo, String> {
    let body = match wait {
        true => json!({ "action": "connect", "port": port, "wait": true }),
        false => json!({ "action": "connect", "port": port }),
    };
    let result = call(backend, "POST", &["studio", instance_id], Some(&body), WAIT_TIMEOUT)?;
    let mut rojo = Rojo::deserialize(&result).map_err(|_| format!("Unexpected bridge response: {result}"))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    while wait && rojo.bridge < WAIT_BRIDGE && !rojo.settled() && Instant::now() < deadline {
        backend.sleep(Duration::from_millis(250));
        rojo = bridge(backend, instance_id, "status", None)?;
    }
    Ok(rojo)
}

/// The Studio instance that has the project's linked place open.
fn place_instance(backend: &dyn Backend, project: &Project) -> Result<String, String> {
    let Some(place) = &project.place else {
        return Err(format!("{} has no linked place. Link one with open_place (project and place).", project.name));
    };
    let instances = studio_instances(backend)?.ok_or(BRIDGE_MISSING)?;
    find_instance(&instances, place).map(|(instance_id, _)| instance_id).ok_or_else(|| {
        let mut message = place.describe();
        message[..1].make_ascii_uppercase();
        format!("{message} is not open in Studio, or its Rojo Manager bridge is not loaded. Use open_place first.")
    })
}

#[derive(Debug, Serialize)]
struct Session {
    #[serde(rename = "instanceId")]
    instance_id: String,
    rojo: Rojo,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

/// rojo_session: connect (starting the server if needed, and with `wait` waiting for the plugin), or
/// pass any other action straight to the bridge.
fn run_session(backend: &dyn Backend, name: &str, action: &str, wait: bool) -> Result<Session, String> {
    let mut project = find_project(backend, name)?;
    let instance_id = place_instance(backend, &project)?;
    if action != "connect" {
        let rojo = bridge(backend, &instance_id, action, None)?;
        return Ok(Session { instance_id, next: rojo.next(), rojo });
    }
    if !project.running() {
        project = run_server(backend, name, "start", true)?;
    }
    let rojo = connect(backend, &instance_id, project.port, wait)?;
    if !wait {
        return Ok(Session { instance_id, rojo, next: None });
    }
    let next = match rojo.status() {
        "Confirming" | "Connected" => rojo.next(),
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

/// rojo_server. Rojo Manager disconnects a connected Studio before stopping its server and reconnects
/// it after a restart; with `wait`, a restart also answers with that Studio's Rojo once it settles.
fn server_tool(backend: &dyn Backend, name: &str, action: &str, wait: bool) -> Result<ProjectResult, String> {
    let instance_id = match action {
        "restart" if wait => connected_instance(backend, &find_project(backend, name)?),
        _ => None,
    };
    let project = run_server(backend, name, action, wait)?.into();
    let rojo = match instance_id {
        Some(instance_id) => {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut rojo = bridge(backend, &instance_id, "status", None)?;
            while !rojo.settled() && Instant::now() < deadline {
                backend.sleep(Duration::from_millis(250));
                rojo = bridge(backend, &instance_id, "status", None)?;
            }
            Some(rojo)
        }
        None => None,
    };
    Ok(ProjectResult { project, rojo })
}

#[derive(Debug, Serialize)]
struct Opened {
    #[serde(rename = "placeId", skip_serializing_if = "Option::is_none")]
    place_id: Option<u64>,
    #[serde(rename = "placeFile", skip_serializing_if = "Option::is_none")]
    place_file: Option<String>,
    #[serde(rename = "instanceId", skip_serializing_if = "Option::is_none")]
    instance_id: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    opened: bool,
    /// The Studio window is hidden.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rojo: Option<Rojo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next: Option<String>,
}

/// How open_place and lane create open Studio.
#[derive(Debug, Clone, Copy, Default)]
struct StudioOptions {
    /// Hide (true) or show (false) the Studio window. None opens visible and leaves an open one as it is.
    hidden: Option<bool>,
    /// The project's idle close in minutes; 0 turns it off.
    idle_close: Option<u64>,
}

impl StudioOptions {
    fn read(args: &Args) -> Result<StudioOptions, String> {
        let hidden = match args.get("hidden") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_bool().ok_or("hidden must be true or false")?),
        };
        let idle_close = match args.get("idle_close") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_u64().ok_or("idle_close must be a number of minutes, 0 for off")?),
        };
        Ok(StudioOptions { hidden, idle_close })
    }
}

/// open_place: reuses a Studio that already has the place open, otherwise opens it and waits up to
/// 3 minutes for its bridge to report in. Rojo Manager opens a local place itself, building it first
/// when its file is missing, and opens, hides, and shows a Studio whose window is to be hidden.
fn open_place(
    backend: &dyn Backend,
    project_name: Option<String>,
    place_text: Option<String>,
    connect_rojo: bool,
    options: StudioOptions,
) -> Result<Opened, String> {
    let mut project = project_name.map(|name| find_project(backend, &name)).transpose()?;
    if project.is_none() && (options.hidden.is_some() || options.idle_close.is_some()) {
        return Err("hidden and idle_close need a project, which Rojo Manager keeps them for.".into());
    }
    if let (Some(found), Some(text)) = (&project, &place_text) {
        let linked = act(backend, &found.name, "link-place", Some(&json!({ "place": text })))?;
        project = Some(linked);
    }
    if let (Some(found), Some(minutes)) = (&project, options.idle_close) {
        project = Some(act(backend, &found.name, "update", Some(&json!({ "idleClose": minutes })))?);
    }
    let place = match (&project, &place_text) {
        (Some(project), _) => project
            .place
            .clone()
            .ok_or_else(|| format!("{} has no linked place. Pass place to link one.", project.name))?,
        (None, Some(text)) if text.eq_ignore_ascii_case("local") => {
            return Err("A local place is built from a project. Pass the project too.".into());
        }
        (None, Some(text)) => PlaceLink::Published(backend.resolve_place(text).map_err(|error| error.to_string())?),
        (None, None) => return Err("Pass a project name or a place ID or link.".into()),
    };
    let (place_id, place_file) = match &place {
        PlaceLink::Published(published) => (Some(published.place_id), None),
        PlaceLink::Local(local) => (None, Some(local.file.clone())),
    };
    if connect_rojo && project.is_none() {
        return Err("connect_rojo needs a project, so Rojo Manager knows which server to use.".into());
    }
    if project.is_none() {
        manager_projects(backend)?; // starts the manager if needed
    }

    let instances = studio_instances(backend)?;
    let mut instance = instances.as_deref().and_then(|instances| find_instance(instances, &place));
    let opened = instance.is_none();
    // Rojo Manager opens the place hidden, or shows or hides the Studio that has it open.
    let name = project.as_ref().map(|project| project.name.clone()).unwrap_or_default();
    let set_hidden = |hidden: bool| {
        call(backend, "POST", &["projects", &name, "open-studio"], Some(&json!({ "hidden": hidden })), BUILD_TIMEOUT)
    };
    match (&instance, options.hidden) {
        (Some((_, shown)), Some(hidden)) if *shown != hidden => {
            set_hidden(hidden)?;
            instance = instance.map(|(instance_id, _)| (instance_id, hidden));
        }
        (None, Some(hidden)) => drop(set_hidden(hidden)?),
        (None, None) => match &place {
            PlaceLink::Published(published) => {
                backend.open_place(published).map_err(|error| format!("Could not open Roblox Studio: {error}"))?
            }
            PlaceLink::Local(_) => {
                drop(call(backend, "POST", &["projects", &name, "open-studio"], None, BUILD_TIMEOUT)?)
            }
        },
        (Some(_), _) => {}
    }
    if opened {
        if instances.is_none() {
            let mut note = "The Rojo Manager bridge is not installed, so the opened place cannot be confirmed.";
            if let Some(project) = project.filter(|_| connect_rojo) {
                run_server(backend, &project.name, "start", true)?;
                note = "The Rojo Manager bridge is not installed, so the opened place cannot be confirmed and \
                        Rojo was not connected. The Rojo server is running; connect it from the Rojo plugin in Studio.";
            }
            return Ok(Opened {
                place_id,
                place_file,
                instance_id: None,
                opened,
                hidden: false,
                note: Some(note),
                rojo: None,
                next: None,
            });
        }
        let deadline = Instant::now() + Duration::from_secs(180);
        while instance.is_none() && Instant::now() < deadline {
            backend.sleep(Duration::from_secs(2));
            let instances = studio_instances(backend)?.unwrap_or_default();
            instance = find_instance(&instances, &place);
        }
        if instance.is_none() {
            let hint = match place {
                PlaceLink::Published(_) => "",
                PlaceLink::Local(_) => {
                    " A local place needs bridge version 9 or newer: check and install it with the updates tool."
                }
            };
            // Whatever keeps a hidden Studio from loading, such as a dialog, should be seen.
            let shown = match options.hidden == Some(true) && set_hidden(false).is_ok() {
                true => " Its window is shown now, in case a dialog there needs an answer.",
                false => "",
            };
            return Err(format!(
                "Studio was asked to open {}, but its Rojo Manager bridge did not report in within 3 minutes. Check \
                 that the Creator Store Rojo is off, so the bridge's Rojo loads.{hint}{shown}",
                place.describe()
            ));
        }
    }
    let (instance_id, hidden) = instance.unzip();
    let hidden = hidden.unwrap_or(false);
    let note = (options.hidden == Some(true) && !hidden)
        .then_some("Studio's window could not be hidden here (hiding is Windows only), so it is open and visible.");
    let mut result = Opened { place_id, place_file, instance_id, opened, hidden, note, rojo: None, next: None };
    if let Some(project) = project.filter(|_| connect_rojo) {
        let session = run_session(backend, &project.name, "connect", true)?;
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
            let place = project.place.clone();
            let mut item = Brief::from(project);
            let open = place.and_then(|place| {
                let instances = instances.as_mut()?;
                let index = instances.iter().position(|instance| instance.has(&place))?;
                Some(instances.swap_remove(index))
            });
            item.studio = open.map(|instance| StudioPlace {
                instance_id: instance.instance_id,
                hidden: instance.hidden,
                rojo: instance.rojo,
            });
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
    #[serde(rename = "placeName", skip_serializing_if = "Option::is_none")]
    place_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    rojo: Rojo,
}

/// get_connected_instances: every Studio window the bridge reports from, with its linked project.
fn connected_instances(backend: &dyn Backend) -> Result<Value, String> {
    let projects = manager_projects(backend)?;
    let instances = studio_instances(backend)?.ok_or(BRIDGE_MISSING)?;
    let linked =
        |instance: &Instance| projects.iter().find(|p| p.place.as_ref().is_some_and(|place| instance.has(place)));
    let instances: Vec<ConnectedInstance> = instances
        .into_iter()
        .map(|instance| ConnectedInstance {
            project: linked(&instance).map(|project| project.name.clone()),
            instance_id: instance.instance_id,
            place_id: instance.place_id,
            place_name: instance.place_name,
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
        None => only_instance(backend)?,
    };
    let body = json!({ "action": "luau", "code": code });
    call(backend, "POST", &["studio", &instance_id], Some(&body), LUAU_CALL_TIMEOUT)
}

/// The one open Studio window with the bridge.
fn only_instance(backend: &dyn Backend) -> Result<String, String> {
    manager_projects(backend)?; // starts the manager if needed
    let instances = studio_instances(backend)?.ok_or(BRIDGE_MISSING)?;
    match instances.as_slice() {
        [only] => Ok(only.instance_id.clone()),
        [] => Err("No Studio window with the Rojo Manager bridge is open.".into()),
        several => {
            let ids: Vec<String> = several.iter().map(|i| format!("{} ({})", i.instance_id, i.describe())).collect();
            Err(format!("Several Studio windows are open; pass instance_id or project: {}", ids.join(", ")))
        }
    }
}

/// studio_output: Output lines from the Studio window `instance_id` names, the one with the
/// project's place open, or the only one open. Rojo Manager checks since, lines, and level.
fn studio_output(backend: &dyn Backend, args: &Args) -> Result<Value, String> {
    let instance_id = match (optional_string(args, "instance_id")?, optional_string(args, "project")?) {
        (Some(instance_id), _) => instance_id,
        (None, Some(project)) => place_instance(backend, &find_project(backend, &project)?)?,
        (None, None) => only_instance(backend)?,
    };
    let mut body = json!({ "action": "output" });
    // Sorted, so the body reads the same whether or not serde_json preserves insertion order.
    for key in ["level", "lines", "since"] {
        if let Some(value) = args.get(key) {
            body[key] = value.clone();
        }
    }
    call(backend, "POST", &["studio", &instance_id], Some(&body), CALL_TIMEOUT)
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
const PROJECT_FIELDS: [(&str, &str); 7] = [
    ("auto_connect", "autoConnect"),
    ("autostart", "autostart"),
    ("idle_close", "idleClose"),
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
    let action = choice(args, "action", &["add", "create", "convert", "update", "remove"])?;
    if matches!(action, "add" | "create" | "convert") {
        let path = string(args, "path")?;
        let (body, timeout) = match action {
            "create" => (json!({ "create": true, "path": path }), Duration::from_secs(60)),
            // syncback reads the whole place
            "convert" => (json!({ "from": string(args, "file")?, "path": path }), Duration::from_secs(300)),
            _ => (json!({ "path": path }), Duration::from_secs(30)),
        };
        manager_projects(backend)?; // starts the manager if needed
        let reply = call(backend, "POST", &["projects"], Some(&body), timeout)?;
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

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum LaneResult {
    Created {
        lane: String,
        /// The project file it serves, the same one as the project it was made from.
        #[serde(rename = "projectFile")]
        project_file: String,
        project: Box<Brief>,
        #[serde(rename = "instanceId", skip_serializing_if = "Option::is_none")]
        instance_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        rojo: Option<Rojo>,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<&'static str>,
    },
    /// The manager's reply, passed on.
    Removed(Value),
}

/// lane: create makes the lane in Rojo Manager, then opens, connects, and accepts it like open_place
/// with connect_rojo; when that fails, the lane is removed again. remove closes it.
fn lane(backend: &dyn Backend, args: &Args) -> Result<LaneResult, String> {
    let action = choice(args, "action", &["create", "remove"])?;
    manager_projects(backend)?; // starts the manager if needed
    if action == "remove" {
        let name = find_project(backend, &string(args, "lane")?)?.name;
        return Ok(LaneResult::Removed(call(backend, "POST", &["lanes", &name, "remove"], None, CALL_TIMEOUT)?));
    }
    let from = find_project(backend, &string(args, "from")?)?.name;
    // Keys in sorted order, as in PROJECT_FIELDS.
    let path = string(args, "path")?;
    let mut body = json!({ "from": from });
    if let Some(holder) = optional_string(args, "holder")? {
        body["holder"] = json!(holder);
    }
    body["path"] = json!(path);
    let options = StudioOptions::read(args)?;
    let name = field::<Project>(call(backend, "POST", &["lanes"], Some(&body), CALL_TIMEOUT)?, "project")?.name;
    let opened = open_place(backend, Some(name.clone()), None, true, options).and_then(|mut opened| {
        if opened.instance_id.is_none() {
            return Err(format!("{BRIDGE_MISSING} A lane needs it to sync its Studio."));
        }
        if let (Some(instance_id), Some("Confirming")) = (&opened.instance_id, opened.rojo.as_ref().map(Rojo::status)) {
            opened.rojo = Some(bridge(backend, instance_id, "accept", None)?);
            opened.next = None;
        }
        Ok(opened)
    });
    match opened {
        Ok(opened) => {
            let project = find_project(backend, &name)?;
            Ok(LaneResult::Created {
                project_file: project.project_file.clone(),
                project: Box::new(project.into()),
                lane: name,
                instance_id: opened.instance_id,
                rojo: opened.rojo,
                note: opened.note,
            })
        }
        Err(error) => match call(backend, "POST", &["lanes", &name, "remove"], None, CALL_TIMEOUT) {
            Ok(_) => Err(format!("{error} The lane was removed again.")),
            Err(cleanup) => Err(format!("{error} Removing lane {name} again failed too: {cleanup}")),
        },
    }
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

/// The wait argument: true unless the agent passed false.
fn wait(args: &Args) -> Result<bool, String> {
    match args.get("wait") {
        None | Some(Value::Null) => Ok(true),
        Some(value) => value.as_bool().ok_or_else(|| "wait must be true or false".to_string()),
    }
}

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
        manage_project(&fake, &args(json!({"action": "create", "path": "C:/Games/Lobby"}))).unwrap();
        let convert = json!({"action": "convert", "path": "C:/Games/Lobby", "file": "C:/Lobby.rbxl"});
        manage_project(&fake, &args(convert)).unwrap();
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
        assert_eq!(error, "Pass at least one of: auto_connect, autostart, idle_close, name, place, port, sourcemap");
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                r#"POST projects {"path":"C:/Games/Lobby"}"#,
                "GET projects",
                r#"POST projects {"create":true,"path":"C:/Games/Lobby"}"#,
                "GET projects",
                r#"POST projects {"from":"C:/Lobby.rbxl","path":"C:/Games/Lobby"}"#,
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
    fn start_has_rojo_manager_wait_and_reports_failures_with_logs() {
        let fake = Fake::default();
        assert_eq!(run_server(&fake, "TestGame", "start", true).unwrap().state, "serving");
        assert_eq!(run_server(&fake, "TestGame", "stop", false).unwrap().state, "serving"); // not waited for
        assert_eq!(fake.calls(), [r#"POST projects/TestGame/start {"wait":true}"#, "POST projects/TestGame/stop"]);

        let fake = Fake { reply: |_, _| json!({"project": project("off", true)}), ..Fake::default() };
        let error = run_server(&fake, "TestGame", "start", true).unwrap_err();
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
        let session = run_session(&fake, "TestGame", "connect", true).unwrap();
        assert_eq!(
            json_text(&session),
            r#"{"instanceId":"7","rojo":{"status":"Confirming"},"next":"Rojo is waiting for the initial sync to be approved. Use action accept or abort."}"#
        );
        // This bridge is older than WAIT_BRIDGE, so it is polled.
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                r#"POST projects/TestGame/start {"wait":true}"#,
                r#"POST studio/7 {"action":"connect","port":34879,"wait":true}"#,
                r#"POST studio/7 {"action":"status"}"#,
                r#"POST studio/7 {"action":"status"}"#,
            ]
        );

        // A newer bridge waits itself.
        let fake = Fake {
            projects: Script::new([json!([project("serving", true)])]),
            instances: studio_open(),
            studio: Script::new([json!({"status": "Connected", "bridge": WAIT_BRIDGE})]),
            ..Fake::default()
        };
        assert_eq!(
            json_text(&run_session(&fake, "TestGame", "connect", true).unwrap()),
            r#"{"instanceId":"7","rojo":{"status":"Connected"}}"#
        );
        assert_eq!(fake.calls(), ["GET projects", r#"POST studio/7 {"action":"connect","port":34879,"wait":true}"#]);
    }

    #[test]
    fn status_shows_a_pending_sync_and_warns_about_studio_only_removals() {
        let pending = json!({"removed": {"count": 2, "paths": ["Workspace.Map", "Workspace.Spawn"]},
                             "added": {"count": 1, "paths": ["ServerScriptService.Main"]},
                             "changed": {"count": 0, "paths": {}}});
        let fake = Fake {
            projects: Script::new([json!([project("serving", true)])]),
            instances: studio_open(),
            studio: Script::new([json!({"status": "Confirming", "pending": pending}), json!({"status": "Confirming"})]),
            ..Fake::default()
        };
        assert_eq!(
            json_text(&run_session(&fake, "TestGame", "status", true).unwrap()),
            r#"{"instanceId":"7","rojo":{"status":"Confirming","pending":{"removed":{"count":2,"paths":["Workspace.Map","Workspace.Spawn"]},"added":{"count":1,"paths":["ServerScriptService.Main"]},"changed":{"count":0,"paths":[]}}},"next":"Rojo is waiting for the initial sync to be approved. Use action accept or abort. Accepting deletes 2 instances found only in Studio (pending.removed); abort keeps them."}"#
        );
        // An older bridge reports Confirming without a summary.
        assert_eq!(
            json_text(&run_session(&fake, "TestGame", "status", true).unwrap()),
            r#"{"instanceId":"7","rojo":{"status":"Confirming"},"next":"Rojo is waiting for the initial sync to be approved. Use action accept or abort."}"#
        );
    }

    #[test]
    fn restart_answers_with_the_reconnected_studio() {
        let fake = Fake {
            projects: Script::new([json!([project("serving", true)])]),
            reply: |_, _| json!({"project": project("off", true)}),
            instances: studio_open(),
            ..Fake::default()
        };
        let stopped = server_tool(&fake, "TestGame", "stop", true).unwrap();
        assert_eq!(json_text(&stopped), r#"{"project":{"name":"TestGame","state":"off","port":34879,"placeId":2}}"#);
        assert_eq!(fake.calls(), [r#"POST projects/TestGame/stop {"wait":true}"#]);

        let fake = Fake {
            projects: Script::new([json!([project("serving", true)])]),
            instances: studio_open(),
            ..Fake::default()
        };
        let restarted = server_tool(&fake, "TestGame", "restart", true).unwrap();
        assert_eq!(
            json_text(&restarted),
            r#"{"project":{"name":"TestGame","state":"serving","port":34879,"placeId":2},"rojo":{"status":"Connected"}}"#
        );
        assert_eq!(
            fake.calls(),
            [
                "GET projects",
                r#"POST studio/7 {"action":"status"}"#,
                r#"POST projects/TestGame/restart {"wait":true}"#,
                r#"POST studio/7 {"action":"status"}"#,
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
            instances: Script::new([
                json!({"bridge": true, "instances": [{"instanceId": "7", "placeId": 2, "rojo": rojo}]}),
            ]),
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
        let link = Some("https://www.roblox.com/games/2".into());
        let opened = open_place(&fake, Some("TestGame".into()), link, false, StudioOptions::default());
        assert_eq!(json_text(&opened.unwrap()), r#"{"placeId":2,"instanceId":"7"}"#);
        assert_eq!(
            fake.calls(),
            ["GET projects", r#"POST projects/TestGame/link-place {"place":"https://www.roblox.com/games/2"}"#]
        );
    }

    #[test]
    fn open_place_opens_studio_when_the_place_is_not_open() {
        let fake = Fake { instances: no_bridge(), ..Fake::default() };
        let opened = open_place(&fake, None, Some("2".into()), false, StudioOptions::default()).unwrap();
        assert_eq!(
            json_text(&opened),
            r#"{"placeId":2,"opened":true,"note":"The Rojo Manager bridge is not installed, so the opened place cannot be confirmed."}"#
        );
        assert_eq!(fake.calls(), ["resolve 2", "GET projects", "open 2"]);
        let error = open_place(&fake, None, Some("2".into()), true, StudioOptions::default()).unwrap_err();
        assert_eq!(error, "connect_rojo needs a project, so Rojo Manager knows which server to use.");
        let hidden = StudioOptions { hidden: Some(true), idle_close: None };
        assert!(open_place(&fake, None, Some("2".into()), false, hidden).unwrap_err().contains("need a project"));

        // connect_rojo without the bridge still starts the server, and says Rojo is not connected.
        let fake = Fake {
            projects: Script::new([json!([project("off", true)]), json!([project("serving", true)])]),
            instances: no_bridge(),
            ..Fake::default()
        };
        let opened = open_place(&fake, Some("TestGame".into()), None, true, StudioOptions::default()).unwrap();
        assert!(opened.opened && opened.note.unwrap().contains("Rojo was not connected"));
        assert!(fake.calls().contains(&r#"POST projects/TestGame/start {"wait":true}"#.to_string()));
    }

    #[test]
    fn sessions_need_a_linked_place_open_with_the_bridge() {
        let fake = Fake { projects: Script::new([json!([project("off", false)])]), ..Fake::default() };
        let error = run_session(&fake, "TestGame", "status", true).unwrap_err();
        assert_eq!(error, "TestGame has no linked place. Link one with open_place (project and place).");

        let fake = Fake { projects: Script::new([json!([project("off", true)])]), ..Fake::default() };
        let error = run_session(&fake, "TestGame", "status", true).unwrap_err();
        assert_eq!(
            error,
            "Place 2 is not open in Studio, or its Rojo Manager bridge is not loaded. Use open_place first."
        );

        let fake =
            Fake { projects: Script::new([json!([project("off", true)])]), instances: no_bridge(), ..Fake::default() };
        assert_eq!(run_session(&fake, "TestGame", "status", true).unwrap_err(), BRIDGE_MISSING);
    }

    #[test]
    fn open_place_reuses_an_open_studio() {
        let fake = Fake {
            projects: Script::new([json!([project("off", true)])]),
            instances: studio_open(),
            ..Fake::default()
        };
        let open = |options| open_place(&fake, Some("TestGame".into()), None, false, options).unwrap();
        assert_eq!(json_text(&open(StudioOptions::default())), r#"{"placeId":2,"instanceId":"7"}"#);
        assert_eq!(fake.calls(), ["GET projects"]);
        // Showing a Studio that is shown asks for nothing; hiding it goes through Rojo Manager, as
        // does the idle close, which is saved on the project first.
        open(StudioOptions { hidden: Some(false), idle_close: None });
        assert_eq!(fake.calls().len(), 2);
        let hidden = open(StudioOptions { hidden: Some(true), idle_close: Some(30) });
        assert_eq!(json_text(&hidden), r#"{"placeId":2,"instanceId":"7","hidden":true}"#);
        assert_eq!(
            fake.calls()[2..],
            [
                "GET projects",
                r#"POST projects/TestGame/update {"idleClose":30}"#,
                r#"POST projects/TestGame/open-studio {"hidden":true}"#
            ]
        );
    }

    #[test]
    fn open_place_has_rojo_manager_build_and_open_a_local_place_then_finds_it_by_name() {
        let mut lane = project("off", false);
        lane["place"] = json!({"file": "TestGame-1a2b3c4d.rbxl"});
        let open = json!({"bridge": true, "instances": [
            {"instanceId": "8", "placeId": 0, "placeName": "Other-00000000.rbxl", "rojo": {}},
            {"instanceId": "9", "placeId": 0, "placeName": "TestGame-1a2b3c4d.rbxl", "rojo": {}}]});
        let fake = Fake {
            projects: Script::new([json!([lane])]),
            instances: Script::new([json!({"bridge": true, "instances": []}), open]),
            ..Fake::default()
        };
        let opened = open_place(&fake, Some("TestGame".into()), None, false, StudioOptions::default()).unwrap();
        assert_eq!(json_text(&opened), r#"{"placeFile":"TestGame-1a2b3c4d.rbxl","instanceId":"9","opened":true}"#);
        assert_eq!(fake.calls(), ["GET projects", "POST projects/TestGame/open-studio"]);
        let error = open_place(&fake, None, Some("local".into()), false, StudioOptions::default()).unwrap_err();
        assert_eq!(error, "A local place is built from a project. Pass the project too.");
    }

    #[test]
    fn lane_create_makes_the_lane_then_opens_connects_and_accepts_it() {
        let mut made = project("off", false);
        made["name"] = json!("TestGame-lane");
        made["place"] = json!({"file": "TestGame-lane-1a2b3c4d.rbxl"});
        made["lane"] = json!({"parent": "TestGame", "holder": "lane B"});
        made["projectFile"] = json!("lobby.project.json");
        let mut serving = made.clone();
        serving["state"] = json!("serving");
        let open = json!({"bridge": true, "instances": [
            {"instanceId": "9", "placeId": 0, "placeName": "TestGame-lane-1a2b3c4d.rbxl", "rojo": {}}]});
        let fake = Fake {
            projects: Script::new([
                json!([project("off", true)]),
                json!([project("off", true)]),
                json!([project("off", true), made.clone()]),
                json!([project("off", true), made]),
                json!([project("off", true), serving]),
            ]),
            reply: |_, parts| match parts {
                ["lanes"] => json!({"project": {"name": "TestGame-lane", "port": 34880, "state": "off"}}),
                _ => json!({"project": {"name": "TestGame-lane", "port": 34880, "state": "serving"}}),
            },
            instances: Script::new([json!({"bridge": true, "instances": []}), open]),
            studio: Script::new([json!({"status": "Confirming"}), json!({"status": "Connected"})]),
            ..Fake::default()
        };
        let create = json!({"action": "create", "from": "testgame", "path": "C:/Lane", "holder": "lane B"});
        let created = lane(&fake, &args(create));
        assert_eq!(
            json_text(&created.unwrap()),
            r#"{"lane":"TestGame-lane","projectFile":"lobby.project.json","project":{"name":"TestGame-lane","state":"serving","port":34879,"placeFile":"TestGame-lane-1a2b3c4d.rbxl","lane":{"parent":"TestGame","holder":"lane B"}},"instanceId":"9","rojo":{"status":"Connected"}}"#
        );
        let calls = fake.calls();
        assert_eq!(calls[2], r#"POST lanes {"from":"TestGame","holder":"lane B","path":"C:/Lane"}"#);
        assert!(calls.contains(&"POST projects/TestGame-lane/open-studio".to_string()));
        assert_eq!(calls[calls.len() - 2], r#"POST studio/9 {"action":"accept"}"#);
        lane(&fake, &args(json!({"action": "remove", "lane": "testgame-lane"}))).unwrap();
        assert_eq!(fake.calls().last().unwrap(), "POST lanes/TestGame-lane/remove");

        // Without the bridge, the lane's Studio could never sync, so the lane is removed again.
        let mut made = project("off", false);
        made["name"] = json!("TestGame-lane");
        made["place"] = json!({"file": "TestGame-lane-1a2b3c4d.rbxl"});
        let fake = Fake {
            projects: Script::new([json!([project("off", true)]), json!([project("off", true)]), json!([made])]),
            reply: |_, _| json!({"project": {"name": "TestGame-lane", "port": 34880, "state": "serving"}}),
            instances: Script::new([json!({"bridge": false, "instances": []})]),
            ..Fake::default()
        };
        let create = json!({"action": "create", "from": "TestGame", "path": "C:/Lane"});
        let error = lane(&fake, &args(create)).unwrap_err();
        assert!(error.contains("not installed") && error.ends_with("The lane was removed again."), "{error}");
        assert!(fake.calls().contains(&"POST lanes/TestGame-lane/remove".to_string()));
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
    fn studio_output_reads_the_window_with_the_projects_place() {
        let reply = json!({"lines": [{"context": "server", "kind": "error", "text": "boom"}], "next": 5});
        let fake = Fake {
            projects: Script::new([json!([project("serving", true)])]),
            instances: studio_open(),
            studio: Script::new([reply.clone()]),
            ..Fake::default()
        };
        let result = studio_output(&fake, &args(json!({"project": "TestGame", "since": 3, "level": "error"}))).unwrap();
        assert_eq!(result, reply);
        assert_eq!(fake.calls(), ["GET projects", r#"POST studio/7 {"action":"output","level":"error","since":3}"#]);
        let unlinked = Fake { projects: Script::new([json!([project("serving", false)])]), ..Fake::default() };
        let error = studio_output(&unlinked, &args(json!({"project": "TestGame"}))).unwrap_err();
        assert!(error.starts_with("TestGame has no linked place"));
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
