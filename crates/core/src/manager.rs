//! The engine: projects, Rojo processes, the port watcher, logs, update checks, and tool jobs.
//! The tray app and the control API both drive it from any thread; it reports changes through an
//! event callback, so it runs the same whether or not a window is loaded.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use crate::backups::{self, Backup};
use crate::config::{self, Config, Lane, LocalPlace, Place, PlaceLink, Settings};
use crate::control::{BackupUpload, BridgePoll, ControlError, ControlHandler, OutputContext, OutputKind, StudioOutput};
use crate::localnet::{self, port_in_use};
use crate::pull::{self, Pulled};
use crate::studio::{self, ResolveError};
use crate::studio_link::Link;
use crate::tooling::{self, Job, RojoState, RojoStatus};
use crate::updates::{self, UpdateState, UpdateStatus};
use crate::{rojo_api, scaffold, startup, studio_process};

pub const LOG_CAP: usize = 2000;
/// New projects take the first free port from here; 34871 is the auto-connect port.
pub const FIRST_PORT: u16 = 34872;
/// A crashed server is restarted at most this many times within CRASH_WINDOW.
const CRASH_RESTARTS: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(60);
/// A bridge polls every few seconds; one silent this long has unloaded with its Studio.
const BRIDGE_TTL: Duration = Duration::from_secs(15);
/// How long a bridge's poll waits for a command before the bridge reports again.
const BRIDGE_HOLD: Duration = Duration::from_secs(5);
/// How long agent Luau may run in Studio.
pub const LUAU_TIMEOUT: Duration = Duration::from_secs(60);
/// Lines of Studio's Output kept per place, and characters kept per line.
const OUTPUT_CAP: usize = 2000;
const OUTPUT_TEXT: usize = 2000;
/// The first bridge version that sends Studio's Output.
const OUTPUT_BRIDGE: u64 = 8;
/// The first bridge version that captures instances for a pull.
const PULL_BRIDGE: u64 = 13;
/// How long a pull waits for Studio to serialize and send its instances.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(120);
/// The longest unpublished place name kept from a bridge; local place files are much shorter.
const MAX_PLACE_NAME: usize = 255;
/// Rojo's port when its port field in Studio is empty.
const ROJO_DEFAULT_PORT: u16 = 34872;
/// Paths kept per kind of change in a pending sync, and characters kept per path.
const PENDING_PATHS: usize = 20;
const PENDING_PATH_TEXT: usize = 200;
/// The window asks for a star on GitHub once the app has been in use this long.
const STAR_AFTER: Duration = Duration::from_secs(3 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    /// No server on the port.
    Off,
    /// A launch or restart is in flight.
    Starting,
    /// Running here, nothing connected.
    Serving,
    /// Running here with a Studio connected.
    Connected,
    /// A Rojo server started outside this app (or kept through a restart of it).
    External,
    /// Another application holds the port.
    Blocked,
}

impl ServerState {
    pub fn running(self) -> bool {
        matches!(self, ServerState::Serving | ServerState::Connected | ServerState::External)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectView {
    pub name: String,
    pub path: String,
    pub project_file: String,
    pub port: u16,
    pub autostart: bool,
    pub auto_connect: bool,
    pub place: Option<PlaceLink>,
    /// The linked place's experience name, once looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experience: Option<String>,
    pub sourcemap: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<Lane>,
    /// Copies of Studio-only instances Rojo removed while syncing this project, newest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backups: Vec<Backup>,
    /// A Studio with this place open waits for Rojo's initial sync to this server to be confirmed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_sync: Option<PendingSync>,
    /// Minutes of nothing happening in the place's Studio after which it is closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_close: Option<u32>,
    /// A Studio with the bridge has this place open.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub studio_open: bool,
    /// That Studio's window is hidden.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub studio_hidden: bool,
    pub state: ServerState,
    /// Installers that apply: "Wally", then "Rokit" or "Aftman".
    pub badges: Vec<String>,
}

/// A project as the tray menu shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayProject {
    pub name: String,
    pub state: ServerState,
    /// It has a linked place to open in Studio.
    pub linked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolJobView {
    pub operation: String,
    pub tool: String,
}

/// Everything a window needs to draw itself. The window process receives it as JSON; the fields
/// that are constants of the build are not sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    #[serde(skip, default = "version")]
    pub version: &'static str,
    pub projects: Vec<ProjectView>,
    pub settings: Settings,
    pub launch_at_startup: bool,
    pub rojo: RojoStatus,
    pub app_update: UpdateStatus,
    pub tool_job: Option<ToolJobView>,
    /// Tool name -> `--version` output (None when missing); empty until first checked.
    pub tool_versions: HashMap<String, Option<String>>,
    #[serde(skip, default = "tools")]
    pub tools: &'static [tooling::Tool],
    pub studio_plugin: tooling::StudioPlugin,
    pub creator_store_rojo: bool,
    /// How many Rojo plugins the bridge last saw running in Studio, if it has reported.
    pub studio_rojo_plugins: Option<u32>,
    /// RojoManagerMCP.exe, for the agent setup commands.
    pub mcp_path: String,
    /// Experiences (universe IDs) the window shows collapsed.
    pub collapsed: Vec<u64>,
    /// Another Studio MCP set up in an agent ("robloxstudio in Codex"), while the Studio tools are on
    /// and the window has not asked whether to keep them.
    pub studio_tools_prompt: Option<String>,
    /// Ask for a star on GitHub; see `star_due`.
    pub star_prompt: bool,
}

fn version() -> &'static str {
    crate::VERSION
}

fn tools() -> &'static [tooling::Tool] {
    &tooling::TOOLS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Something in the snapshot changed.
    Changed,
    /// A short message for the person using the app.
    Notice(String),
    /// A problem worth a Windows notification when the window is closed, such as a crash.
    Alert(String),
    /// A line of a project's Rojo output.
    Log { project: String, seq: u64, line: String },
    /// An app update was installed; the tray app restarts into it.
    Restart,
}

/// Changes to one project, checked as a whole before any is applied. `place: Some(None)` unlinks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Update {
    pub name: Option<String>,
    pub port: Option<u16>,
    pub autostart: Option<bool>,
    pub auto_connect: Option<bool>,
    /// Missing keeps the place; `null` unlinks it.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "present")]
    pub place: Option<Option<PlaceChoice>>,
    pub sourcemap: Option<bool>,
    /// Minutes; 0 turns closing an idle Studio off.
    pub idle_close: Option<u32>,
}

/// Rojo in one Studio waiting for its initial sync to be confirmed, with what that sync would do
/// when the bridge says (bridge 10 and later).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingSync {
    /// The Studio window's instanceId.
    pub instance: String,
    pub summary: Option<SyncSummary>,
}

/// What accepting Rojo's initial sync would do in Studio. Every removal is an instance that exists
/// only in Studio, since the initial sync removes what the project does not have.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSummary {
    pub removed: Changes,
    pub added: Changes,
    pub changed: Changes,
}

/// How many instances one kind of change touches, and the first of their paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub count: u64,
    #[serde(default, deserialize_with = "paths")]
    pub paths: Vec<String>,
}

/// Paths from a bridge, which is untrusted: strings only, PENDING_PATHS of them, each cut to
/// PENDING_PATH_TEXT characters. An empty Luau table may arrive as {}.
fn paths<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
    let paths = items.iter().filter_map(Value::as_str).take(PENDING_PATHS);
    Ok(paths.map(|path| path.chars().take(PENDING_PATH_TEXT).collect()).collect())
}

/// A place to link: a published place, or a local place file, which the engine names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaceChoice {
    Published(Place),
    Local,
}

/// A field that is present, even as `null`, is `Some`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(deserializer: D) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

struct ToolJob {
    view: ToolJobView,
    /// Holds request.json and the installer's result.json; deleted with the job.
    folder: tempfile::TempDir,
    console: Console,
}

/// The terminal running ToolInstaller: its own console process on Windows. On macOS, Terminal runs
/// it through job.command, which records the installer's pid in the job folder.
struct Console {
    child: Child,
    #[cfg(target_os = "macos")]
    started: Instant,
}

impl Console {
    #[cfg(windows)]
    fn open(installer: &Path, request: &Path, app_dir: &Path) -> std::io::Result<Console> {
        let mut command = Command::new(installer);
        command.arg(request).current_dir(app_dir);
        std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0000_0010); // CREATE_NEW_CONSOLE
        Ok(Console { child: command.spawn()? })
    }

    #[cfg(target_os = "macos")]
    fn open(installer: &Path, request: &Path, app_dir: &Path) -> std::io::Result<Console> {
        use std::os::unix::fs::PermissionsExt;
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"));
        let folder = request.parent().unwrap_or(app_dir);
        let script = folder.join("job.command");
        // Terminal does not pass on this app's environment, so an isolated profile is named here.
        let profile = std::env::var_os("ROJO_MANAGER_DATA_DIR")
            .map(|dir| format!("export ROJO_MANAGER_DATA_DIR={}\n", quote(Path::new(&dir))))
            .unwrap_or_default();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n{profile}echo $$ > {}\ncd {} && exec {} {}\n",
                quote(&folder.join("pid")),
                quote(app_dir),
                quote(installer),
                quote(request)
            ),
        )?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
        let child = Command::new("open").arg("-a").arg("Terminal").arg(&script).spawn()?;
        Ok(Console { child, started: Instant::now() })
    }

    /// The installer exited, or its terminal was closed, without a result.
    fn ended(&mut self, folder: &Path) -> bool {
        #[cfg(windows)]
        let _ = folder;
        #[cfg(windows)]
        return self.child.try_wait().ok().flatten().is_some();
        #[cfg(target_os = "macos")]
        {
            let _ = self.child.try_wait(); // `open` itself returns at once
            let pid = std::fs::read_to_string(folder.join("pid")).ok().and_then(|pid| pid.trim().parse::<i32>().ok());
            match pid {
                // SAFETY: signal 0 only checks that the process exists.
                Some(pid) => unsafe { libc::kill(pid, 0) != 0 },
                None => self.started.elapsed() > Duration::from_secs(60),
            }
        }
    }
}

#[derive(Default)]
struct State {
    config: Config,
    procs: HashMap<String, Child>,
    /// `rojo sourcemap --watch` beside a server this app runs, by project.
    sourcemaps: HashMap<String, Child>,
    starting: HashSet<String>,
    restarting: HashSet<String>,
    cancellations: HashMap<String, Arc<AtomicBool>>,
    expected_stop: HashMap<String, bool>,
    /// The watcher's view of each port; Starting is derived, never stored.
    states: HashMap<String, ServerState>,
    logs: HashMap<String, VecDeque<(u64, String)>>,
    /// (project path, manifest mtime) whose pinned Rojo ran fine.
    rojo_checked: HashSet<(String, Option<SystemTime>)>,
    rojo: Option<RojoStatus>,
    rojo_revision: u64,
    app_update: Option<UpdateStatus>,
    app_update_revision: u64,
    tool_job: Option<ToolJob>,
    tool_versions: HashMap<&'static str, Option<String>>,
    /// The last tool job's outcome, as its notice said.
    job_result: Option<String>,
    /// Recent unexpected exits per project, for the crash restart limit.
    crashes: HashMap<String, Vec<Instant>>,
    studio_rojo_plugins: Option<u32>,
    /// Each running Studio bridge's last poll, by the random id it picked when it loaded.
    bridges: HashMap<u64, BridgeReport>,
    /// Experience names by universe ID, looked up once per run.
    experiences: HashMap<u64, String>,
    /// Each project's backups, newest first; read from disk at launch and kept in step.
    backups: HashMap<String, Vec<Backup>>,
    /// Another Studio MCP set up in an agent, found at launch.
    other_studio_mcp: Option<String>,
    /// Studio's recent Output by place (ID, and name when unpublished), from its Edit DataModel and playtests.
    output: HashMap<(u64, Option<String>), VecDeque<OutputLine>>,
    output_seq: u64,
    /// Pulls waiting for their bridge's capture, by the id the capture command named; the value is
    /// the capture (base64 .rbxm) once it arrived.
    captures: HashMap<u64, Option<String>>,
    /// The Studio process each project's place was last opened hidden in, so it can be shown
    /// before its bridge reports.
    opened_hidden: HashMap<String, u32>,
}

/// One line of Studio's Output, numbered across all places so a reader can ask for newer lines.
#[derive(Debug, Clone, Serialize)]
pub struct OutputLine {
    /// Readers get `next` instead.
    #[serde(skip)]
    pub seq: u64,
    pub context: OutputContext,
    pub kind: OutputKind,
    pub text: String,
}

/// What a Studio bridge last said and was told.
struct BridgeReport {
    place_id: u64,
    /// The name of an unpublished place (place ID 0): the file name of a local place.
    place_name: Option<String>,
    /// The server its Rojo is connected to, or 0.
    port: u16,
    /// The Rojo plugin's status, as the bridge describes it.
    rojo: Value,
    /// The auto-connect server offered to it.
    offered: Option<u16>,
    seen: Instant,
    /// Its Studio process, found once through the poll's connection.
    pid: Option<u32>,
    /// When something last happened in its Studio: an edit, a selection, a sync, Output, a command
    /// from an agent, or the user being in its window.
    active: Instant,
    /// Being closed for being idle, or its user was told that its bridge is too old for that.
    closing: bool,
    /// Its Studio's window is hidden, as of its last poll.
    hidden: bool,
}

/// What a bridge's poll says besides Rojo's state.
#[derive(Debug, Default, Clone, Copy)]
pub struct PollExtra {
    /// The poll's (client port, server port).
    pub connection: Option<(u16, u16)>,
    /// Seconds since something last happened in its Studio.
    pub idle: Option<u64>,
}

impl State {
    /// The running server whose project auto-connects to this place: (project name, port).
    fn auto_connect_target(&self, place_id: u64, place_name: Option<&str>) -> Option<(String, u16)> {
        let project = self.config.projects.iter().find(|p| {
            p.auto_connect
                && p.place.as_ref().is_some_and(|place| place.matches(place_id, place_name))
                && self.states.get(&p.name).is_some_and(|s| s.running())
        })?;
        Some((project.name.clone(), project.port))
    }

    /// The Studio waiting for Rojo's initial sync to this project's server to be confirmed.
    fn pending_sync(&self, project: &config::Project) -> Option<PendingSync> {
        let place = project.place.as_ref()?;
        self.bridges.iter().find_map(|(id, report)| {
            let rojo = &report.rojo;
            let port = rojo.get("port").and_then(|port| match port {
                Value::String(text) if text.trim().is_empty() => Some(ROJO_DEFAULT_PORT),
                Value::String(text) => text.trim().parse().ok(),
                other => other.as_u64().and_then(|port| u16::try_from(port).ok()),
            });
            let waiting = rojo.get("status").and_then(Value::as_str) == Some("Confirming")
                && port == Some(project.port)
                && place.matches(report.place_id, report.place_name.as_deref())
                && report.seen.elapsed() < BRIDGE_TTL;
            let summary = || rojo.get("pending").and_then(|pending| SyncSummary::deserialize(pending).ok());
            waiting.then(|| PendingSync { instance: id.to_string(), summary: summary() })
        })
    }

    /// The Studio windows that have this project's place open, by their bridges' recent polls.
    fn place_bridges<'a>(&'a self, project: &'a config::Project) -> impl Iterator<Item = (u64, &'a BridgeReport)> {
        self.bridges.iter().filter_map(move |(id, report)| {
            let open = report.seen.elapsed() < BRIDGE_TTL
                && project.place.as_ref()?.matches(report.place_id, report.place_name.as_deref());
            open.then_some((*id, report))
        })
    }

    fn view_state(&self, name: &str) -> ServerState {
        let state = self.states.get(name).copied().unwrap_or(ServerState::Off);
        // The watcher can lag a launch by a tick, so in-flight work wins.
        if self.starting.contains(name)
            || self.restarting.contains(name)
            || (state == ServerState::Off && self.procs.contains_key(name))
        {
            ServerState::Starting
        } else {
            state
        }
    }

    fn busy(&self, name: &str) -> bool {
        self.starting.contains(name) || self.procs.contains_key(name)
    }

    fn log(&mut self, name: &str, line: String) -> u64 {
        let buffer = self.logs.entry(name.to_string()).or_default();
        let seq = buffer.back().map_or(0, |(seq, _)| seq + 1);
        if buffer.len() == LOG_CAP {
            buffer.pop_front();
        }
        buffer.push_back((seq, line));
        seq
    }

    fn port(&self, name: &str) -> Result<u16, String> {
        self.config.project(name).map(|p| p.port).ok_or_else(|| missing(name))
    }
}

pub struct Engine {
    state: Mutex<State>,
    emit: Box<dyn Fn(Event) + Send + Sync>,
    /// Folder holding ToolInstaller.exe and RojoManagerMCP.exe.
    app_dir: PathBuf,
    closing: AtomicBool,
    wake: (Mutex<bool>, Condvar),
    /// Counts snapshot changes, so `wait_until` can sleep until the next one.
    changes: (Mutex<u64>, Condvar),
    /// Commands on their way to the bridges in Studio.
    link: Link,
}

fn missing(name: &str) -> String {
    format!("No project named {name}")
}

/// Rojo prints colors; logs show plain text.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
        } else if chars.clone().next() == Some('[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
    }
    out
}

fn manifest_key(path: &str) -> (String, Option<SystemTime>) {
    let modified = ["rokit.toml", "aftman.toml"]
        .iter()
        .find_map(|file| std::fs::metadata(Path::new(path).join(file)).ok())
        .and_then(|meta| meta.modified().ok());
    (path.to_string(), modified)
}

fn project_files(folder: &Path) -> Vec<String> {
    let mut files: Vec<String> = std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.path().is_file())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".project.json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

/// Whether the Rojo server on a project's port serves that project, judged by the `name` in its
/// project file (any name when the file cannot be read). None when no Rojo server answers.
fn serves_project(project: &config::Project) -> Option<bool> {
    let info = rojo_api::server_info(project.port, Duration::from_millis(500))?;
    let file = std::fs::read(Path::new(&project.path).join(&project.project_file)).ok();
    let value = file.and_then(|data| serde_json::from_slice::<Value>(&data).ok());
    let name = value.as_ref().and_then(|value| value.get("name")?.as_str());
    Some(name.is_none_or(|name| name == info.project_name))
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |since| since.as_secs())
}

/// Whether to ask for a star: not asked before, projects added, and in use for STAR_AFTER.
fn star_due(config: &Config, now: u64) -> bool {
    !config.star_asked
        && !config.projects.is_empty()
        && config.first_run > 0
        && now.saturating_sub(config.first_run) >= STAR_AFTER.as_secs()
}

/// Hide a Rojo process's console on Windows. On macOS, run it in its own process group: Rokit's
/// shim starts the real rojo as an ordinary child there, and `end_server` ends the whole group.
fn own_group(command: &mut Command) {
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(command, crate::CREATE_NO_WINDOW);
    #[cfg(target_os = "macos")]
    std::os::unix::process::CommandExt::process_group(command, 0);
}

/// End a process started with `own_group`. On macOS its whole group is asked to stop, then killed
/// after a second, so the real rojo behind Rokit's shim never outlives it.
fn end_server(child: &mut Child) {
    #[cfg(target_os = "macos")]
    {
        let group = child.id() as libc::pid_t;
        // SAFETY: plain signals to the process group this app created for the child.
        unsafe { libc::killpg(group, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(1);
        while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // SAFETY: as above; the shim may have exited and left the real rojo running.
        unsafe { libc::killpg(group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The project's local place file, which Rojo Manager made and which goes with the project.
fn local_place(project: &config::Project) -> Option<PathBuf> {
    match &project.place {
        Some(PlaceLink::Local(place)) => Some(place.path()),
        _ => None,
    }
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(b.trim_end_matches(['\\', '/']))
}

impl Engine {
    /// An engine for settings loaded with `config::load`; `app_dir` holds the companion executables.
    pub fn new(config: Config, app_dir: PathBuf, emit: impl Fn(Event) + Send + Sync + 'static) -> Arc<Engine> {
        Arc::new(Engine {
            state: Mutex::new(State { config, ..State::default() }),
            emit: Box::new(emit),
            app_dir,
            closing: AtomicBool::new(false),
            wake: (Mutex::new(false), Condvar::new()),
            changes: (Mutex::new(0), Condvar::new()),
            link: Link::default(),
        })
    }

    /// Start the watcher and update checks, then autostart projects.
    pub fn run(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        std::thread::spawn(move || engine.watch());
        self.record_first_run();
        self.sweep_places();
        {
            let mut state = self.lock();
            let names: Vec<String> = state.config.projects.iter().map(|p| p.name.clone()).collect();
            state.backups = names
                .into_iter()
                .map(|name| {
                    let list = backups::list(&name);
                    (name, list)
                })
                .collect();
        }
        self.check_rojo(false);
        let auto_update = self.settings().auto_update;
        self.check_app_update(false, auto_update);
        let engine = Arc::clone(self);
        std::thread::spawn(move || engine.refresh_bridge());
        self.refresh_tool_versions();
        self.look_up_experiences();
        let engine = Arc::clone(self);
        std::thread::spawn(move || {
            let found = studio::other_studio_mcp();
            engine.lock().other_studio_mcp = found;
            engine.changed();
        });
        let names: Vec<String> =
            self.lock().config.projects.iter().filter(|p| p.autostart).map(|p| p.name.clone()).collect();
        for name in names {
            let _ = self.start(&name);
        }
    }

    /// Stop servers this app started; servers started elsewhere keep running.
    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.wake_watcher();
        let mut state = self.lock();
        for cancellation in state.cancellations.values() {
            cancellation.store(true, Ordering::SeqCst);
        }
        let children: Vec<_> = state.procs.drain().collect();
        let ports: Vec<u16> = children.iter().filter_map(|(name, _)| state.port(name).ok()).collect();
        let sourcemaps: Vec<_> = state.sourcemaps.drain().collect();
        drop(state);
        for (_, mut child) in children.into_iter().chain(sourcemaps) {
            end_server(&mut child);
        }
        // As in stop_server: the real rojo behind Rokit's shim ends a moment later. Waiting lets an
        // update's restart start these servers again on the same ports.
        let deadline = Instant::now() + Duration::from_secs(1);
        while ports.iter().any(|&port| port_in_use(port)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn changed(&self) {
        *self.changes.0.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        self.changes.1.notify_all();
        (self.emit)(Event::Changed);
    }

    /// Block until `done` holds or `timeout` passes; false on timeout. Checks after each change, and
    /// at least every 250 ms, since a server can begin answering without one. Costs nothing while
    /// no one waits.
    fn wait_until(&self, timeout: Duration, done: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = *self.changes.0.lock().unwrap_or_else(|p| p.into_inner());
            if done() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let guard = self.changes.0.lock().unwrap_or_else(|p| p.into_inner());
            let slice = (deadline - now).min(Duration::from_millis(250));
            let _ = self.changes.1.wait_timeout_while(guard, slice, |count| *count == seen);
        }
    }

    /// Wait for a start, restart, or stop to finish: a server that answers, or none.
    fn wait_for_server(&self, name: &str, action: &str) {
        let stop = action == "stop";
        let timeout = Duration::from_secs(if stop { 10 } else { 30 });
        self.wait_until(timeout, || {
            let Some(project) = self.project(name) else { return true };
            match project.state {
                ServerState::Off => true,
                // Right after a stop, the port can read as another app's until Rojo's child exits.
                ServerState::Blocked => !stop,
                ServerState::Starting => false,
                _ if stop => false,
                _ => rojo_api::server_info(project.port, Duration::from_millis(500)).is_some(),
            }
        });
    }

    pub fn notice(&self, message: impl Into<String>) {
        (self.emit)(Event::Notice(message.into()));
    }

    fn alert(&self, message: impl Into<String>) {
        (self.emit)(Event::Alert(message.into()));
    }

    fn log(&self, name: &str, line: &str) {
        let line = strip_ansi(line.trim_end());
        let seq = self.lock().log(name, line.clone());
        (self.emit)(Event::Log { project: name.to_string(), seq, line });
    }

    fn wake_watcher(&self) {
        *self.wake.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.wake.1.notify_all();
    }

    fn sleep_watcher(&self, timeout: Duration) {
        let guard = self.wake.0.lock().unwrap_or_else(|p| p.into_inner());
        let (mut woken, _) =
            self.wake.1.wait_timeout_while(guard, timeout, |woken| !*woken).unwrap_or_else(|p| p.into_inner());
        *woken = false;
    }

    // ------------------------------------------------------------- reading

    fn view(state: &State, name: &str) -> Option<ProjectView> {
        let p = state.config.project(name)?;
        Some(ProjectView {
            name: p.name.clone(),
            path: p.path.clone(),
            project_file: p.project_file.clone(),
            port: p.port,
            autostart: p.autostart,
            auto_connect: p.auto_connect,
            place: p.place.clone(),
            sourcemap: p.sourcemap,
            lane: p.lane.clone(),
            pending_sync: state.pending_sync(p),
            idle_close: p.idle_close,
            studio_open: state.place_bridges(p).next().is_some(),
            studio_hidden: state.place_bridges(p).any(|(_, report)| report.hidden),
            backups: state.backups.get(&p.name).cloned().unwrap_or_default(),
            experience: p
                .place
                .as_ref()
                .and_then(PlaceLink::published)
                .and_then(|place| state.experiences.get(&place.universe_id).cloned()),
            state: state.view_state(&p.name),
            badges: tooling::project_badges(Path::new(&p.path)).into_iter().map(String::from).collect(),
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        let state = self.lock();
        let projects = state.config.projects.iter().filter_map(|p| Self::view(&state, &p.name)).collect();
        let studio_tools_prompt = state
            .other_studio_mcp
            .clone()
            .filter(|_| state.config.settings.studio_tools && !state.config.studio_tools_asked);
        Snapshot {
            version: crate::VERSION,
            projects,
            settings: state.config.settings,
            launch_at_startup: startup::startup_enabled(),
            rojo: state.rojo.clone().unwrap_or(RojoStatus {
                state: RojoState::Checking,
                message: "Checking Rojo...".into(),
                action: None,
            }),
            app_update: state.app_update.clone().unwrap_or(UpdateStatus {
                state: UpdateState::Checking,
                message: "Checking for app updates...".into(),
                url: updates::RELEASES_URL.into(),
            }),
            tool_job: state.tool_job.as_ref().map(|job| job.view.clone()),
            tool_versions: state
                .tool_versions
                .iter()
                .map(|(name, version)| (name.to_string(), version.clone()))
                .collect(),
            tools: &tooling::TOOLS,
            studio_plugin: tooling::studio_plugin(),
            creator_store_rojo: tooling::creator_store_rojo_installed(),
            studio_rojo_plugins: state.studio_rojo_plugins,
            mcp_path: self.app_dir.join(crate::executable("RojoManagerMCP")).display().to_string(),
            collapsed: state.config.collapsed.clone(),
            // One question at a time.
            star_prompt: studio_tools_prompt.is_none() && star_due(&state.config, unix_now()),
            studio_tools_prompt,
        }
    }

    pub fn project(&self, name: &str) -> Option<ProjectView> {
        Self::view(&self.lock(), name)
    }

    /// Each project's server state, and whether it has a place to open; cheap enough for the tray.
    pub fn tray_projects(&self) -> Vec<TrayProject> {
        let state = self.lock();
        let project = |p: &config::Project| TrayProject {
            name: p.name.clone(),
            state: state.view_state(&p.name),
            linked: p.place.is_some(),
        };
        state.config.projects.iter().map(project).collect()
    }

    pub fn logs(&self, name: &str) -> Vec<(u64, String)> {
        self.lock().logs.get(name).map(|buffer| buffer.iter().cloned().collect()).unwrap_or_default()
    }

    // ------------------------------------------------------------- servers

    /// Launch `rojo serve` for a project. Returns once the launch is underway.
    pub fn start(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let mut state = self.lock();
        let project = state.config.project(name).cloned().ok_or_else(|| missing(name))?;
        // A launch that a stop cancelled may still be checking Rojo's version; it goes ahead after all.
        if state.starting.contains(name)
            && let Some(cancellation) = state.cancellations.get(name)
        {
            cancellation.store(false, Ordering::SeqCst);
            return Ok(());
        }
        if self.closing.load(Ordering::SeqCst) || state.busy(name) || port_in_use(project.port) {
            return Ok(());
        }
        let Some(rojo) = tooling::find_tool("Rojo") else {
            state.rojo = Some(RojoStatus {
                state: RojoState::Missing,
                message: "Rojo is not installed.".into(),
                action: Some("Install Rojo".into()),
            });
            drop(state);
            self.changed();
            return Err("Rojo is not installed.".into());
        };
        state.starting.insert(name.to_string());
        let cancelled = Arc::new(AtomicBool::new(false));
        state.cancellations.insert(name.to_string(), Arc::clone(&cancelled));
        state.expected_stop.insert(name.to_string(), false);
        drop(state);
        self.log(name, &format!("$ rojo serve {} --port {}", project.project_file, project.port));
        self.changed();

        let engine = Arc::clone(self);
        let name = name.to_string();
        std::thread::spawn(move || {
            let launched = engine.launch(&name, &project, &rojo, &cancelled);
            // A start that came after launch() saw the cancellation clears it again; launch anew.
            let again = {
                let mut state = engine.lock();
                state.starting.remove(&name);
                matches!(launched, Ok(None))
                    && !cancelled.load(Ordering::SeqCst)
                    && !engine.closing.load(Ordering::SeqCst)
            };
            if again {
                let _ = engine.start(&name);
                return;
            }
            engine.changed();
            engine.wake_watcher();
            match launched {
                Ok(Some(output)) => {
                    engine.start_sourcemap(&name);
                    let mut reader = BufReader::new(output);
                    let mut line = Vec::new();
                    while reader.read_until(b'\n', &mut line).is_ok_and(|read| read > 0) {
                        let text = String::from_utf8_lossy(&line);
                        if !text.trim().is_empty() {
                            engine.log(&name, &text);
                        }
                        line.clear();
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    engine.log(&name, &error);
                    let help = match tooling::project_manager(Path::new(&project.path)) {
                        Some(manager) => format!("Click {manager} to install its pinned tools."),
                        None => "Use Settings > Tools to install Rojo.".into(),
                    };
                    engine.notice(format!("Could not start {name}. {help} See Logs for details."));
                }
            }
        });
        Ok(())
    }

    /// Spawn the server and adopt it unless the launch was cancelled; returns its output pipe.
    fn launch(
        &self,
        name: &str,
        project: &config::Project,
        rojo: &Path,
        cancelled: &AtomicBool,
    ) -> Result<Option<std::io::PipeReader>, String> {
        // A missing pinned version needs an install, not a silent fallback to another version.
        let key = manifest_key(&project.path);
        if !self.lock().rojo_checked.contains(&key) {
            tooling::tool_version("Rojo", Some(Path::new(&project.path)))?;
            self.lock().rojo_checked.insert(key);
        }
        if cancelled.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let (reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
        let mut command = Command::new(rojo);
        command
            .arg("serve")
            .arg(Path::new(&project.path).join(&project.project_file))
            .arg("--port")
            .arg(project.port.to_string())
            .current_dir(&project.path)
            .stdin(std::process::Stdio::null())
            .stdout(writer.try_clone().map_err(|e| e.to_string())?)
            .stderr(writer);
        own_group(&mut command);
        let mut child = command.spawn().map_err(|e| format!("Could not run Rojo: {e}"))?;
        drop(command); // closes our copies of the pipe's write end, so reading ends with the process
        let mut state = self.lock();
        if cancelled.load(Ordering::SeqCst) || self.closing.load(Ordering::SeqCst) {
            drop(state);
            end_server(&mut child);
            return Ok(None);
        }
        state.procs.insert(name.to_string(), child);
        Ok(Some(reader))
    }

    /// Run `rojo sourcemap --watch` beside a server this app runs, when its project asks for it, so
    /// luau-lsp sees an up to date sourcemap.json. Its output goes to the project's log.
    fn start_sourcemap(self: &Arc<Self>, name: &str) {
        let mut state = self.lock();
        let Some(project) = state.config.project(name).filter(|p| p.sourcemap).cloned() else { return };
        if !state.procs.contains_key(name) || state.sourcemaps.contains_key(name) {
            return;
        }
        let Some(rojo) = tooling::find_tool("Rojo") else { return };
        let spawned = std::io::pipe().and_then(|(reader, writer)| {
            let mut command = Command::new(rojo);
            command
                .arg("sourcemap")
                .arg(Path::new(&project.path).join(&project.project_file))
                .args(["--output", "sourcemap.json", "--include-non-scripts", "--watch"])
                .current_dir(&project.path)
                .stdin(std::process::Stdio::null())
                .stdout(writer.try_clone()?)
                .stderr(writer);
            own_group(&mut command);
            Ok((command.spawn()?, reader))
        });
        let (child, output) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                drop(state);
                return self.log(name, &format!("[sourcemap could not start: {error}]"));
            }
        };
        let pid = child.id();
        state.sourcemaps.insert(name.to_string(), child);
        drop(state);
        self.log(
            name,
            &format!("$ rojo sourcemap {} --output sourcemap.json --include-non-scripts --watch", project.project_file),
        );
        let engine = Arc::clone(self);
        let name = name.to_string();
        std::thread::spawn(move || {
            // Rojo reports every rebuild; a repeat would push the server's own output out of view.
            let mut last = String::new();
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if !line.trim().is_empty() && line != last {
                    engine.log(&name, &format!("[sourcemap] {line}"));
                    last = line;
                }
            }
            // It ended on its own; stop_sourcemap forgets the process before ending it.
            let exited = {
                let mut state = engine.lock();
                let ours = state.sourcemaps.get(&name).is_some_and(|child| child.id() == pid);
                if ours { state.sourcemaps.remove(&name) } else { None }
            };
            if let Some(mut child) = exited {
                let code = child.wait().ok().and_then(|status| status.code());
                let shown = code.map_or("unknown".to_string(), |c| c.to_string());
                engine.log(&name, &format!("[sourcemap exited with code {shown}]"));
            }
        });
    }

    fn stop_sourcemap(&self, name: &str) {
        let child = self.lock().sourcemaps.remove(name);
        if let Some(mut child) = child {
            end_server(&mut child);
        }
    }

    pub fn stop(&self, name: &str) {
        self.lock().restarting.remove(name);
        self.disconnect_studio(name);
        self.stop_server(name);
        self.wake_watcher();
        self.changed();
    }

    /// Disconnect Rojo in the Studio connected to the project's server, so stopping the server does
    /// not leave Rojo there in Error. Answers that Studio, for a restart to reconnect.
    fn disconnect_studio(&self, name: &str) -> Option<u64> {
        let state = self.lock();
        let port = state.port(name).ok()?;
        let live = state.bridges.iter().filter(|(id, report)| self.bridge_live(**id, report));
        let studio = live.clone().find(|(_, report)| report.port == port).map(|(id, _)| *id)?;
        drop(state);
        self.link.run(studio, "disconnect", json!({}), Duration::from_secs(5)).ok().map(|_| studio)
    }

    fn stop_server(&self, name: &str) {
        self.stop_sourcemap(name);
        let (child, project) = {
            let mut state = self.lock();
            if let Some(cancellation) = state.cancellations.get(name) {
                cancellation.store(true, Ordering::SeqCst);
            }
            let Some(project) = state.config.project(name).cloned() else { return };
            let child = state.procs.remove(name);
            if child.is_some() {
                state.expected_stop.insert(name.to_string(), true);
            }
            (child, project)
        };
        let port = project.port;
        if let Some(mut child) = child {
            end_server(&mut child);
            // Rokit's shim exits first; the real rojo holding the port ends a moment later. Until
            // it does, the watcher would take our own dying server for another app's.
            let deadline = Instant::now() + Duration::from_secs(1);
            while port_in_use(port) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            return;
        }
        if !port_in_use(port) {
            return;
        }
        match serves_project(&project) {
            None => {
                return self.notice(format!(
                    "Port {port} is used by another application. Change the port or close that application."
                ));
            }
            Some(false) => {
                return self.notice(format!(
                    "Port {port} is used by a Rojo server for another project. Stop it where it was started."
                ));
            }
            Some(true) => {}
        }
        match localnet::port_owner(port) {
            None => {}
            Some((_, image)) if !image.eq_ignore_ascii_case(&crate::executable("rojo")) => {
                self.notice("The listener is not a Rojo process. Stop it from the application that started it.")
            }
            Some((pid, _)) => {
                if let Err(error) = localnet::kill_process(pid) {
                    self.notice(format!("Could not stop the existing Rojo server: {error}"));
                }
            }
        }
    }

    /// Stop, then start again once the port is free (up to 5 seconds).
    pub fn restart(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let port = self.lock().port(name)?;
        // A Studio that was connected is reconnected once the server is back.
        let studio = self.disconnect_studio(name);
        self.stop(name);
        self.lock().restarting.insert(name.to_string());
        self.changed();
        let engine = Arc::clone(self);
        let name = name.to_string();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if !engine.lock().restarting.contains(&name) || engine.closing.load(Ordering::SeqCst) {
                    return; // cancelled by a stop, or closing
                }
                if !port_in_use(port) {
                    engine.lock().restarting.remove(&name);
                    if engine.start(&name).is_ok()
                        && let Some(studio) = studio
                    {
                        engine.wait_for_server(&name, "start");
                        let args = json!({ "port": port, "wait": true });
                        let _ = engine.link.run(studio, "connect", args, Duration::from_secs(40));
                    }
                    return;
                }
                if Instant::now() >= deadline {
                    engine.lock().restarting.remove(&name);
                    engine.changed();
                    engine.notice(format!("{name}: port {port} is still in use, so the server was not restarted"));
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Ok(())
    }

    /// The row's main button: stop anything running or starting, otherwise start.
    pub fn toggle(self: &Arc<Self>, name: &str) -> Result<(), String> {
        let (busy, port) = {
            let state = self.lock();
            (state.busy(name) || state.restarting.contains(name), state.port(name)?)
        };
        if busy || port_in_use(port) {
            self.stop(name);
            Ok(())
        } else {
            self.start(name)
        }
    }

    pub fn start_all(self: &Arc<Self>) {
        let names: Vec<(String, u16)> = self.lock().config.projects.iter().map(|p| (p.name.clone(), p.port)).collect();
        let started = names.iter().filter(|(name, port)| !port_in_use(*port) && self.start(name).is_ok()).count();
        self.notice(format!("Starting {started} server{}", if started == 1 { "" } else { "s" }));
    }

    pub fn stop_all(&self) {
        let names: Vec<(String, u16, bool)> = {
            let state = self.lock();
            state.config.projects.iter().map(|p| (p.name.clone(), p.port, state.busy(&p.name))).collect()
        };
        for (name, port, busy) in names {
            if busy || port_in_use(port) {
                self.stop(&name);
            }
        }
        self.notice("Stopping all servers");
    }

    /// What holds a blocked project's port, for the Kill confirmation.
    pub fn port_owner(&self, name: &str) -> Result<Option<(u32, String)>, String> {
        let port = self.lock().port(name)?;
        Ok(localnet::port_owner(port))
    }

    /// Force-end the process holding a project's port, if it is still `pid`.
    pub fn kill_port_owner(&self, name: &str, pid: u32) -> Result<(), String> {
        let port = self.lock().port(name)?;
        let Some((owner, image)) = localnet::port_owner(port) else {
            self.wake_watcher();
            return Err(format!("Port {port} is free now."));
        };
        if owner != pid {
            return Err(format!("Port {port} changed hands. Check it again."));
        }
        if matches!(pid, 0 | 4) {
            return Err(format!("Port {port} is held by Windows. Change this project's port instead."));
        }
        localnet::kill_process(pid).map_err(|error| format!("Could not end {image}: {error}"))?;
        self.wake_watcher();
        self.notice(format!("Ended {image} (PID {pid}). Port {port} is free."));
        Ok(())
    }

    /// A bridge's poll: the server its Rojo is connected to (0 for none), so only real sessions
    /// show as Connected. Answers with the place's auto-connect server and whether to accept Rojo's
    /// sync prompt; the bridge connects when that offer first appears, so turning auto-connect on or
    /// starting the server connects an open place.
    pub fn report_bridge(
        self: &Arc<Self>,
        id: u64,
        place_id: u64,
        place_name: Option<String>,
        port: u16,
        rojo: Value,
        extra: PollExtra,
    ) -> Option<(u16, bool)> {
        // The Studio process is looked up once per bridge, from the connection its poll came in on.
        let known = self.lock().bridges.get(&id).and_then(|report| report.pid);
        let pid = known.or_else(|| {
            let (client, server) = extra.connection?;
            localnet::connection_owner(client, server).filter(|pid| studio_process::is_studio(*pid))
        });
        let hidden = pid.and_then(studio_process::hidden).unwrap_or(false);
        let foreground = pid.is_some_and(studio_process::foreground);
        let now = Instant::now();
        let (target, previous, accept, idle, outdated) = {
            let mut state = self.lock();
            let place_name = place_name.filter(|name| place_id == 0 && name.len() <= MAX_PLACE_NAME);
            let target = state.auto_connect_target(place_id, place_name.as_deref());
            let offered = target.as_ref().map(|(_, port)| *port);
            let before = state.bridges.get(&id);
            let reported = extra.idle.and_then(|idle| now.checked_sub(Duration::from_secs(idle)));
            let active = match (foreground, before.map(|report| report.active), reported) {
                (true, _, _) => now,
                (false, Some(before), Some(reported)) => before.max(reported),
                (false, before, reported) => before.or(reported).unwrap_or(now),
            };
            let closing = before.is_some_and(|report| report.closing);
            let project = state
                .config
                .projects
                .iter()
                .find(|p| p.place.as_ref().is_some_and(|place| place.matches(place_id, place_name.as_deref())))
                .map(|p| (p.name.clone(), p.idle_close));
            // The bridge names the process from here on, so the one remembered from opening the
            // place hidden is dropped before that number can come to mean another Studio.
            if let (Some(_), Some((name, _))) = (pid, &project) {
                state.opened_hidden.remove(name);
            }
            // A Studio running a command is in use, however long ago its last one came.
            let idle = project.and_then(|(name, minutes)| Some((name, minutes?))).filter(|(_, minutes)| {
                let limit = Duration::from_secs(u64::from(*minutes) * 60);
                !closing && active.elapsed() >= limit && !self.link.busy(id)
            });
            // Only a bridge that says how long its Studio has been idle (13 and later) has it
            // closed: an older one's edits and syncs go unseen here, so its user is told instead.
            let (idle, outdated) = if extra.idle.is_some() { (idle, None) } else { (None, idle) };
            let report = BridgeReport {
                place_id,
                place_name,
                port,
                rojo,
                offered,
                seen: now,
                pid,
                active,
                closing: closing || idle.is_some() || outdated.is_some(),
                hidden,
            };
            (target, state.bridges.insert(id, report), state.config.settings.auto_connect_accept, idle, outdated)
        };
        if let Some((name, _)) = outdated {
            self.alert(format!(
                "{name}: its Studio looks idle but stays open, because its bridge is too old to tell. Restart that \
                 Studio to load the new bridge."
            ));
        }
        if previous.as_ref().map(|report| report.port) != Some(port) {
            self.wake_watcher();
        }
        if previous.as_ref().map(|report| report.hidden) != Some(hidden) {
            self.changed();
        }
        if let Some((name, minutes)) = idle {
            let engine = Arc::clone(self);
            std::thread::spawn(move || engine.close_idle(&name, id, minutes));
            return None; // no auto-connect for a Studio that is closing
        }
        let offered_before = previous.as_ref().and_then(|report| report.offered);
        // The window shows a sync waiting for confirmation, and what it would do.
        let waiting = |rojo: &Value| {
            (rojo.get("status").and_then(Value::as_str) == Some("Confirming")).then(|| rojo.get("pending").cloned())
        };
        let now = self.lock().bridges.get(&id).map(|report| waiting(&report.rojo));
        if previous.map(|report| waiting(&report.rojo)) != now {
            self.changed();
        }
        let (name, offer) = target?;
        if port == 0 && offered_before != Some(offer) {
            self.notice(format!("{name}: connecting Rojo in Studio"));
        }
        Some((offer, accept))
    }

    /// An unexpected exit. When restarting is on, the server starts again, up to CRASH_RESTARTS
    /// times a minute, so a project that fails at once does not loop.
    fn crashed(self: &Arc<Self>, name: &str, code: &str) {
        let restart = {
            let mut state = self.lock();
            state.config.settings.restart_crashed.then(|| {
                let recent = state.crashes.entry(name.to_string()).or_default();
                recent.retain(|at| at.elapsed() < CRASH_WINDOW);
                recent.push(Instant::now());
                recent.len() <= CRASH_RESTARTS
            })
        };
        match restart {
            Some(true) => {
                self.log(name, "[restarting after the crash]");
                self.notice(format!("{name} crashed and was restarted"));
                let _ = self.restart(name);
            }
            Some(false) => self.alert(format!("{name} keeps crashing, so it was not restarted. Check Logs.")),
            None => self.alert(format!("{name} stopped unexpectedly (code {code}). Check Logs.")),
        }
    }

    /// A bridge polled recently, or is running a command it was handed.
    fn bridge_live(&self, id: u64, report: &BridgeReport) -> bool {
        report.seen.elapsed() < BRIDGE_TTL || self.link.busy(id)
    }

    /// The Studio windows whose bridge polled recently: {instanceId, placeId, placeName, rojo, hidden},
    /// with placeName only for an unpublished place and hidden only when its window is.
    pub fn studio_instances(&self) -> Vec<Value> {
        let state = self.lock();
        let live = state.bridges.iter().filter(|(id, report)| self.bridge_live(**id, report));
        let mut instances: Vec<(u64, &BridgeReport)> = live.map(|(id, report)| (*id, report)).collect();
        instances.sort_by_key(|(id, _)| *id);
        let instance = |(id, report): (u64, &BridgeReport)| {
            let mut instance = json!({ "instanceId": id.to_string(), "placeId": report.place_id, "rojo": report.rojo });
            if let Some(name) = &report.place_name {
                instance["placeName"] = json!(name);
            }
            if report.hidden {
                instance["hidden"] = json!(true);
            }
            instance
        };
        instances.into_iter().map(instance).collect()
    }

    /// Run a command in one Studio window through its bridge and wait for the result.
    pub fn studio_command(&self, instance: &str, action: &str, args: Value) -> Result<Value, ControlError> {
        let id = instance
            .parse::<u64>()
            .ok()
            .filter(|id| self.lock().bridges.get(id).is_some_and(|report| self.bridge_live(*id, report)));
        let id = id.ok_or_else(|| ControlError::new(format!("No Studio window has instanceId {instance}"), 404))?;
        let timeout = match action {
            "luau" => LUAU_TIMEOUT,
            // The bridge waits up to 30 seconds for Rojo to connect.
            "connect" if args.get("wait") == Some(&Value::Bool(true)) => Duration::from_secs(40),
            _ => Duration::from_secs(15),
        };
        self.touch(id);
        self.link.run(id, action, args, timeout).map_err(|error| ControlError::new(error, 409))
    }

    /// Something was just done with this Studio, so it is not idle.
    fn touch(&self, id: u64) {
        if let Some(report) = self.lock().bridges.get_mut(&id) {
            report.active = Instant::now();
        }
    }

    /// The Studio processes that have this project's place open: those whose bridge reports it, the
    /// one it was opened hidden in, and on Windows those whose window title starts with a local
    /// place's path, which covers a Studio whose bridge has not loaded.
    fn place_processes(&self, project: &config::Project) -> Vec<u32> {
        let mut pids: Vec<u32> = {
            let state = self.lock();
            let reported = state.place_bridges(project).filter_map(|(_, report)| report.pid);
            reported.chain(state.opened_hidden.get(&project.name).copied()).collect()
        };
        if let Some(path) = local_place(project) {
            pids.extend(localnet::window_owners(&format!("{} - ", path.display())));
        }
        pids.sort_unstable();
        pids.dedup();
        pids.retain(|pid| studio_process::is_studio(*pid));
        pids
    }

    /// End the Studio windows that have the project's place open. Returns how many closed; one
    /// that would not close is an error. Unsaved changes there are lost.
    fn close_place(&self, project: &config::Project) -> Result<usize, String> {
        let pids = self.place_processes(project);
        for pid in &pids {
            studio_process::close(*pid).map_err(|e| format!("Could not close Studio (process {pid}): {e}"))?;
        }
        let mut state = self.lock();
        state.opened_hidden.remove(&project.name);
        state.bridges.retain(|_, report| report.pid.is_none_or(|pid| !pids.contains(&pid)));
        Ok(pids.len())
    }

    /// Close the Studio of the idle bridge `id` after `minutes` with nothing happening in it, and
    /// stop its project's server unless it autostarts or another Studio is connected to it. The
    /// project, its place, and other Studios with the place open stay.
    fn close_idle(self: &Arc<Self>, name: &str, id: u64, minutes: u32) {
        let (project, pid) = {
            let state = self.lock();
            (state.config.project(name).cloned(), state.bridges.get(&id).and_then(|report| report.pid))
        };
        let Some(project) = project else { return };
        let Some(pid) = pid else {
            // Said once: the bridge stays marked as closing.
            return self.alert(format!("{name}: its Studio is idle, but its process was not found to close it."));
        };
        // Rojo is disconnected first, so the server it leaves does not see a dropped connection.
        let _ = self.link.run(id, "disconnect", json!({}), Duration::from_secs(5));
        match studio_process::close(pid).map_err(|e| format!("Could not close Studio (process {pid}): {e}")) {
            Ok(()) => {
                let connected = {
                    let mut state = self.lock();
                    state.bridges.remove(&id);
                    state.bridges.values().any(|report| report.port == project.port)
                };
                if !project.autostart && !connected {
                    self.stop(name);
                }
                self.notice(format!("{name}: closed its Studio after {minutes} min idle"));
            }
            Err(error) => {
                if let Some(report) = self.lock().bridges.get_mut(&id) {
                    report.closing = false;
                    report.active = Instant::now(); // try again after another idle period
                }
                self.alert(format!("{name}: {error}"));
            }
        }
        self.changed();
    }

    /// Store a backup a bridge sent, under the project whose server's patch removed the instances.
    /// A failed copy is stored too, so the history shows it, and announced.
    pub fn save_backup(&self, upload: BackupUpload) {
        let project = self.lock().config.projects.iter().find(|p| p.port == upload.port).map(|p| p.name.clone());
        let Some(project) = project else { return };
        let data = match (upload.data, upload.error) {
            (Some(data), _) => BASE64.decode(data.as_bytes()).map_err(|_| "The bridge sent a damaged copy".to_string()),
            (None, error) => Err(error.unwrap_or_else(|| "The bridge sent no copy".into())),
        };
        match backups::save(&project, upload.paths, upload.skipped, data) {
            Ok(backup) => {
                if let Some(error) = &backup.error {
                    self.alert(format!("{project}: Rojo removed instances that existed only in Studio, and backing them up failed: {error}"));
                }
                let mut state = self.lock();
                let list = state.backups.entry(project).or_default();
                list.insert(0, backup);
                list.truncate(backups::KEEP);
            }
            Err(error) => self.alert(format!("{project}: could not save a backup: {error}")),
        }
        self.changed();
    }

    /// The live Studio window a project syncs: connected to its server, or else with its place open.
    fn project_instance(&self, project: &config::Project) -> Option<u64> {
        let state = self.lock();
        let live = || state.bridges.iter().filter(|(id, report)| self.bridge_live(**id, report));
        let connected = live().find(|(_, report)| report.port == project.port).map(|(id, _)| *id);
        connected.or_else(|| {
            let place = project.place.as_ref()?;
            live().find(|(_, report)| place.matches(report.place_id, report.place_name.as_deref())).map(|(id, _)| *id)
        })
    }

    /// Put a backup's instances back under their old parents in the project's open Studio: the
    /// newest restorable backup without an `id`. Answers {"backup", "restored", "missing"}.
    pub fn restore_backup(&self, name: &str, id: Option<&str>) -> Result<Value, String> {
        let project = self.lock().config.project(name).cloned().ok_or_else(|| missing(name))?;
        let (backup, data) = backups::read(&project.name, id)?;
        let instance = self.project_instance(&project).ok_or_else(|| {
            format!("{name} is not open in Studio. Open its place, then restore; backups go back into the open place.")
        })?;
        let args = json!({ "data": BASE64.encode(data) });
        let result = self.link.run(instance, "restore", args, Duration::from_secs(60))?;
        // An empty Luau table arrives as {}.
        let list = |key: &str| result.get(key).filter(|value| value.is_array()).cloned().unwrap_or_else(|| json!([]));
        Ok(json!({ "backup": backup.id, "restored": list("restored"), "missing": list("missing") }))
    }

    /// Accept or abort the initial sync Rojo waits on in one Studio window.
    pub fn answer_sync(&self, instance: &str, accept: bool) -> Result<(), String> {
        let action = if accept { "accept" } else { "abort" };
        self.studio_command(instance, action, json!({})).map(|_| ()).map_err(|error| error.message)
    }

    /// Keep new lines of a place's Output; the oldest go past OUTPUT_CAP.
    pub fn report_output(&self, output: StudioOutput) {
        let mut guard = self.lock();
        let state = &mut *guard;
        let name = output.place_name.filter(|name| output.place_id == 0 && name.len() <= MAX_PLACE_NAME);
        // A Studio that prints is in use, such as one running a playtest.
        let now = Instant::now();
        let place = state.bridges.values_mut().filter(|r| r.place_id == output.place_id && r.place_name == name);
        place.for_each(|report| report.active = now);
        let buffer = state.output.entry((output.place_id, name)).or_default();
        for line in output.lines {
            let mut text = line.text;
            if let Some((end, _)) = text.char_indices().nth(OUTPUT_TEXT) {
                text.truncate(end);
            }
            state.output_seq += 1;
            if buffer.len() == OUTPUT_CAP {
                buffer.pop_front();
            }
            buffer.push_back(OutputLine { seq: state.output_seq, context: output.context, kind: line.kind, text });
        }
    }

    /// The Output of the place open in one Studio window, oldest first: the newest `count` lines after
    /// `since` that are at least as severe as `level`, and `next`, the `since` for the next read.
    pub fn studio_output(
        &self,
        instance: &str,
        since: u64,
        level: OutputKind,
        count: usize,
    ) -> Result<Value, ControlError> {
        let state = self.lock();
        let id = instance.parse::<u64>().ok();
        let report = id.and_then(|id| state.bridges.get(&id).filter(|report| self.bridge_live(id, report)));
        let report =
            report.ok_or_else(|| ControlError::new(format!("No Studio window has instanceId {instance}"), 404))?;
        let buffer = state.output.get(&(report.place_id, report.place_name.clone()));
        let bridge = report.rojo.get("bridge").and_then(Value::as_u64).unwrap_or(0);
        if buffer.is_none() && bridge < OUTPUT_BRIDGE {
            return Err(ControlError::new(
                "This Studio's bridge is too old to send its Output. Update the bridge in Rojo Manager, then restart \
                Studio.",
                409,
            ));
        }
        let lines: Vec<&OutputLine> =
            buffer.into_iter().flatten().filter(|line| line.seq > since && line.kind >= level).collect();
        let next = buffer.and_then(VecDeque::back).map_or(since, |line| line.seq.max(since));
        Ok(json!({ "lines": &lines[lines.len().saturating_sub(count)..], "next": next }))
    }

    /// The bridge's count of Rojo plugins running in a Studio that just opened a place.
    pub fn report_studio_rojo_plugins(&self, count: u32) {
        let changed = self.lock().studio_rojo_plugins.replace(count) != Some(count);
        if changed {
            self.changed();
        }
    }

    // ------------------------------------------------------------- watcher

    /// Poll the TCP table: every 0.2 s while a server starts, otherwise every second. A listener
    /// this app did not start is identified over HTTP once per owning process.
    fn watch(self: Arc<Self>) {
        // port -> (listener pid, checked at, is Rojo)
        let mut probes: HashMap<u16, (u32, Instant, bool)> = HashMap::new();
        while !self.closing.load(Ordering::SeqCst) {
            self.end_orphaned_lanes();
            let (connections, idle_ports) = {
                let state = self.lock();
                let idle = state.procs.is_empty() && state.starting.is_empty() && state.restarting.is_empty();
                let ports: Vec<u16> = state.config.projects.iter().map(|p| p.port).collect();
                (!state.procs.is_empty(), idle.then_some(ports))
            };
            // macOS reads every process's sockets for a snapshot; while this app runs no server, a
            // connection attempt per project port shows whether there is anything to look at.
            let quiet =
                cfg!(target_os = "macos") && idle_ports.is_some_and(|ports| !ports.into_iter().any(localnet::answers));
            let snapshot = if quiet { Ok(Default::default()) } else { localnet::tcp_snapshot(connections) };
            let Ok((listening, established)) = snapshot else {
                self.sleep_watcher(Duration::from_secs(1));
                continue;
            };
            let mut changed = false;
            let (projects, mut launching, bridged) = {
                let mut state = self.lock();
                let bridges = state.bridges.len();
                state.bridges.retain(|id, report| self.bridge_live(*id, report));
                // A Studio that closed no longer shows as open on its project.
                changed |= state.bridges.len() != bridges;
                // The ports live bridges report as connected; None when no bridge runs.
                let bridged: Option<HashSet<u16>> = (!state.bridges.is_empty())
                    .then(|| state.bridges.values().map(|report| report.port).filter(|&port| port != 0).collect());
                (state.config.projects.clone(), !state.starting.is_empty() || !state.restarting.is_empty(), bridged)
            };
            for project in projects {
                let (name, port) = (project.name.clone(), project.port);
                let exited = {
                    let mut state = self.lock();
                    let exit = state.procs.get_mut(&name).and_then(|child| child.try_wait().ok().flatten());
                    if exit.is_some() {
                        state.procs.remove(&name);
                    }
                    exit.map(|status| (status, state.expected_stop.remove(&name).unwrap_or(false)))
                };
                if let Some((status, expected)) = exited {
                    self.stop_sourcemap(&name);
                    // A signal (macOS) has no exit code; its status reads "signal: 11 (SIGSEGV)".
                    let shown = status.code().map_or_else(|| status.to_string(), |c| c.to_string());
                    self.log(&name, &format!("[process exited with code {shown}]"));
                    changed = true;
                    if !expected && !status.success() {
                        self.crashed(&name, &shown);
                    }
                }
                let owned = self.lock().procs.contains_key(&name);
                let state = match listening.get(&port) {
                    Some(_) if owned => {
                        // Studio keeps idle connections open, such as the one behind Rojo's "is serving"
                        // prompt, so a live bridge decides; without one, any connection counts.
                        let open = established.get(&port).copied().unwrap_or(0) > 0;
                        if open && bridged.as_ref().is_none_or(|ports| ports.contains(&port)) {
                            ServerState::Connected
                        } else {
                            ServerState::Serving
                        }
                    }
                    Some(&pid) => {
                        let probe = probes.get(&port).copied();
                        // A non-Rojo listener is re-checked now and then, in case Rojo was still binding.
                        let is_rojo = match probe {
                            Some((checked, at, is_rojo))
                                if checked == pid && (is_rojo || at.elapsed() < Duration::from_secs(5)) =>
                            {
                                is_rojo
                            }
                            _ => {
                                // Rojo serving another project here counts as another app.
                                let is_rojo = serves_project(&project) == Some(true);
                                probes.insert(port, (pid, Instant::now(), is_rojo));
                                is_rojo
                            }
                        };
                        if is_rojo { ServerState::External } else { ServerState::Blocked }
                    }
                    None => {
                        probes.remove(&port);
                        launching |= owned;
                        ServerState::Off
                    }
                };
                let previous = self.lock().states.insert(name.clone(), state);
                if previous != Some(state) {
                    changed = true;
                    if state == ServerState::Blocked {
                        self.alert(format!("{name}: port {port} is used by another application."));
                    }
                }
            }
            if changed {
                self.changed();
            }
            self.sleep_watcher(if launching { Duration::from_millis(200) } else { Duration::from_secs(1) });
        }
    }

    // ------------------------------------------------------------- projects

    /// Add the Rojo project at `path`, or each new project in its immediate subfolders. Returns the
    /// added names; errors when nothing could be added.
    pub fn add_projects(&self, path: &Path) -> Result<Vec<String>, String> {
        let added = if !project_files(path).is_empty() {
            vec![self.register(path, None, None, false)?]
        } else {
            let entries = std::fs::read_dir(path).map_err(|e| format!("Cannot read that folder: {e}"))?;
            let mut children: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|child| child.is_dir() && !project_files(child).is_empty())
                .collect();
            children.sort();
            let known: Vec<String> = self.lock().config.projects.iter().map(|p| p.path.clone()).collect();
            children.retain(|child| !known.iter().any(|path| same_path(path, &child.to_string_lossy())));
            if children.is_empty() {
                return Err("No Rojo projects found in that folder or its children".into());
            }
            let added: Vec<String> =
                children.iter().filter_map(|child| self.register(child, None, None, false).ok()).collect();
            if added.is_empty() {
                return Err("None of the Rojo projects in that folder could be added".into());
            }
            added
        };
        self.changed();
        if let [name] = added.as_slice() {
            let port = self.lock().port(name).unwrap_or_default();
            self.notice(format!("Added {name} on port {port}. Click the port to change it."));
        } else {
            let folder =
                path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
            self.notice(format!("Added {} projects from {folder}", added.len()));
        }
        Ok(added)
    }

    /// Save the project at `path` on the first free port and return its name. Without a
    /// `project_file`, default.project.json is preferred. A lane takes the folder's name with a number
    /// when that name is taken. With `local`, the project gets a local place and auto-connect.
    fn register(
        &self,
        path: &Path,
        project_file: Option<String>,
        lane: Option<config::Lane>,
        local: bool,
    ) -> Result<String, String> {
        let mut files = project_files(path);
        let project_file = match project_file {
            Some(file) if files.contains(&file) => file,
            Some(file) => return Err(format!("No {file} in {}", path.display())),
            None if files.is_empty() => return Err("No *.project.json file found in that folder".into()),
            None => match files.iter().position(|f| f == "default.project.json") {
                Some(index) => files.remove(index),
                None => files.remove(0),
            },
        };
        let path_text = path.to_string_lossy().into_owned();
        let folder =
            path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("Choose a folder, not a drive")?;
        let mut state = self.lock();
        let taken = |name: &str| state.config.projects.iter().any(|p| p.name.to_lowercase() == name.to_lowercase());
        if let Some(other) = state.config.projects.iter().find(|p| same_path(&p.path, &path_text)) {
            return Err(format!("{} already has that folder", other.name));
        }
        let name = match lane.is_some() {
            true => (1..).map(|n| if n == 1 { folder.clone() } else { format!("{folder} {n}") }).find(|n| !taken(n)),
            false => Some(folder.clone()).filter(|n| !taken(n)),
        };
        let name = name.ok_or_else(|| format!("A project named {folder} is already registered"))?;
        let used: HashSet<u16> = state.config.projects.iter().map(|p| p.port).collect();
        let listening = localnet::listeners().unwrap_or_default();
        let port = (FIRST_PORT..=u16::MAX)
            .find(|port| !used.contains(port) && !listening.contains_key(port))
            .ok_or("No available project ports. Remove a project or change an assigned port.")?;
        let mut config = state.config.clone();
        config.projects.push(config::Project {
            name: name.clone(),
            path: path_text,
            project_file,
            port,
            autostart: false,
            auto_connect: local,
            place: local.then(|| PlaceLink::Local(LocalPlace::new(&name))),
            sourcemap: false,
            lane,
            idle_close: None,
        });
        config::save(&config)?;
        state.config = config;
        Ok(name)
    }

    /// Look up a place ID or link with Roblox; empty text means "unlink", and "local" a local place
    /// file. Call before `update`.
    pub fn resolve_place(text: &str) -> Result<Option<PlaceChoice>, ResolveError> {
        match text.trim() {
            "" => Ok(None),
            text if text.eq_ignore_ascii_case("local") => Ok(Some(PlaceChoice::Local)),
            text => studio::resolve_place(text).map(|place| Some(PlaceChoice::Published(place))),
        }
    }

    /// Apply changes to one project after checking all of them.
    pub fn update(self: &Arc<Self>, name: &str, update: Update) -> Result<ProjectView, String> {
        let mut state = self.lock();
        let project = state.config.project(name).cloned().ok_or_else(|| missing(name))?;
        let new_name = update.name.as_deref().map(str::trim).unwrap_or(name).to_string();
        let linked = match update.place {
            None => project.place.clone(),
            Some(None) => None,
            Some(Some(PlaceChoice::Published(place))) => Some(PlaceLink::Published(place)),
            // A project that already has a local place keeps its file.
            Some(Some(PlaceChoice::Local)) => match &project.place {
                Some(local @ PlaceLink::Local(_)) => Some(local.clone()),
                _ => Some(PlaceLink::Local(LocalPlace::new(&new_name))),
            },
        };
        if update.auto_connect == Some(true) && linked.is_none() {
            return Err("Auto-connect needs a linked place.".into());
        }
        if update.autostart == Some(true) && project.lane.is_some() {
            return Err("A lane does not autostart.".into());
        }
        if project.lane.is_some() && !matches!(linked, Some(PlaceLink::Local(_))) {
            return Err("A lane keeps its local place.".into());
        }
        if update.idle_close.is_some_and(|minutes| minutes > config::MAX_IDLE_CLOSE) {
            return Err(format!("Idle close is at most {} minutes; 0 turns it off.", config::MAX_IDLE_CLOSE));
        }
        if new_name != name {
            if new_name.is_empty() {
                return Err("Project name cannot be empty".into());
            }
            if state.config.projects.iter().any(|p| p.name != name && p.name.to_lowercase() == new_name.to_lowercase())
            {
                return Err(format!("A project named {new_name} is already registered"));
            }
            if state.busy(name) {
                return Err("Stop the server before renaming it".into());
            }
        }
        if let Some(port) = update.port.filter(|port| *port != project.port) {
            if port == 0 {
                return Err("Port must be 1-65535".into());
            }
            if let Some(other) = state.config.projects.iter().find(|p| p.name != name && p.port == port) {
                return Err(format!("Port {port} already used by {}", other.name));
            }
            // Moving off a port another app holds is how a blocked project gets going again.
            if state.busy(name) {
                return Err("Stop the server before changing its port".into());
            }
        }
        // Unlinking a local place keeps a copy beside the project, since Studio saves into it and a
        // converted place's map is only there. The sweep at the next launch removes the original.
        let kept = match local_place(&project) {
            Some(old) if old.is_file() && !matches!(linked, Some(PlaceLink::Local(_))) => {
                let kept = Path::new(&project.path).join(old.file_name().unwrap_or_default());
                if !kept.exists() {
                    std::fs::copy(&old, &kept)
                        .map_err(|e| format!("Could not keep the local place at {}: {e}", kept.display()))?;
                }
                Some(kept)
            }
            _ => None,
        };
        let mut config = state.config.clone();
        let p = config.project_mut(name).ok_or_else(|| missing(name))?;
        p.port = update.port.unwrap_or(p.port);
        p.autostart = update.autostart.unwrap_or(p.autostart);
        p.auto_connect = update.auto_connect.unwrap_or(p.auto_connect) && linked.is_some();
        p.place = linked;
        p.name = new_name.clone();
        p.sourcemap = update.sourcemap.unwrap_or(p.sourcemap);
        p.idle_close = update.idle_close.map_or(p.idle_close, |minutes| (minutes > 0).then_some(minutes));
        // Lanes made from it follow the new name.
        for lane in config.projects.iter_mut().filter_map(|p| p.lane.as_mut()).filter(|lane| lane.parent == name) {
            lane.parent = new_name.clone();
        }
        config::save(&config)?;
        state.config = config;
        if new_name != name {
            // Logs, the last seen state, and backups follow the project to its new name.
            backups::rename(name, &new_name);
            if let Some(list) = state.backups.remove(name) {
                state.backups.insert(new_name.clone(), list);
            }
            if let Some(logs) = state.logs.remove(name) {
                state.logs.insert(new_name.clone(), logs);
            }
            if let Some(seen) = state.states.remove(name) {
                state.states.insert(new_name.clone(), seen);
            }
            if let Some(pid) = state.opened_hidden.remove(name) {
                state.opened_hidden.insert(new_name.clone(), pid);
            }
        }
        // A new idle time counts from now, so a Studio already idle that long is not closed at once.
        if update.idle_close.is_some() {
            let open: Vec<u64> = state
                .config
                .project(&new_name)
                .map_or_else(Vec::new, |p| state.place_bridges(p).map(|(id, _)| id).collect());
            for id in open {
                if let Some(report) = state.bridges.get_mut(&id) {
                    report.active = Instant::now();
                }
            }
        }
        let view = Self::view(&state, &new_name).ok_or_else(|| missing(&new_name))?;
        drop(state);
        // A running server picks up the change at once.
        match update.sourcemap {
            Some(true) => self.start_sourcemap(&new_name),
            Some(false) => self.stop_sourcemap(&new_name),
            None => {}
        }
        self.changed();
        if let Some(kept) = kept {
            self.notice(format!("Kept {new_name}'s local place at {}", kept.display()));
        }
        if view.place.as_ref().is_some_and(|place| place.published().is_some()) && view.experience.is_none() {
            self.look_up_experiences();
        }
        Ok(view)
    }

    /// Fetch the names of linked experiences not looked up yet, for grouping places by experience.
    fn look_up_experiences(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        std::thread::spawn(move || {
            let missing: HashSet<u64> = {
                let state = engine.lock();
                let linked = state.config.projects.iter().filter_map(|p| p.place.as_ref()?.published());
                let linked = linked.map(|place| place.universe_id);
                linked.filter(|id| !state.experiences.contains_key(id)).collect()
            };
            for universe_id in missing {
                if let Ok(name) = studio::experience_name(universe_id) {
                    engine.lock().experiences.insert(universe_id, name);
                    engine.changed();
                }
            }
        });
    }

    /// The answer to the window's question, asked once when another Studio MCP is set up: whether
    /// agents keep Rojo Manager's Studio tools as well.
    pub fn answer_studio_tools(&self, keep: bool) -> Result<(), String> {
        let mut state = self.lock();
        let mut config = state.config.clone();
        config.settings.studio_tools = keep;
        config.studio_tools_asked = true;
        config::save(&config)?;
        state.config = config;
        drop(state);
        self.changed();
        Ok(())
    }

    /// Remember when this profile first ran, for the star prompt.
    fn record_first_run(&self) {
        let mut state = self.lock();
        if state.config.first_run != 0 {
            return;
        }
        let mut config = state.config.clone();
        config.first_run = unix_now();
        if config::save(&config).is_ok() {
            state.config = config;
        }
    }

    /// The star prompt was answered, either way; it is not shown again.
    pub fn answer_star(&self) -> Result<(), String> {
        let mut state = self.lock();
        let mut config = state.config.clone();
        config.star_asked = true;
        config::save(&config)?;
        state.config = config;
        drop(state);
        self.changed();
        Ok(())
    }

    /// Show or hide an experience's places in the window.
    pub fn set_collapsed(&self, universe_id: u64, collapsed: bool) -> Result<(), String> {
        let mut state = self.lock();
        let mut config = state.config.clone();
        config.collapsed.retain(|id| *id != universe_id);
        if collapsed {
            config.collapsed.push(universe_id);
        }
        config::save(&config)?;
        state.config = config;
        drop(state);
        self.changed();
        Ok(())
    }

    /// Make a lane from the project `parent`, at `path`, a copy of it such as a git worktree: a
    /// project there with a local place of its own and auto-connect on, so its Studio connects as
    /// soon as it opens. Opening it is up to the caller. Returns the lane's project name.
    pub fn create_lane(&self, parent: &str, path: &Path, holder: Option<String>) -> Result<String, String> {
        let (parent, project_file) = {
            let state = self.lock();
            let project = state.config.project(parent).ok_or_else(|| missing(parent))?;
            (project.name.clone(), project.project_file.clone())
        };
        let lane = config::Lane { parent: parent.clone(), holder };
        let held = lane.holder.as_deref().map(|holder| format!(" for {holder}")).unwrap_or_default();
        let name = self.register(path, Some(project_file), Some(lane), true)?;
        self.changed();
        self.notice(format!("Made lane {name} from {parent}{held}"));
        Ok(name)
    }

    /// Refuse a new project's folder before anything is written in it, when `register` would refuse
    /// its name (a project is named after its folder).
    fn check_name_free(&self, path: &Path) -> Result<(), String> {
        let folder =
            path.file_name().map(|n| n.to_string_lossy().to_lowercase()).ok_or("Choose a folder, not a drive")?;
        match self.lock().config.projects.iter().find(|p| p.name.to_lowercase() == folder) {
            Some(other) => {
                Err(format!("A project named {} is already registered. Choose another folder name.", other.name))
            }
            None => Ok(()),
        }
    }

    /// Make a Rojo project in `path`, a new or empty folder, with `rojo init`, and add it with a
    /// local place. Returns its name.
    pub fn create_project(&self, path: &Path) -> Result<String, String> {
        self.check_name_free(path)?;
        let rojo = tooling::find_tool("Rojo").ok_or("Rojo is not installed. Install it in Settings.")?;
        scaffold::create(&rojo, path)?;
        let name = self.register(path, None, None, true)?;
        self.changed();
        self.notice(format!("Created {name} with a local place"));
        Ok(name)
    }

    /// Make a Rojo project in `path`, a new or empty folder, from the place file `file`: its code
    /// services become files, and a copy of the place, map and all, becomes the project's local place.
    /// Returns its name.
    pub fn convert_place(&self, file: &Path, path: &Path) -> Result<String, String> {
        self.check_name_free(path)?;
        let rojo = tooling::find_tool("Rojo").ok_or("Rojo is not installed. Install it in Settings.")?;
        if !file.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("rbxl")) {
            return Err("Choose a .rbxl place file. In Studio, File > Save to File saves one.".into());
        }
        scaffold::convert(&rojo, file, path)?;
        let name = self.register(path, None, None, true)?;
        let place = self.lock().config.project(&name).and_then(local_place).ok_or("The new project has no place")?;
        let copied = place.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::copy(file, &place));
        if let Err(error) = copied {
            let _ = self.remove_project(&name);
            return Err(format!(
                "Could not copy the place file: {error}. The project folder at {} is kept.",
                path.display()
            ));
        }
        self.changed();
        self.notice(format!("Converted {} into {name}", file.file_name().unwrap_or_default().to_string_lossy()));
        Ok(name)
    }

    /// Stop the project's server if it runs, then drop the project. Its folder is kept, but its local
    /// place file goes with it, and a lane's Studio window is closed first.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let lane = self.lock().config.project(name).is_some_and(|p| p.lane.is_some());
        match lane {
            true => self.remove_lane(name).map(|_| ()),
            false => self.remove_project(name),
        }
    }

    /// Force-close the Studio windows that have the lane's place open, then remove it as `remove`
    /// does. Returns how many Studio windows closed; a Studio that would not close stops it.
    pub fn remove_lane(&self, name: &str) -> Result<usize, String> {
        let project = self.lock().config.project(name).filter(|p| p.lane.is_some()).cloned();
        let project = project.ok_or_else(|| format!("No lane named {name}"))?;
        let closed = self.close_place(&project)?;
        self.remove_project(name)?;
        Ok(closed)
    }

    /// End lanes whose folder is gone, such as a deleted git worktree. Their Studio stays open.
    fn end_orphaned_lanes(&self) {
        let gone: Vec<String> = {
            let state = self.lock();
            let lanes = state.config.projects.iter().filter(|p| p.lane.is_some());
            let gone = |p: &&config::Project| {
                std::fs::metadata(&p.path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            };
            lanes.filter(gone).map(|p| p.name.clone()).collect()
        };
        for name in gone {
            if self.remove_project(&name).is_ok() {
                self.notice(format!("Lane {name} ended: its folder is gone"));
            }
        }
    }

    /// Delete place files in the places folder that no project links, left by a crash or an unlink,
    /// with the .rbxl.lock files Studio leaves beside them when it is closed by force.
    fn sweep_places(&self) {
        let linked: HashSet<PathBuf> = self.lock().config.projects.iter().filter_map(local_place).collect();
        let Ok(entries) = std::fs::read_dir(config::data_dir().join("places")) else { return };
        for path in entries.flatten().map(|entry| entry.path()) {
            let place = path.to_string_lossy().strip_suffix(".lock").map(PathBuf::from).unwrap_or_else(|| path.clone());
            if place.extension().is_some_and(|ext| ext == "rbxl") && !linked.contains(&place) {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn remove_project(&self, name: &str) -> Result<(), String> {
        let (busy, port) = {
            let state = self.lock();
            (state.busy(name), state.port(name)?)
        };
        if busy || port_in_use(port) {
            self.stop(name);
        }
        let mut state = self.lock();
        let place = state.config.project(name).and_then(local_place);
        let mut config = state.config.clone();
        config.projects.retain(|p| p.name != name);
        config::save(&config)?;
        state.config = config;
        state.logs.remove(name);
        state.states.remove(name);
        state.opened_hidden.remove(name);
        drop(state);
        if let Some(path) = place {
            let _ = std::fs::remove_file(format!("{}.lock", path.display()));
            // A Studio closed by force leaves auto-recovery copies of the place behind.
            studio_process::remove_recovery(&path);
            let _ = std::fs::remove_file(path);
        }
        self.changed();
        Ok(())
    }

    /// Open the project's place in Studio. A local place is built first when its file is missing,
    /// so what Studio saves into it is kept. With `hidden`, Studio opens without showing a window
    /// (Windows only). A Studio that already has the place open is shown or hidden instead, where
    /// that is supported.
    pub fn open_studio(&self, name: &str, hidden: bool) -> Result<PlaceLink, String> {
        let project = self.lock().config.project(name).cloned().ok_or_else(|| missing(name))?;
        let place = project.place.clone();
        let place = place.ok_or_else(|| format!("{name} has no linked place. Set one in Rojo Manager."))?;
        let open = self.place_processes(&project);
        if !studio_process::CAN_HIDE && hidden && !open.is_empty() {
            return Err("Hiding Studio is Windows only for now.".into());
        }
        if studio_process::CAN_HIDE && !open.is_empty() {
            for pid in &open {
                studio_process::set_hidden(*pid, hidden)?;
            }
            for report in
                self.lock().bridges.values_mut().filter(|report| report.pid.is_some_and(|p| open.contains(&p)))
            {
                report.hidden = hidden;
                report.active = Instant::now();
            }
            self.changed();
            return Ok(place);
        }
        let target = match &place {
            PlaceLink::Published(place) => studio::launch_uri(place),
            PlaceLink::Local(local) => {
                let path = local.path();
                if !path.is_file() {
                    self.build_place(&project, &path)?;
                }
                // A lock left by a Studio that crashed or was closed by force keeps Studio from
                // opening the file. A running Studio holds its lock open, so only a stale one goes.
                let _ = std::fs::remove_file(format!("{}.lock", path.display()));
                path.to_string_lossy().into_owned()
            }
        };
        let pid = studio_process::open(&target, hidden).map_err(|e| format!("Could not open Roblox Studio: {e}"))?;
        if let Some(pid) = pid {
            self.lock().opened_hidden.insert(name.to_string(), pid);
        }
        Ok(place)
    }

    /// Write what Studio holds for the project's synced instances into its files, or with `preview`
    /// list what that would change. The place must be open in a Studio with the bridge.
    pub fn pull(self: &Arc<Self>, name: &str, preview: bool) -> Result<Pulled, String> {
        let project = self.lock().config.project(name).cloned().ok_or_else(|| missing(name))?;
        let folder = PathBuf::from(&project.path);
        let file = std::fs::read(folder.join(&project.project_file))
            .map_err(|e| format!("Could not read {}: {e}", project.project_file))?;
        let file: Value =
            serde_json::from_slice(&file).map_err(|e| format!("{} is not JSON: {e}", project.project_file))?;
        let roots = pull::capture_roots(&file)?;
        let instance = self.project_instance(&project).ok_or_else(|| {
            format!("{name} is not open in Studio. Open its place, then pull; Pull reads the open place.")
        })?;
        let bridge = self.lock().bridges.get(&instance).and_then(|r| r.rojo.get("bridge")?.as_u64()).unwrap_or(0);
        if bridge < PULL_BRIDGE {
            return Err("This Studio's bridge is too old to pull. Update the bridge in Rojo Manager, then restart \
                Studio."
                .into());
        }
        pull::check_tools(&folder)?;
        let mut id = [0u8; 6];
        getrandom::fill(&mut id).map_err(|e| e.to_string())?;
        let capture = id.iter().fold(0u64, |id, byte| id << 8 | u64::from(*byte)); // fits a Luau number
        self.lock().captures.insert(capture, None);
        self.touch(instance);
        let result = self.link.run(instance, "capture", json!({ "id": capture, "roots": roots }), CAPTURE_TIMEOUT);
        let data = self.lock().captures.remove(&capture).flatten();
        let result = result?;
        let entries: Vec<pull::Entry> = match result.get("entries") {
            Some(entries) if entries.is_array() => {
                serde_json::from_value(entries.clone()).map_err(|_| "The bridge sent an unreadable capture")?
            }
            _ => Vec::new(), // an empty Luau table arrives as {}
        };
        let missing: Vec<String> = result
            .get("missing")
            .and_then(Value::as_array)
            .map(|paths| paths.iter().filter_map(Value::as_str).take(200).map(str::to_string).collect())
            .unwrap_or_default();
        // The bridge is untrusted: only the instances this project syncs may be replaced.
        if entries.iter().any(|entry| !roots.contains(&entry.path)) {
            return Err("The bridge sent instances the project does not sync".into());
        }
        let data = match (data, entries.iter().any(|entry| entry.count > 0)) {
            (Some(data), _) => BASE64.decode(data.as_bytes()).map_err(|_| "The bridge sent a damaged capture")?,
            (None, false) => Vec::new(),
            (None, true) => return Err("Studio's instances did not arrive".into()),
        };
        let prepared = pull::prepare(&folder, &project.project_file, &data, &entries)?;
        let mut pulled = prepared.syncback(true)?;
        if !preview && !pulled.is_empty() {
            // Rojo is kept away while the files are written: its server can crash when folders are
            // removed under it, and a connected Rojo would add the new files as instances beside
            // the ones they came from. So that Studio's session ends and the server stops; then
            // both come back, and connecting again matches the instances by name.
            let session = {
                let state = self.lock();
                let connected = state.bridges.get(&instance).is_some_and(|report| report.port == project.port);
                connected
                    || state.pending_sync(&project).is_some_and(|pending| pending.instance == instance.to_string())
            };
            let serving =
                self.project(name).is_some_and(|p| !matches!(p.state, ServerState::Off | ServerState::Blocked));
            let connect = || {
                let args = json!({ "port": project.port, "wait": true });
                match self.link.run(instance, "connect", args, Duration::from_secs(40)) {
                    Ok(status) => status.get("status").and_then(Value::as_str).unwrap_or("Unknown").to_string(),
                    Err(error) => error,
                }
            };
            if session {
                self.link.run(instance, "disconnect", json!({}), Duration::from_secs(15))?;
            }
            if serving {
                self.stop(name);
                self.wait_for_server(name, "stop");
                if port_in_use(project.port) {
                    if session {
                        connect();
                    }
                    return Err(format!("{name}'s server did not stop, so nothing was written."));
                }
            }
            let written = prepared.syncback(false);
            let mut rojo = None;
            if serving {
                let _ = self.start(name);
                self.wait_for_server(name, "start");
                if !port_in_use(project.port) {
                    rojo = Some("The server did not start again; see its log.".to_string());
                }
            }
            if session && rojo.is_none() {
                rojo = Some(connect()).filter(|status| status != "Connected");
            }
            pulled = written?;
            pulled.rojo = rojo;
            let (written, removed) = (pulled.written.len(), pulled.removed.len());
            self.log(name, &format!("[pulled from Studio: {written} written, {removed} removed]"));
        }
        pulled.preview = preview;
        pulled.missing = missing;
        Ok(pulled)
    }

    /// A bridge's capture for a pull that waits for it; anything else is dropped.
    pub fn save_capture(&self, id: u64, data: String) {
        if let Some(slot) = self.lock().captures.get_mut(&id) {
            *slot = Some(data);
        }
    }

    /// Build the project into a place file with `rojo build`; its output goes to the project's log.
    fn build_place(&self, project: &config::Project, path: &Path) -> Result<(), String> {
        let rojo = tooling::find_tool("Rojo").ok_or("Rojo is not installed.")?;
        let folder = path.parent().ok_or("Invalid place folder")?;
        std::fs::create_dir_all(folder).map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
        let file = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        self.log(&project.name, &format!("$ rojo build {} --output {file}", project.project_file));
        let mut command = Command::new(rojo);
        command
            .arg("build")
            .arg(Path::new(&project.path).join(&project.project_file))
            .arg("--output")
            .arg(path)
            .current_dir(&project.path)
            .stdin(std::process::Stdio::null());
        own_group(&mut command);
        let output = command.output().map_err(|e| format!("Could not run Rojo: {e}"))?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            self.log(&project.name, line);
        }
        if output.status.success() && path.is_file() {
            return Ok(());
        }
        let _ = std::fs::remove_file(path); // a half-written file would be opened next time
        let last = text.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("no output");
        Err(format!("Could not build {}'s place: {}", project.name, strip_ansi(last)))
    }

    pub fn open_folder(&self, name: &str) -> Result<(), String> {
        let path = self.lock().config.project(name).map(|p| p.path.clone()).ok_or_else(|| missing(name))?;
        crate::shell::open(&path).map_err(|e| format!("Cannot open folder: {e}"))
    }

    // ------------------------------------------------------------- settings

    /// Change one preference: a Settings key, or "launch_at_startup".
    pub fn set_setting(self: &Arc<Self>, key: &str, value: bool) -> Result<(), String> {
        let mut state = self.lock();
        if key == "launch_at_startup" {
            let hidden = state.config.settings.start_hidden;
            drop(state);
            startup::set_startup(value, hidden).map_err(|e| format!("Could not change Windows startup: {e}"))?;
            self.changed();
            return Ok(());
        }
        let mut config = state.config.clone();
        if !config.settings.set(key, value) {
            return Err(format!("Unknown setting {key}. Settings: launch_at_startup, {}", Settings::KEYS.join(", ")));
        }
        config::save(&config)?;
        state.config = config;
        drop(state);
        if key == "start_hidden" && startup::startup_enabled() {
            startup::set_startup(true, value).map_err(|e| format!("Could not change Windows startup: {e}"))?;
        }
        match key {
            "app_version_check" => self.check_app_update(false, false),
            "version_check" => self.check_rojo(false),
            _ => self.changed(),
        }
        Ok(())
    }

    pub fn settings(&self) -> Settings {
        self.lock().config.settings
    }

    // ------------------------------------------------------------- checks and tool jobs

    /// Check Rojo's install and, when enabled or forced, its latest release.
    pub fn check_rojo(self: &Arc<Self>, force: bool) {
        let (revision, enabled) = {
            let mut state = self.lock();
            state.rojo_revision += 1;
            state.rojo = None;
            (state.rojo_revision, state.config.settings.version_check)
        };
        self.changed();
        let engine = Arc::clone(self);
        std::thread::spawn(move || {
            let status = tooling::inspect_rojo(force || enabled);
            let mut state = engine.lock();
            if state.rojo_revision == revision {
                // An older check that finishes after a retry is ignored.
                state.rojo = Some(status);
                drop(state);
                engine.changed();
            }
        });
    }

    /// Check for a newer release when enabled or forced. With `install`, a newer release is installed,
    /// restarting into it; the new version's launch then rebuilds the bridge (see `refresh_bridge`).
    pub fn check_app_update(self: &Arc<Self>, force: bool, install: bool) {
        let (revision, enabled) = {
            let mut state = self.lock();
            state.app_update_revision += 1;
            state.app_update = None;
            (state.app_update_revision, state.config.settings.app_version_check)
        };
        self.changed();
        let engine = Arc::clone(self);
        std::thread::spawn(move || {
            let status = updates::inspect_update(enabled || force || install, crate::VERSION);
            let outdated = status.state == UpdateState::Outdated;
            let mut state = engine.lock();
            if state.app_update_revision != revision {
                return;
            }
            state.app_update = Some(status);
            drop(state);
            engine.changed();
            if install && outdated {
                let _ = engine.install_app_update(); // a failure shows in the update status
            }
        });
    }

    /// Rebuild an outdated Studio bridge in the background at launch, so updating Rojo Manager or
    /// Rojo updates the bridge too. Only a bridge this profile built (bridge.json) is rebuilt, and only
    /// when Rojo and Lune are installed; otherwise the window keeps offering Update bridge. This keeps
    /// isolated profiles, which share Studio's Plugins folder, from replacing the real bridge.
    fn refresh_bridge(&self) {
        let recorded = matches!(tooling::studio_plugin(), tooling::StudioPlugin::Bridge { built: Some(_) });
        let tools = tooling::find_tool("Rojo").is_some() && tooling::find_tool("Lune").is_some();
        if !recorded || !tools || self.bridge_update().is_none() {
            return;
        }
        match tooling::install_rojo_bridge() {
            Ok(_) => self.notice("Studio bridge updated. Restart Studio to load it."),
            Err(error) => {
                self.notice(format!("Could not update the Studio bridge: {error}. Use Update bridge in Settings."))
            }
        }
        self.changed();
    }

    /// Why the installed Studio bridge needs rebuilding, if it does.
    pub fn bridge_update(&self) -> Option<String> {
        let rojo = match self.lock().tool_versions.get("Rojo") {
            Some(version) => version.clone(),
            None => tooling::tool_version("Rojo", None).ok(), // not read yet at launch
        };
        tooling::bridge_outdated(&tooling::studio_plugin(), rojo.as_deref())
    }

    /// The newer release's version while one is available ("0.7.1"), for the tray.
    pub fn available_update(&self) -> Option<String> {
        let state = self.lock();
        let update = state.app_update.as_ref().filter(|update| update.state == UpdateState::Outdated)?;
        update.url.rsplit_once("/tag/v").map(|(_, version)| version.to_string())
    }

    /// Download and swap in the latest release, then have the tray app restart into it.
    pub fn install_app_update(&self) -> Result<String, String> {
        let installing = |message: String| UpdateStatus {
            state: UpdateState::Installing,
            message,
            url: updates::RELEASES_URL.into(),
        };
        {
            let mut state = self.lock();
            if state.app_update.as_ref().is_some_and(|update| update.state == UpdateState::Installing) {
                return Err("The update is already being installed.".into());
            }
            // A check finishing now must not overwrite this status.
            state.app_update_revision += 1;
            state.app_update = Some(installing("Downloading the update...".into()));
        }
        self.changed();
        let result = updates::install_latest(&self.app_dir, crate::VERSION);
        self.lock().app_update = Some(match &result {
            Ok(version) => installing(format!("Restarting into Rojo Manager {version}...")),
            Err(error) => {
                UpdateStatus { state: UpdateState::Error, message: error.clone(), url: updates::RELEASES_URL.into() }
            }
        });
        self.changed();
        if result.is_ok() {
            (self.emit)(Event::Restart);
        }
        result
    }

    pub fn refresh_tool_versions(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        std::thread::spawn(move || {
            let versions =
                tooling::TOOLS.iter().map(|tool| (tool.name, tooling::tool_version(tool.name, None).ok())).collect();
            engine.lock().tool_versions = versions;
            engine.changed();
        });
    }

    /// Run a tool job in the ToolInstaller console; the result arrives as a notice.
    pub fn run_tool_job(self: &Arc<Self>, job: Job) -> Result<(), String> {
        let mut state = self.lock();
        if state.tool_job.is_some() {
            return Err("An installation is already running. Check its terminal window.".into());
        }
        let (operation, tool) = match &job {
            Job::Global { tool } => ("global", tool),
            Job::Project { tool, .. } => ("project", tool),
            Job::Bridge { tool } => ("bridge", tool),
            Job::Plugin { tool } => ("plugin", tool),
        };
        let view = ToolJobView { operation: operation.into(), tool: tool.clone() };
        let installer = self.app_dir.join(crate::executable("ToolInstaller"));
        if !installer.is_file() {
            return Err("ToolInstaller is missing. Extract the complete release ZIP and retry.".into());
        }
        let folder = tempfile::Builder::new().prefix("rojo-manager-job-").tempdir().map_err(|e| e.to_string())?;
        let request = folder.path().join("request.json");
        std::fs::write(&request, serde_json::to_vec(&job).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
        let console = Console::open(&installer, &request, &self.app_dir)
            .map_err(|e| format!("Could not start installation: {e}"))?;
        let message = format!("Installing {}. Follow any prompts in the terminal.", view.tool);
        state.tool_job = Some(ToolJob { view, folder, console });
        drop(state);
        self.changed();
        self.notice(message);
        let engine = Arc::clone(self);
        std::thread::spawn(move || engine.wait_for_tool_job());
        Ok(())
    }

    fn wait_for_tool_job(self: Arc<Self>) {
        let result = loop {
            std::thread::sleep(Duration::from_millis(400));
            let mut state = self.lock();
            let Some(job) = state.tool_job.as_mut() else { return };
            let result_path = job.folder.path().join("result.json");
            if result_path.is_file() {
                let parsed =
                    std::fs::read(&result_path).ok().and_then(|data| serde_json::from_slice::<Value>(&data).ok());
                let outcome =
                    parsed.as_ref().and_then(|v| Some((v.get("ok")?.as_bool()?, v.get("message")?.as_str()?)));
                break match outcome {
                    Some((true, message)) => message.to_string(),
                    Some((false, message)) => format!("Installation failed: {message}"),
                    None => "Installation failed: Invalid installer result".to_string(),
                };
            }
            if job.console.ended(job.folder.path()) {
                break "Installation failed: Installation ended without completing. Check the terminal and retry."
                    .into();
            }
        };
        {
            let mut state = self.lock();
            state.tool_job = None; // deletes the request folder
            state.rojo_checked.clear();
            state.job_result = Some(result.clone());
        }
        self.changed();
        self.notice(result);
        self.refresh_tool_versions();
        self.check_rojo(true);
    }
}

// ----------------------------------------------------------------- control API

impl Engine {
    /// One project as reported to agents: its view without badges, plus its last output lines.
    fn agent_view(&self, name: &str) -> Option<Value> {
        let view = self.project(name)?;
        let logs = self.logs(name);
        let log: Vec<&String> = logs.iter().skip(logs.len().saturating_sub(8)).map(|(_, line)| line).collect();
        let mut value = serde_json::to_value(view).ok()?;
        let object = value.as_object_mut()?;
        object.remove("badges");
        object.remove("backups");
        object.insert("log".into(), json!(log));
        Some(value)
    }

    /// Install an update for an agent: "app" restarts into the newest release; "bridge" waits for the
    /// installer, whose terminal may ask the user to trust a tool.
    fn install_update(self: &Arc<Self>, target: &str) -> Result<Value, ControlError> {
        if target == "app" {
            let version = self.install_app_update().map_err(|e| ControlError::new(e, 409))?;
            self.notice(format!("Updating to Rojo Manager {version}, requested by an agent"));
            return Ok(json!({ "message": format!(
                "Rojo Manager {version} is installed and restarting. Its running servers restart with it, so \
                reconnect Rojo in Studio afterwards."
            )}));
        }
        if target != "bridge" {
            return Err(ControlError::new(format!("Unknown update {target}. Pass app or bridge."), 404));
        }
        self.lock().job_result = None;
        self.run_tool_job(Job::Bridge { tool: "Rojo".into() }).map_err(|e| ControlError::new(e, 409))?;
        while self.lock().tool_job.is_some() {
            std::thread::sleep(Duration::from_millis(500));
        }
        let result = self.lock().job_result.clone().unwrap_or_default();
        if result.starts_with("Installation failed") {
            return Err(ControlError::new(result, 500));
        }
        // The installer's result already says to restart Studio.
        Ok(json!({ "message": result }))
    }

    fn settings_value(&self) -> Value {
        let mut value = serde_json::to_value(self.settings()).unwrap_or_default();
        if let Some(object) = value.as_object_mut() {
            object.insert("launch_at_startup".into(), json!(startup::startup_enabled()));
        }
        json!({ "settings": value })
    }
}

/// The engine as the control servers' handler.
pub struct Control(pub Arc<Engine>);

impl ControlHandler for Control {
    fn projects(&self) -> Vec<Value> {
        let names: Vec<String> = self.0.lock().config.projects.iter().map(|p| p.name.clone()).collect();
        names.iter().filter_map(|name| self.0.agent_view(name)).collect()
    }

    fn act(&self, project: Option<&str>, action: &str, body: &Map<String, Value>) -> Result<Value, ControlError> {
        let engine = &self.0;
        if action == "add" {
            let path = body.get("path").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
            let path = PathBuf::from(path.ok_or_else(|| ControlError::bad("Pass the project folder's path"))?);
            if !path.is_absolute() {
                return Err(ControlError::bad(format!("{} is not an absolute path", path.display())));
            }
            let from = body.get("from").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
            let create = match body.get("create") {
                None | Some(Value::Null) => false,
                Some(value) => value.as_bool().ok_or_else(|| ControlError::bad("create must be a bool"))?,
            };
            let names = match from.map(PathBuf::from) {
                Some(file) if !file.is_absolute() || !file.is_file() => {
                    return Err(ControlError::new(format!("No place file at {}", file.display()), 404));
                }
                Some(file) => engine.convert_place(&file, &path).map(|name| vec![name]),
                None if create => engine.create_project(&path).map(|name| vec![name]),
                None if !path.is_dir() => {
                    return Err(ControlError::new(format!("No folder at {}", path.display()), 404));
                }
                None => engine.add_projects(&path),
            };
            let names = names.map_err(|e| ControlError::new(e, 409))?;
            let added: Vec<Value> = names.iter().filter_map(|name| engine.agent_view(name)).collect();
            return Ok(json!({ "added": added }));
        }
        let name = project.ok_or_else(|| ControlError::bad("Pass a project"))?;
        let Some(view) = engine.project(name) else {
            return Err(ControlError::new(missing(name), 404));
        };
        let reply = |name: &str| Ok(json!({ "project": engine.agent_view(name) }));
        // With "wait", start, restart, and stop answer once the server is up or gone.
        let wait = match body.get("wait") {
            None | Some(Value::Null) => false,
            Some(value) => value.as_bool().ok_or_else(|| ControlError::bad("wait must be a bool"))?,
        };
        let waited = |name: &str| {
            if wait {
                engine.wait_for_server(name, action);
            }
            reply(name)
        };
        match action {
            "update" | "link-place" => {
                let (mut update, place) = parse_update(action, body)?;
                if let Some(text) = place {
                    update.place = Some(Engine::resolve_place(&text).map_err(|error| match error {
                        ResolveError::Invalid(message) => ControlError::bad(message),
                        ResolveError::Unreachable(message) => {
                            ControlError::new(format!("Could not reach Roblox: {message}"), 502)
                        }
                    })?);
                }
                let updated = engine.update(name, update).map_err(|e| ControlError::new(e, 409))?;
                engine.notice(format!("{}: updated by an agent", updated.name));
                reply(&updated.name)
            }
            "restore" => {
                let id = body.get("backup").and_then(Value::as_str);
                let restored = engine.restore_backup(name, id).map_err(|e| ControlError::new(e, 409))?;
                engine.notice(format!("{name}: backup restored by an agent"));
                Ok(restored)
            }
            "remove" => {
                engine.remove(name).map_err(|e| ControlError::new(e, 500))?;
                engine.notice(format!("{name}: removed by an agent"));
                Ok(json!({ "removed": name }))
            }
            "start" | "restart" if view.state == ServerState::Blocked => {
                Err(ControlError::new(format!("Port {} is used by another application", view.port), 409))
            }
            "start" => {
                if matches!(view.state, ServerState::Off | ServerState::Starting) {
                    // a Starting one may be a launch that a stop cancelled
                    engine
                        .start(name)
                        .map_err(|e| ControlError::new(format!("Rojo could not be started. {e}"), 409))?;
                }
                engine.notice(format!("{name}: start requested by an agent"));
                waited(name)
            }
            "stop" => {
                engine.stop(name);
                engine.notice(format!("{name}: stop requested by an agent"));
                waited(name)
            }
            "restart" => {
                engine.restart(name).map_err(|e| ControlError::new(e, 409))?;
                engine.notice(format!("{name}: restart requested by an agent"));
                waited(name)
            }
            "open-studio" => {
                let status = if view.place.is_none() { 409 } else { 500 };
                let hidden = match body.get("hidden") {
                    None | Some(Value::Null) => false,
                    Some(value) => value.as_bool().ok_or_else(|| ControlError::bad("hidden must be a bool"))?,
                };
                engine.open_studio(name, hidden).map_err(|e| ControlError::new(e, status))?;
                engine.notice(format!("{name}: open studio requested by an agent"));
                reply(name)
            }
            "pull" => {
                let preview = match body.get("preview") {
                    None | Some(Value::Null) => false,
                    Some(value) => value.as_bool().ok_or_else(|| ControlError::bad("preview must be a bool"))?,
                };
                let pulled = engine.pull(name, preview).map_err(|e| ControlError::new(e, 409))?;
                if !preview {
                    engine.notice(format!("{name}: pulled from Studio by an agent"));
                }
                serde_json::to_value(pulled).map_err(|e| ControlError::new(e.to_string(), 500))
            }
            _ => Err(ControlError::new(format!("Unknown action {action}"), 404)),
        }
    }

    fn lane(&self, name: Option<&str>, body: &Map<String, Value>) -> Result<Value, ControlError> {
        let engine = &self.0;
        if let Some(name) = name {
            let name = engine.project(name).map_or_else(|| name.to_string(), |p| p.name);
            let closed = engine.remove_lane(&name).map_err(|e| ControlError::new(e, 409))?;
            engine.notice(format!("Lane {name}: removed by an agent"));
            return Ok(json!({ "removed": name, "closedStudioWindows": closed }));
        }
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty());
        let parent = text("from").ok_or_else(|| ControlError::bad("Pass from, the project to make the lane from"))?;
        let path = PathBuf::from(text("path").ok_or_else(|| ControlError::bad("Pass the lane folder's path"))?);
        if !path.is_absolute() || !path.is_dir() {
            return Err(ControlError::new(format!("No folder at {}. Pass an absolute path.", path.display()), 404));
        }
        let holder = text("holder").map(|holder| holder.chars().take(100).collect::<String>());
        let name = engine.create_lane(parent, &path, holder).map_err(|e| ControlError::new(e, 409))?;
        Ok(json!({ "project": engine.agent_view(&name) }))
    }

    fn backups(&self, project: &str) -> Result<Value, ControlError> {
        let view = self.0.project(project).ok_or_else(|| ControlError::new(missing(project), 404))?;
        Ok(json!({ "project": view.name, "backups": view.backups }))
    }

    fn save_backup(&self, backup: BackupUpload) {
        self.0.save_backup(backup);
    }

    fn logs(&self, project: &str) -> Result<Value, ControlError> {
        let view = self.0.project(project).ok_or_else(|| ControlError::new(missing(project), 404))?;
        let lines: Vec<String> = self.0.logs(&view.name).into_iter().map(|(_, line)| line).collect();
        Ok(json!({ "project": view.name, "state": view.state, "lines": lines }))
    }

    fn report_rojo_plugins(&self, count: u32) {
        self.0.report_studio_rojo_plugins(count);
    }

    fn report_output(&self, output: StudioOutput) {
        self.0.report_output(output);
    }

    fn settings(&self, changes: Option<&Map<String, Value>>) -> Result<Value, ControlError> {
        if let Some(changes) = changes {
            for (key, value) in changes {
                if key != "launch_at_startup" && !Settings::KEYS.contains(&key.as_str()) {
                    let keys = Settings::KEYS.join(", ");
                    return Err(ControlError::bad(format!(
                        "Unknown setting {key}. Settings: launch_at_startup, {keys}"
                    )));
                }
                if !value.is_boolean() {
                    return Err(ControlError::bad(format!("{key} must be true or false")));
                }
            }
            for (key, value) in changes {
                self.0.set_setting(key, value.as_bool() == Some(true)).map_err(|e| ControlError::new(e, 500))?;
            }
            if !changes.is_empty() {
                self.0.notice("Settings changed by an agent");
            }
        }
        Ok(self.0.settings_value())
    }

    fn save_capture(&self, id: u64, data: String) {
        self.0.save_capture(id, data);
    }

    fn poll_bridge(&self, id: u64, poll: BridgePoll, connection: Option<(u16, u16)>) -> Value {
        let extra = PollExtra { connection, idle: poll.idle };
        let offer = self.0.report_bridge(id, poll.place_id, poll.place_name, poll.port, poll.rojo, extra);
        let result = poll.result.map(|result| {
            let outcome = match result.ok {
                true => Ok(result.value),
                false => Err(result.value.as_str().unwrap_or("The command failed in Studio").to_string()),
            };
            (result.id, outcome)
        });
        let command = self.0.link.poll(id, result, BRIDGE_HOLD);
        let backups = self.0.settings().backups;
        json!({
            "port": offer.map(|(port, _)| port),
            "accept": offer.is_some_and(|(_, accept)| accept),
            "backups": backups,
            "command": command,
        })
    }

    fn studio(&self, command: Option<(&str, &Map<String, Value>)>) -> Result<Value, ControlError> {
        let Some((instance, body)) = command else {
            let installed = matches!(tooling::studio_plugin(), tooling::StudioPlugin::Bridge { .. });
            return Ok(json!({ "bridge": installed, "instances": self.0.studio_instances() }));
        };
        let action = body.get("action").and_then(Value::as_str).unwrap_or_default();
        if action == "output" {
            let (since, count, level) = parse_output_query(body)?;
            return self.0.studio_output(instance, since, level, count);
        }
        let valid = match action {
            "status" | "disconnect" | "accept" | "abort" => true,
            "connect" => body.get("port").and_then(Value::as_u64).is_some_and(|port| (1..=65535).contains(&port)),
            "luau" => body.get("code").is_some_and(Value::is_string),
            _ => false,
        };
        if !valid {
            return Err(ControlError::bad(
                "Pass action status, disconnect, accept, abort, connect (with port), luau (with code), output, \
                 show, or hide",
            ));
        }
        self.0.studio_command(instance, action, Value::Object(body.clone()))
    }

    fn updates(&self, install: Option<&str>) -> Result<Value, ControlError> {
        if let Some(target) = install {
            return self.0.install_update(target);
        }
        let app = updates::inspect_update(true, crate::VERSION);
        let bridge = match (tooling::studio_plugin(), self.0.bridge_update()) {
            (_, Some(message)) => json!({ "state": "outdated", "message": message }),
            (tooling::StudioPlugin::Bridge { .. }, None) => json!({ "state": "current" }),
            (_, None) => json!({ "state": "missing", "message": "The Studio bridge is not installed." }),
        };
        Ok(json!({
            "app": { "installed": crate::VERSION, "state": app.state, "message": app.message },
            "bridge": bridge,
        }))
    }
}

/// Read an output request: {since, lines, level}, defaulting to every line (the newest 100).
fn parse_output_query(body: &Map<String, Value>) -> Result<(u64, usize, OutputKind), ControlError> {
    let since = match body.get("since") {
        None | Some(Value::Null) => 0,
        Some(value) => value.as_u64().ok_or_else(|| ControlError::bad("since must be a line number"))?,
    };
    let count = match body.get("lines") {
        None | Some(Value::Null) => 100,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=500).contains(n))
            .ok_or_else(|| ControlError::bad("lines must be 1 to 500"))?,
    };
    let level = match body.get("level").and_then(Value::as_str) {
        None | Some("all") => OutputKind::Output,
        Some("warning") => OutputKind::Warning,
        Some("error") => OutputKind::Error,
        Some(_) => return Err(ControlError::bad("level must be all, warning, or error")),
    };
    Ok((since, count as usize, level))
}

/// Read an agent's update body (name, port, autostart, autoConnect, place, sourcemap, idleClose) or
/// link-place body (place). The place text comes back separately, still to be looked up.
fn parse_update(action: &str, body: &Map<String, Value>) -> Result<(Update, Option<String>), ControlError> {
    const FIELDS: &str = "name, port, autostart, autoConnect, place, sourcemap, idleClose";
    if action == "link-place" {
        let place = body.get("place").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
        let place = place.ok_or_else(|| ControlError::bad("Pass a place ID or link, or local"))?;
        return Ok((Update::default(), Some(place.to_string())));
    }
    if body.is_empty() {
        return Err(ControlError::bad(format!("Pass at least one of: {FIELDS}")));
    }
    let (mut update, mut place) = (Update::default(), None);
    for (key, value) in body {
        let wrong = |kind: &str| ControlError::bad(format!("{key} must be a {kind}"));
        match key.as_str() {
            "name" => update.name = Some(value.as_str().ok_or_else(|| wrong("str"))?.to_string()),
            "place" => place = Some(value.as_str().ok_or_else(|| wrong("str"))?.to_string()),
            "port" => {
                let port = value.as_u64().ok_or_else(|| wrong("int"))?;
                let port = u16::try_from(port).ok().filter(|p| *p > 0);
                update.port = Some(port.ok_or_else(|| ControlError::new("Port must be 1-65535", 409))?);
            }
            "autostart" => update.autostart = Some(value.as_bool().ok_or_else(|| wrong("bool"))?),
            "autoConnect" => update.auto_connect = Some(value.as_bool().ok_or_else(|| wrong("bool"))?),
            "sourcemap" => update.sourcemap = Some(value.as_bool().ok_or_else(|| wrong("bool"))?),
            "idleClose" => {
                let minutes = value.as_u64().and_then(|minutes| u32::try_from(minutes).ok());
                update.idle_close = Some(minutes.ok_or_else(|| wrong("number of minutes (0 for off)"))?);
            }
            _ => return Err(ControlError::bad(format!("Unknown field {key}. Fields: {FIELDS}"))),
        }
    }
    Ok((update, place))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi_colors_are_stripped_from_logs() {
        assert_eq!(strip_ansi("\u{1b}[0m\u{1b}[1m\u{1b}[32mlocalhost\u{1b}[0m:34872"), "localhost:34872");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn agent_updates_are_checked_field_by_field() {
        let body = |value: Value| value.as_object().cloned().unwrap();
        let (update, place) =
            parse_update("update", &body(json!({"port": 35000, "autoConnect": true, "place": ""}))).unwrap();
        assert_eq!((update.port, update.auto_connect, place.as_deref()), (Some(35000), Some(true), Some("")));
        assert_eq!(parse_update("update", &body(json!({"port": "1"}))).unwrap_err().message, "port must be a int");
        assert_eq!(parse_update("update", &body(json!({"sourcemap": true}))).unwrap().0.sourcemap, Some(true));
        assert!(parse_update("update", &body(json!({"colour": 1}))).unwrap_err().message.starts_with("Unknown field"));
        assert_eq!(parse_update("update", &body(json!({}))).unwrap_err().status, 400);
        assert_eq!(parse_update("link-place", &body(json!({"place": " 123 "}))).unwrap().1.as_deref(), Some("123"));
    }

    #[test]
    fn studio_output_is_read_after_a_cursor_by_level_and_needs_a_new_bridge() {
        let engine = Engine::new(Config::default(), PathBuf::new(), |_| {});
        let report = |id, place_id, name: Option<&str>, rojo| {
            engine.report_bridge(id, place_id, name.map(Into::into), 0, rojo, PollExtra::default())
        };
        report(1, 100, None, json!({"bridge": OUTPUT_BRIDGE}));
        report(2, 200, None, json!({"bridge": OUTPUT_BRIDGE - 1}));
        report(3, 0, Some("Lane-1.rbxl"), json!({"bridge": OUTPUT_BRIDGE}));
        let line = |text: &str, kind| crate::control::OutputReport { text: text.into(), kind };
        engine.report_output(StudioOutput {
            place_id: 0,
            place_name: Some("Lane-2.rbxl".into()),
            context: OutputContext::Edit,
            lines: vec![line("other lane", OutputKind::Output)],
        });
        engine.report_output(StudioOutput {
            place_id: 0,
            place_name: Some("Lane-1.rbxl".into()),
            context: OutputContext::Edit,
            lines: vec![line("lane", OutputKind::Output)],
        });
        engine.report_output(StudioOutput {
            place_id: 100,
            place_name: None,
            context: OutputContext::Server,
            lines: vec![
                line("hi", OutputKind::Output),
                line("careful", OutputKind::Warning),
                line("boom", OutputKind::Error),
            ],
        });
        let read = |since, level, count| engine.studio_output("1", since, level, count).unwrap();
        let texts = |value: Value| -> Vec<String> {
            value["lines"].as_array().unwrap().iter().map(|line| line["text"].as_str().unwrap().into()).collect()
        };
        let all = read(0, OutputKind::Output, 100);
        assert_eq!(all["next"], 5);
        assert_eq!(all["lines"][0], json!({"context": "server", "kind": "output", "text": "hi"}));
        assert_eq!(texts(read(0, OutputKind::Warning, 100)), ["careful", "boom"]);
        assert_eq!(texts(read(3, OutputKind::Output, 1)), ["boom"]);
        assert_eq!(read(5, OutputKind::Output, 100), json!({"lines": [], "next": 5}));
        assert_eq!(texts(engine.studio_output("3", 0, OutputKind::Output, 100).unwrap()), ["lane"]);
        assert_eq!(engine.studio_output("2", 0, OutputKind::Output, 100).unwrap_err().status, 409);
        assert_eq!(engine.studio_output("9", 0, OutputKind::Output, 100).unwrap_err().status, 404);
    }

    #[test]
    fn the_star_prompt_waits_for_projects_and_three_days_and_asks_once() {
        let project = config::Project {
            name: "MyGame".into(),
            path: "C:\\Games\\MyGame".into(),
            project_file: "default.project.json".into(),
            port: FIRST_PORT,
            autostart: false,
            auto_connect: false,
            place: None,
            sourcemap: false,
            lane: None,
            idle_close: None,
        };
        let day = 24 * 60 * 60;
        let mut config = Config { projects: vec![project], first_run: 10 * day, ..Config::default() };
        assert!(!star_due(&config, 12 * day));
        assert!(star_due(&config, 13 * day));
        config.star_asked = true;
        assert!(!star_due(&config, 13 * day));
        let empty = Config { first_run: 10 * day, ..Config::default() };
        assert!(!star_due(&empty, 13 * day));
    }

    #[test]
    fn auto_connect_finds_a_local_place_by_its_file_name() {
        let local = config::Project {
            name: "Lane".into(),
            path: "C:\\Games\\Lane".into(),
            project_file: "default.project.json".into(),
            port: FIRST_PORT,
            autostart: false,
            auto_connect: true,
            place: Some(PlaceLink::Local(LocalPlace { file: "Lane-1.rbxl".into() })),
            sourcemap: false,
            lane: Some(Lane { parent: "MyGame".into(), holder: None }),
            idle_close: None,
        };
        let engine = Engine::new(Config { projects: vec![local], ..Config::default() }, PathBuf::new(), |_| {});
        engine.lock().states.insert("Lane".into(), ServerState::Serving);
        let offer = |place_id, name: Option<&str>| {
            engine.report_bridge(1, place_id, name.map(Into::into), 0, json!({}), PollExtra::default())
        };
        assert_eq!(offer(0, Some("Lane-1.rbxl")), Some((FIRST_PORT, true)));
        assert_eq!(offer(0, Some("Lane-2.rbxl")), None);
        assert_eq!(offer(0, None), None); // an older bridge does not name the place
        assert_eq!(offer(7, Some("Lane-1.rbxl")), None);
    }

    #[test]
    fn a_lane_is_saved_with_its_own_place_and_ends_when_its_folder_is_gone() {
        let _guard = crate::isolated();
        let folders = tempfile::tempdir().unwrap();
        let folder = |parent: &str| {
            let path = folders.path().join(parent).join("MyGame");
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("default.project.json"), "{}").unwrap();
            path
        };
        let engine = Engine::new(Config::default(), PathBuf::new(), |_| {});
        let main = folder("main");
        engine.add_projects(&main).unwrap();
        assert!(engine.create_lane("Missing", &folder("other"), None).is_err());
        // A new project named like one already listed is refused before anything is written.
        let taken = folders.path().join("new").join("mygame");
        assert!(engine.create_project(&taken).unwrap_err().contains("already registered") && !taken.exists());
        // A copy without the parent's project file is refused, not served from another file.
        let wrong = folders.path().join("wrong");
        std::fs::create_dir_all(&wrong).unwrap();
        std::fs::write(wrong.join("lobby.project.json"), "{}").unwrap();
        assert!(engine.create_lane("MyGame", &wrong, None).unwrap_err().contains("default.project.json"));
        let lane_folder = folder("lane");
        let name = engine.create_lane("MyGame", &lane_folder, Some("lane B".into())).unwrap();
        assert_eq!(name, "MyGame 2");
        let saved = config::load().unwrap().projects.pop().unwrap();
        assert_eq!(saved.lane, Some(Lane { parent: "MyGame".into(), holder: Some("lane B".into()) }));
        assert!(saved.auto_connect);
        let Some(PlaceLink::Local(place)) = saved.place else { panic!("a lane has a local place") };
        let autostart = Update { autostart: Some(true), ..Update::default() };
        assert!(engine.update("MyGame 2", autostart).is_err());
        // Renaming the parent keeps the lane under it.
        engine.update("MyGame", Update { name: Some("Game".into()), ..Update::default() }).unwrap();
        assert_eq!(engine.project("MyGame 2").unwrap().lane.unwrap().parent, "Game");
        // Unlinking a local place keeps a copy beside the project.
        let local = Update { place: Some(Some(PlaceChoice::Local)), ..Update::default() };
        let Some(PlaceLink::Local(own)) = engine.update("Game", local).unwrap().place else { panic!("local") };
        std::fs::create_dir_all(own.path().parent().unwrap()).unwrap();
        std::fs::write(own.path(), "map").unwrap();
        engine.update("Game", Update { place: Some(None), ..Update::default() }).unwrap();
        let kept = main.join(&own.file);
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "map");
        // Deleting the lane's folder ends it, with its place; a place nothing links is swept.
        std::fs::create_dir_all(place.path().parent().unwrap()).unwrap();
        std::fs::write(place.path(), "built").unwrap();
        let stray = place.path().with_file_name("Stray-00000000.rbxl");
        std::fs::write(&stray, "left by a crash").unwrap();
        let lock = place.path().with_file_name("Stray-00000000.rbxl.lock");
        std::fs::write(&lock, "left by a force-closed Studio").unwrap();
        std::fs::remove_dir_all(&lane_folder).unwrap();
        engine.end_orphaned_lanes();
        assert!(engine.project("MyGame 2").is_none() && !place.path().exists());
        engine.sweep_places();
        assert!(!stray.exists() && !lock.exists());
    }

    #[test]
    fn a_studio_waiting_to_sync_shows_on_the_project_it_connects_to() {
        let project = config::Project {
            name: "MyGame".into(),
            path: "C:\\Games\\MyGame".into(),
            project_file: "default.project.json".into(),
            port: 34880,
            autostart: false,
            auto_connect: false,
            place: Some(PlaceLink::Published(Place { place_id: 100, universe_id: 1 })),
            sourcemap: false,
            lane: None,
            idle_close: None,
        };
        let engine = Engine::new(Config { projects: vec![project], ..Config::default() }, PathBuf::new(), |_| {});
        let pending = || engine.project("MyGame").unwrap().pending_sync;
        let report = |rojo| engine.report_bridge(7, 100, None, 0, rojo, PollExtra::default());
        report(json!({"status": "Confirming", "port": "34999"}));
        assert_eq!(pending(), None); // connecting to another server
        let paths: Vec<String> = (0..30).map(|n| format!("Workspace.Part{n}")).collect();
        let summary = json!({"removed": {"count": 30, "paths": paths}, "added": {"count": 0, "paths": {}},
                             "changed": {"count": 1, "paths": ["ServerScriptService.Main", 5]}});
        report(json!({"status": "Confirming", "port": "34880", "pending": summary}));
        let shown = pending().unwrap();
        let summary = shown.summary.unwrap();
        assert_eq!(shown.instance, "7");
        assert_eq!((summary.removed.count, summary.removed.paths.len()), (30, PENDING_PATHS));
        assert_eq!(summary.changed.paths, ["ServerScriptService.Main"]);
        assert!(summary.added.paths.is_empty());
        // An older bridge: waiting, with nothing to show about it.
        report(json!({"status": "Confirming", "port": "34880"}));
        assert_eq!(pending().unwrap().summary, None);
        report(json!({"status": "Connected", "port": "34880"}));
        assert_eq!(pending(), None);
    }

    #[test]
    fn an_idle_studio_with_an_old_bridge_stays_open_and_its_user_is_told_once() {
        let project = config::Project {
            name: "MyGame".into(),
            path: "C:\\Games\\MyGame".into(),
            project_file: "default.project.json".into(),
            port: 34880,
            autostart: false,
            auto_connect: false,
            place: Some(PlaceLink::Published(Place { place_id: 100, universe_id: 1 })),
            sourcemap: false,
            lane: None,
            idle_close: Some(1),
        };
        let alerts = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&alerts);
        let engine =
            Engine::new(Config { projects: vec![project], ..Config::default() }, PathBuf::new(), move |event| {
                if let Event::Alert(message) = event {
                    seen.lock().unwrap().push(message);
                }
            });
        // A bridge older than 13 does not say how long its Studio has been idle.
        let report = || engine.report_bridge(7, 100, None, 0, json!({}), PollExtra::default());
        report();
        assert!(alerts.lock().unwrap().is_empty());
        engine.lock().bridges.get_mut(&7).unwrap().active =
            Instant::now().checked_sub(Duration::from_secs(61)).unwrap();
        report();
        report();
        assert_eq!(alerts.lock().unwrap().len(), 1);
        assert!(alerts.lock().unwrap()[0].starts_with("MyGame: its Studio looks idle but stays open"));
    }

    #[test]
    fn waiting_wakes_on_a_change_and_gives_up_at_the_timeout() {
        let engine = Engine::new(Config::default(), PathBuf::new(), |_| {});
        let flag = Arc::new(AtomicBool::new(false));
        let setter = {
            let (engine, flag) = (Arc::clone(&engine), Arc::clone(&flag));
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                flag.store(true, Ordering::SeqCst);
                engine.changed();
            })
        };
        let started = Instant::now();
        assert!(engine.wait_until(Duration::from_secs(5), || flag.load(Ordering::SeqCst)));
        assert!(started.elapsed() < Duration::from_millis(240)); // woken by the change, not the slice
        setter.join().unwrap();
        assert!(!engine.wait_until(Duration::from_millis(100), || false));
    }

    #[test]
    fn project_paths_compare_like_windows() {
        assert!(same_path("C:\\Games\\A\\", "c:\\games\\a"));
        assert!(!same_path("C:\\Games\\A", "C:\\Games\\AB"));
    }
}
