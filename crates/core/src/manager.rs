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

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value, json};

use crate::config::{self, Config, Place, Settings};
use crate::control::{BridgePoll, ControlError, ControlHandler, OutputContext, OutputKind, StudioOutput};
use crate::localnet::{self, port_in_use};
use crate::studio::{self, ResolveError};
use crate::studio_link::Link;
use crate::tooling::{self, Job, RojoState, RojoStatus};
use crate::updates::{self, UpdateState, UpdateStatus};
use crate::{rojo_api, startup};

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
    pub place: Option<Place>,
    /// The linked place's experience name, once looked up.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experience: Option<String>,
    pub sourcemap: bool,
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
    pub place: Option<Option<Place>>,
    pub sourcemap: Option<bool>,
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
    /// Another Studio MCP set up in an agent, found at launch.
    other_studio_mcp: Option<String>,
    /// Studio's recent Output by place, from its Edit DataModel and playtests.
    output: HashMap<u64, VecDeque<OutputLine>>,
    output_seq: u64,
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
    /// The server its Rojo is connected to, or 0.
    port: u16,
    /// The Rojo plugin's status, as the bridge describes it.
    rojo: Value,
    /// The auto-connect server offered to it.
    offered: Option<u16>,
    seen: Instant,
}

impl State {
    /// The running server whose project auto-connects to this place: (project name, port).
    fn auto_connect_target(&self, place_id: u64) -> Option<(String, u16)> {
        let project = self.config.projects.iter().find(|p| {
            p.auto_connect
                && p.place.is_some_and(|place| place.place_id == place_id)
                && self.states.get(&p.name).is_some_and(|s| s.running())
        })?;
        Some((project.name.clone(), project.port))
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
            link: Link::default(),
        })
    }

    /// Start the watcher and update checks, then autostart projects.
    pub fn run(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        std::thread::spawn(move || engine.watch());
        self.record_first_run();
        self.check_rojo(false);
        let auto_update = self.settings().auto_update;
        self.check_app_update(false, auto_update);
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
        (self.emit)(Event::Changed);
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
            place: p.place,
            sourcemap: p.sourcemap,
            experience: p.place.and_then(|place| state.experiences.get(&place.universe_id).cloned()),
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
        self.stop_server(name);
        self.wake_watcher();
        self.changed();
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
                    let _ = engine.start(&name);
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
    pub fn report_bridge(&self, id: u64, place_id: u64, port: u16, rojo: Value) -> Option<(u16, bool)> {
        let (target, previous, accept) = {
            let mut state = self.lock();
            let target = state.auto_connect_target(place_id);
            let offered = target.as_ref().map(|(_, port)| *port);
            let report = BridgeReport { place_id, port, rojo, offered, seen: Instant::now() };
            (target, state.bridges.insert(id, report), state.config.settings.auto_connect_accept)
        };
        if previous.as_ref().map(|report| report.port) != Some(port) {
            self.wake_watcher();
        }
        let (name, offer) = target?;
        if port == 0 && previous.and_then(|report| report.offered) != Some(offer) {
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

    /// The Studio windows whose bridge polled recently: {instanceId, placeId, rojo}.
    pub fn studio_instances(&self) -> Vec<Value> {
        let state = self.lock();
        let live = state.bridges.iter().filter(|(id, report)| self.bridge_live(**id, report));
        let mut instances: Vec<(u64, &BridgeReport)> = live.map(|(id, report)| (*id, report)).collect();
        instances.sort_by_key(|(id, _)| *id);
        let instance = |(id, report): (u64, &BridgeReport)| json!({ "instanceId": id.to_string(), "placeId": report.place_id, "rojo": report.rojo });
        instances.into_iter().map(instance).collect()
    }

    /// Run a command in one Studio window through its bridge and wait for the result.
    pub fn studio_command(&self, instance: &str, action: &str, args: Value) -> Result<Value, ControlError> {
        let id = instance
            .parse::<u64>()
            .ok()
            .filter(|id| self.lock().bridges.get(id).is_some_and(|report| self.bridge_live(*id, report)));
        let id = id.ok_or_else(|| ControlError::new(format!("No Studio window has instanceId {instance}"), 404))?;
        let timeout = if action == "luau" { LUAU_TIMEOUT } else { Duration::from_secs(15) };
        self.link.run(id, action, args, timeout).map_err(|error| ControlError::new(error, 409))
    }

    /// Keep new lines of a place's Output; the oldest go past OUTPUT_CAP.
    pub fn report_output(&self, output: StudioOutput) {
        let mut guard = self.lock();
        let state = &mut *guard;
        let buffer = state.output.entry(output.place_id).or_default();
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
        let buffer = state.output.get(&report.place_id);
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
            let (projects, mut launching, bridged) = {
                let mut state = self.lock();
                state.bridges.retain(|id, report| self.bridge_live(*id, report));
                // The ports live bridges report as connected; None when no bridge runs.
                let bridged: Option<HashSet<u16>> = (!state.bridges.is_empty())
                    .then(|| state.bridges.values().map(|report| report.port).filter(|&port| port != 0).collect());
                (state.config.projects.clone(), !state.starting.is_empty() || !state.restarting.is_empty(), bridged)
            };
            let mut changed = false;
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

    /// Add the Rojo project at `path`, or each new project in its immediate subfolders.
    /// Returns the added names; errors when nothing could be added.
    pub fn add_projects(&self, path: &Path) -> Result<Vec<String>, String> {
        let added = if !project_files(path).is_empty() {
            vec![self.register(path)?]
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
            let added: Vec<String> = children.iter().filter_map(|child| self.register(child).ok()).collect();
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

    /// Save the project at `path` on the first free port and return its name.
    fn register(&self, path: &Path) -> Result<String, String> {
        let mut files = project_files(path);
        if files.is_empty() {
            return Err("No *.project.json file found in that folder".into());
        }
        let project_file = match files.iter().position(|f| f == "default.project.json") {
            Some(index) => files.remove(index),
            None => files.remove(0),
        };
        let path_text = path.to_string_lossy().into_owned();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("Choose a folder, not a drive")?;
        let mut state = self.lock();
        if state
            .config
            .projects
            .iter()
            .any(|p| p.name.to_lowercase() == name.to_lowercase() || same_path(&p.path, &path_text))
        {
            return Err(format!("A project named {name} is already registered"));
        }
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
            auto_connect: false,
            place: None,
            sourcemap: false,
        });
        config::save(&config)?;
        state.config = config;
        Ok(name)
    }

    /// Look up a place ID or link with Roblox; empty text means "unlink". Call before `update`.
    pub fn resolve_place(text: &str) -> Result<Option<Place>, ResolveError> {
        match text.trim() {
            "" => Ok(None),
            text => studio::resolve_place(text).map(Some),
        }
    }

    /// Apply changes to one project after checking all of them.
    pub fn update(self: &Arc<Self>, name: &str, update: Update) -> Result<ProjectView, String> {
        let mut state = self.lock();
        let project = state.config.project(name).cloned().ok_or_else(|| missing(name))?;
        let linked = update.place.unwrap_or(project.place);
        if update.auto_connect == Some(true) && linked.is_none() {
            return Err("Auto-connect needs a linked place.".into());
        }
        let new_name = update.name.as_deref().map(str::trim).unwrap_or(name).to_string();
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
        let mut config = state.config.clone();
        let p = config.project_mut(name).ok_or_else(|| missing(name))?;
        p.port = update.port.unwrap_or(p.port);
        p.autostart = update.autostart.unwrap_or(p.autostart);
        p.auto_connect = update.auto_connect.unwrap_or(p.auto_connect) && linked.is_some();
        p.place = linked;
        p.name = new_name.clone();
        p.sourcemap = update.sourcemap.unwrap_or(p.sourcemap);
        config::save(&config)?;
        state.config = config;
        if new_name != name {
            // Logs and the last seen state follow the project to its new name.
            if let Some(logs) = state.logs.remove(name) {
                state.logs.insert(new_name.clone(), logs);
            }
            if let Some(seen) = state.states.remove(name) {
                state.states.insert(new_name.clone(), seen);
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
        if view.place.is_some() && view.experience.is_none() {
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
                let linked = state.config.projects.iter().filter_map(|p| p.place).map(|place| place.universe_id);
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

    /// Stop the project's server if it runs, then drop the project. Its files are kept.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let (busy, port) = {
            let state = self.lock();
            (state.busy(name), state.port(name)?)
        };
        if busy || port_in_use(port) {
            self.stop(name);
        }
        let mut state = self.lock();
        let mut config = state.config.clone();
        config.projects.retain(|p| p.name != name);
        config::save(&config)?;
        state.config = config;
        state.logs.remove(name);
        state.states.remove(name);
        drop(state);
        self.changed();
        Ok(())
    }

    pub fn open_studio(&self, name: &str) -> Result<Place, String> {
        let place = self.lock().config.project(name).ok_or_else(|| missing(name))?.place;
        let place = place.ok_or_else(|| format!("{name} has no linked place. Set one in Rojo Manager."))?;
        studio::open_place(&place).map_err(|e| format!("Could not open Roblox Studio: {e}"))?;
        Ok(place)
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

    /// Check for a newer release when enabled or forced. With `install`, a newer release is installed
    /// (restarting into it), and otherwise an outdated Studio bridge is rebuilt.
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
            } else if install
                && engine.bridge_update().is_some()
                && let Err(error) = engine.run_tool_job(Job::Bridge { tool: "Rojo".into() })
            {
                engine.notice(error);
            }
        });
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
            if !path.is_absolute() || !path.is_dir() {
                return Err(ControlError::new(format!("No folder at {}. Pass an absolute path.", path.display()), 404));
            }
            let names = engine.add_projects(&path).map_err(|e| ControlError::new(e, 409))?;
            let added: Vec<Value> = names.iter().filter_map(|name| engine.agent_view(name)).collect();
            return Ok(json!({ "added": added }));
        }
        let name = project.ok_or_else(|| ControlError::bad("Pass a project"))?;
        let Some(view) = engine.project(name) else {
            return Err(ControlError::new(missing(name), 404));
        };
        let reply = |name: &str| Ok(json!({ "project": engine.agent_view(name) }));
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
                reply(name)
            }
            "stop" => {
                engine.stop(name);
                engine.notice(format!("{name}: stop requested by an agent"));
                reply(name)
            }
            "restart" => {
                engine.restart(name).map_err(|e| ControlError::new(e, 409))?;
                engine.notice(format!("{name}: restart requested by an agent"));
                reply(name)
            }
            "open-studio" => {
                let status = if view.place.is_none() { 409 } else { 500 };
                engine.open_studio(name).map_err(|e| ControlError::new(e, status))?;
                engine.notice(format!("{name}: open studio requested by an agent"));
                reply(name)
            }
            _ => Err(ControlError::new(format!("Unknown action {action}"), 404)),
        }
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

    fn poll_bridge(&self, id: u64, poll: BridgePoll) -> Value {
        let offer = self.0.report_bridge(id, poll.place_id, poll.port, poll.rojo);
        let result = poll.result.map(|result| {
            let outcome = match result.ok {
                true => Ok(result.value),
                false => Err(result.value.as_str().unwrap_or("The command failed in Studio").to_string()),
            };
            (result.id, outcome)
        });
        let command = self.0.link.poll(id, result, BRIDGE_HOLD);
        json!({ "port": offer.map(|(port, _)| port), "accept": offer.is_some_and(|(_, accept)| accept), "command": command })
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
                "Pass action status, disconnect, accept, abort, connect (with port), luau (with code), or output",
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

/// Read an agent's update body (name, port, autostart, autoConnect, place, sourcemap) or link-place body
/// (place). The place text comes back separately, still to be looked up.
fn parse_update(action: &str, body: &Map<String, Value>) -> Result<(Update, Option<String>), ControlError> {
    const FIELDS: &str = "name, port, autostart, autoConnect, place, sourcemap";
    if action == "link-place" {
        let place = body.get("place").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
        let place = place.ok_or_else(|| ControlError::bad("Pass a place ID or link"))?;
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
        engine.report_bridge(1, 100, 0, json!({"bridge": OUTPUT_BRIDGE}));
        engine.report_bridge(2, 200, 0, json!({"bridge": OUTPUT_BRIDGE - 1}));
        let line = |text: &str, kind| crate::control::OutputReport { text: text.into(), kind };
        engine.report_output(StudioOutput {
            place_id: 100,
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
        assert_eq!(all["next"], 3);
        assert_eq!(all["lines"][0], json!({"context": "server", "kind": "output", "text": "hi"}));
        assert_eq!(texts(read(0, OutputKind::Warning, 100)), ["careful", "boom"]);
        assert_eq!(texts(read(1, OutputKind::Output, 1)), ["boom"]);
        assert_eq!(read(3, OutputKind::Output, 100), json!({"lines": [], "next": 3}));
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
    fn project_paths_compare_like_windows() {
        assert!(same_path("C:\\Games\\A\\", "c:\\games\\a"));
        assert!(!same_path("C:\\Games\\A", "C:\\Games\\AB"));
    }
}
