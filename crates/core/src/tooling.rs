//! Roblox tool discovery and explicit, user-triggered installation jobs.
//!
//! Project installs use existing manifests. Settings installs target Rokit's global manifest and
//! never rewrite a game's pins. Jobs run in the ToolInstaller console so trust prompts stay
//! interactive; `execute_job` prints progress to stdout.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::https::{self, HttpsError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Tool {
    pub name: &'static str,
    pub executable: &'static str,
    pub repository: &'static str,
    pub description: &'static str,
}

pub const TOOLS: [Tool; 6] = [
    Tool {
        name: "Rojo",
        executable: "rojo",
        repository: "rojo-rbx/rojo",
        description: "Sync projects with Roblox Studio",
    },
    Tool {
        name: "Rokit",
        executable: "rokit",
        repository: "rojo-rbx/rokit",
        description: "Manage pinned project tools",
    },
    Tool {
        name: "Wally",
        executable: "wally",
        repository: "UpliftGames/wally",
        description: "Install a project's Lua packages",
    },
    Tool {
        name: "StyLua",
        executable: "stylua",
        repository: "JohnnyMorganz/StyLua",
        description: "Format Lua and Luau code",
    },
    Tool {
        name: "Selene",
        executable: "selene",
        repository: "Kampfkarren/selene",
        description: "Check Lua and Luau code for mistakes",
    },
    Tool { name: "Lune", executable: "lune", repository: "lune-org/lune", description: "Run standalone Luau scripts" },
];

/// The TOOLS entry with this exact name.
pub fn tool(name: &str) -> Option<&'static Tool> {
    TOOLS.iter().find(|tool| tool.name == name)
}

pub fn rokit_home() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join(".rokit")
}

fn home() -> Result<PathBuf, String> {
    std::env::home_dir().ok_or_else(|| "Could not find your home folder".to_string())
}

/// Rokit's bin first (it may be newer than this process's PATH), then PATH. `name` is a TOOLS
/// name (e.g. "Rojo") or, for others like "Aftman", the lowercase executable name.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let executable = tool(name).map_or_else(|| name.to_lowercase(), |tool| tool.executable.to_string());
    let candidate = rokit_home().join("bin").join(format!("{executable}{}", std::env::consts::EXE_SUFFIX));
    if candidate.is_file() {
        return Some(candidate);
    }
    search_path(&executable)
}

/// Like Python's `shutil.which`: each PATH folder in order, trying each PATHEXT extension on Windows.
fn search_path(executable: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        pathext.split(';').filter(|ext| !ext.is_empty()).map(str::to_ascii_lowercase).collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&path)
        .flat_map(|folder| extensions.iter().map(move |ext| folder.join(format!("{executable}{ext}"))))
        .find(|candidate| candidate.is_file())
}

/// The first `major.minor.patch` in `text` that does not start mid-number.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let bytes = text.as_bytes();
    (0..bytes.len())
        .filter(|&i| bytes[i].is_ascii_digit() && (i == 0 || !bytes[i - 1].is_ascii_digit()))
        .find_map(|i| version_prefix(&text[i..]))
}

/// The whole version from `<tool> --version` output, prerelease included: "7.7.0-rc.1" from
/// "Rojo 7.7.0-rc.1". Release tags and plugin builds need this, not just the numbers.
pub fn full_version(text: &str) -> Option<&str> {
    text.split_whitespace().map(|word| word.trim_start_matches('v')).find(|word| version_prefix(word).is_some())
}

fn version_prefix(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = [0; 3];
    let mut rest = text;
    for (index, part) in parts.iter_mut().enumerate() {
        if index > 0 {
            rest = rest.strip_prefix('.')?;
        }
        let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        *part = rest.get(..end).filter(|digits| !digits.is_empty())?.parse().ok()?;
        rest = &rest[end..];
    }
    Some((parts[0], parts[1], parts[2]))
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(15);

/// `<tool> --version` (hidden console, 15 s timeout), run in `cwd` so pinned versions apply.
pub fn tool_version(name: &str, cwd: Option<&Path>) -> Result<String, String> {
    let executable = find_tool(name).ok_or_else(|| format!("{name} is not installed"))?;
    let cwd = match cwd {
        Some(cwd) => cwd.to_path_buf(),
        None => home()?,
    };
    let mut command = Command::new(executable);
    command.arg("--version").current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, crate::CREATE_NO_WINDOW);
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + VERSION_TIMEOUT;
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if Instant::now() >= deadline {
            // Not reading the pipes: a tool the shim started may still hold them open.
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{name} did not respond within 15 seconds"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if stderr.is_empty() { stdout.trim() } else { stderr.trim() };
        return Err(if detail.is_empty() { format!("{name} could not start") } else { detail.to_string() });
    }
    if parse_version(&stdout).is_none() {
        return Err(format!("Could not read {name}'s version"));
    }
    Ok(stdout.trim().to_string())
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
    /// Read from github.com's /releases/latest redirect because the API quota ran out.
    #[serde(skip)]
    pub from_release_page: bool,
}

const API_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// The latest stable GitHub release; falls back to the release page redirect on 403/429.
pub fn latest_release(repository: &str) -> Result<Release, String> {
    fetch_latest_release(repository).map_err(|error| error.to_string())
}

/// `latest_release` keeping the HTTP status, so a private repository can be told apart from being
/// offline. An invalid or unstable release is reported as `Transport`.
pub(crate) fn fetch_latest_release(repository: &str) -> Result<Release, HttpsError> {
    let api = format!("https://api.github.com/repos/{repository}/releases/latest");
    match https::get_json(&api, "application/vnd.github+json", API_TIMEOUT) {
        Ok(value) => parse_release(value).map_err(HttpsError::Transport),
        // GitHub's public latest-release redirect is independent of the API quota.
        Err(HttpsError::Status(403 | 429)) => {
            let page = https::final_url(&format!("https://github.com/{repository}/releases/latest"), API_TIMEOUT)?;
            release_from_page(repository, &page).map_err(HttpsError::Transport)
        }
        Err(error) => Err(error),
    }
}

/// A release from GitHub's API; drafts and prereleases are refused.
fn parse_release(value: Value) -> Result<Release, String> {
    let invalid = || "GitHub returned an invalid release".to_string();
    if !value.get("tag_name").is_some_and(Value::is_string) {
        return Err(invalid());
    }
    let flagged = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
    if flagged("draft") || flagged("prerelease") {
        return Err("No stable release was returned".into());
    }
    serde_json::from_value(value).map_err(|_| invalid())
}

/// The release named by the URL github.com/<repository>/releases/latest redirected to.
fn release_from_page(repository: &str, url: &str) -> Result<Release, String> {
    let unexpected = || "Could not locate GitHub's latest stable release".to_string();
    let (host, path) = url.strip_prefix("https://").and_then(|rest| rest.split_once('/')).ok_or_else(unexpected)?;
    let path = path.split(['?', '#']).next().unwrap_or_default();
    let prefix = format!("{repository}/releases/tag/");
    let matches = path.get(..prefix.len()).is_some_and(|head| head.eq_ignore_ascii_case(&prefix));
    if !host.eq_ignore_ascii_case("github.com") || !matches {
        return Err(unexpected());
    }
    Ok(Release { tag_name: percent_decode(&path[prefix.len()..]), assets: Vec::new(), from_release_page: true })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = bytes
            .get(index + 1..index + 3)
            .filter(|hex| bytes[index] == b'%' && hex.iter().all(u8::is_ascii_hexdigit))
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RojoState {
    Checking,
    Missing,
    Broken,
    Unchecked,
    Error,
    Outdated,
    Current,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RojoStatus {
    pub state: RojoState,
    pub message: String,
    /// Button label for the suggested fix ("Install Rojo", "Update Rojo", "Retry"), if any.
    pub action: Option<String>,
}

fn rojo_status(state: RojoState, message: String, action: Option<&str>) -> RojoStatus {
    RojoStatus { state, message, action: action.map(String::from) }
}

pub fn inspect_rojo(check_updates: bool) -> RojoStatus {
    if find_tool("Rojo").is_none() {
        return rojo_status(RojoState::Missing, "Rojo is not installed.".into(), Some("Install Rojo"));
    }
    let installed = match tool_version("Rojo", None) {
        Ok(installed) => installed,
        Err(error) => {
            return rojo_status(RojoState::Broken, format!("Rojo could not start: {error}"), Some("Install Rojo"));
        }
    };
    if !check_updates {
        return rojo_status(RojoState::Unchecked, format!("{installed}. Update checks are disabled."), None);
    }
    let latest = latest_release(TOOLS[0].repository).ok().map(|release| release.tag_name);
    compare_rojo(&installed, latest.as_deref())
}

/// Status of an installed Rojo (its `--version` output) against the latest tag, when it is known.
/// A prerelease of the latest version counts as outdated.
fn compare_rojo(installed_text: &str, latest_tag: Option<&str>) -> RojoStatus {
    let versions = latest_tag.and_then(|tag| Some((tag, parse_version(tag)?, parse_version(installed_text)?)));
    let Some((tag, latest, installed)) = versions else {
        return rojo_status(RojoState::Error, format!("{installed_text}. Could not check for updates."), Some("Retry"));
    };
    if latest > installed || (latest == installed && installed_text.contains('-')) {
        let message = format!("{installed_text} is installed. {} is available.", tag.trim_start_matches('v'));
        return rojo_status(RojoState::Outdated, message, Some("Update Rojo"));
    }
    rojo_status(RojoState::Current, format!("{installed_text} is up to date."), None)
}

/// Run a console program in `cwd` with this console's input and output, after echoing it.
/// Arguments are passed as they are, never through a shell.
fn run_command<S: AsRef<OsStr>>(program: &Path, args: &[S], cwd: &Path) -> Result<(), String> {
    let line = std::iter::once(program.as_os_str())
        .chain(args.iter().map(<S as AsRef<OsStr>>::as_ref))
        .map(|arg| quote_arg(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    println!("\n> {line}");
    let status =
        Command::new(program).args(args).current_dir(cwd).status().map_err(|e| format!("Could not run {line}: {e}"))?;
    if !status.success() {
        return Err(format!("Command '{line}' failed with {status}."));
    }
    Ok(())
}

/// An argument as it would be written on a Windows command line, for display.
fn quote_arg(arg: &str) -> String {
    if arg.is_empty() || arg.contains([' ', '\t', '"']) {
        format!("\"{}\"", arg.replace('"', "\\\""))
    } else {
        arg.to_string()
    }
}

const ROKIT_DOWNLOADS: &str = "https://github.com/rojo-rbx/rokit/releases/download/";

/// Rokit's path, downloading it from its official release and self-installing it when missing.
fn ensure_rokit() -> Result<PathBuf, String> {
    if let Some(existing) = find_tool("Rokit") {
        return Ok(existing);
    }
    if !cfg!(all(windows, target_arch = "x86_64")) {
        return Err("Install Rokit from https://github.com/rojo-rbx/rokit, then retry".into());
    }
    let asset = rokit_asset(&latest_release(TOOLS[1].repository)?)?;
    println!("Downloading Rokit from its official GitHub release...");
    let folder = tempfile::Builder::new().prefix("rojo-manager-rokit-").tempdir().map_err(|e| e.to_string())?;
    let archive = folder.path().join("rokit.zip");
    https::download(&asset.browser_download_url, &archive, DOWNLOAD_TIMEOUT).map_err(|e| e.to_string())?;
    let executable = extract_rokit(&archive, asset.digest.as_deref(), folder.path())?;
    run_command(&executable, &["self-install"], folder.path())?;
    drop(folder);
    find_tool("Rokit").ok_or_else(|| "Rokit installed but could not be located. Restart the manager and retry.".into())
}

/// The release's Windows x64 archive. Without API assets (read from the release page), the name is
/// the one documented by Rokit's official Windows installer script.
fn rokit_asset(release: &Release) -> Result<Asset, String> {
    let mut assets: Vec<Asset> =
        release.assets.iter().filter(|asset| asset.name.ends_with("windows-x86_64.zip")).cloned().collect();
    if assets.is_empty() && release.from_release_page {
        let tag = &release.tag_name;
        let version = tag.strip_prefix('v').unwrap_or(tag);
        let parts: Vec<&str> = version.split('.').collect();
        if parts.len() != 3 || !parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())) {
            return Err("Unexpected Rokit stable release tag".into());
        }
        let name = format!("rokit-{version}-windows-x86_64.zip");
        assets.push(Asset { browser_download_url: format!("{ROKIT_DOWNLOADS}{tag}/{name}"), name, digest: None });
    }
    let [asset] = <[Asset; 1]>::try_from(assets).map_err(|_| "Could not find the Windows Rokit release".to_string())?;
    if !asset.browser_download_url.starts_with(ROKIT_DOWNLOADS) {
        return Err("Unexpected Rokit download location".into());
    }
    Ok(asset)
}

/// Check the archive against GitHub's "sha256:<hex>" digest, when there is one, then copy its one
/// rokit.exe into `folder`. Nothing else in the archive is written.
fn extract_rokit(archive: &Path, digest: Option<&str>, folder: &Path) -> Result<PathBuf, String> {
    let data = fs::read(archive).map_err(|e| e.to_string())?;
    if let Some(expected) = digest.and_then(|digest| digest.strip_prefix("sha256:")) {
        let mut actual = String::new();
        for byte in Sha256::digest(&data).iter() {
            let _ = write!(actual, "{byte:02x}");
        }
        if !actual.eq_ignore_ascii_case(expected) {
            return Err("The Rokit download checksum did not match".into());
        }
    }
    let mut zipped = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| e.to_string())?;
    let binaries: Vec<usize> = (0..zipped.len())
        .filter(|&index| {
            let name = zipped.by_index(index).ok().and_then(|file| file.name().ok().map(|name| name.into_owned()));
            name.is_some_and(|name| name.rsplit(['/', '\\']).next() == Some("rokit.exe"))
        })
        .collect();
    let [index] =
        <[usize; 1]>::try_from(binaries).map_err(|_| "The Rokit archive did not contain one rokit.exe".to_string())?;
    let mut source = zipped.by_index(index).map_err(|e| e.to_string())?;
    let executable = folder.join("rokit.exe");
    let mut output = fs::File::create(&executable).map_err(|e| e.to_string())?;
    std::io::copy(&mut source, &mut output).map_err(|e| e.to_string())?;
    Ok(executable)
}

/// Rokit arguments that install or update one official tool in Rokit's global manifest, given that
/// manifest's text (empty when there is none). `repair` reinstalls a configured tool that is broken.
/// An alias taken by a different tool is never overwritten.
fn global_tool_command(tool: &Tool, manifest: &str, repair: bool) -> Result<Vec<String>, String> {
    let manifest: toml::Table =
        manifest.parse().map_err(|error| format!("Could not read Rokit's global manifest: {error}"))?;
    let configured = manifest.get("tools").and_then(toml::Value::as_table);
    let official = |spec: &toml::Value| {
        spec.as_str()
            .and_then(|spec| spec.split('@').next())
            .is_some_and(|repo| repo.eq_ignore_ascii_case(tool.repository))
    };
    if let Some((alias, _)) = configured.into_iter().flatten().find(|(_, spec)| official(spec)) {
        return Ok(if repair {
            // Reinstall only this already-configured official tool, not the entire toolchain.
            vec!["add".into(), "--global".into(), "--force".into(), tool.repository.into(), alias.clone()]
        } else {
            vec!["update".into(), "--global".into(), tool.repository.into()]
        });
    }
    if let Some((alias, spec)) =
        configured.into_iter().flatten().find(|(alias, _)| alias.eq_ignore_ascii_case(tool.executable))
    {
        let spec = spec.as_str().unwrap_or(spec.type_str());
        return Err(format!("The global {alias} alias belongs to a different tool: {spec}"));
    }
    Ok(vec!["add".into(), "--global".into(), tool.repository.into(), tool.executable.into()])
}

fn install_global(name: &str) -> Result<String, String> {
    let tool = tool(name).ok_or("Unknown tool")?;
    let was_installed = find_tool(name).is_some();
    let rokit = ensure_rokit()?;
    let home = home()?;
    if name == "Rokit" {
        if was_installed {
            run_command(&rokit, &["self-update"], &home)?;
        }
    } else {
        let repair = tool_version(name, None).is_err();
        let manifest = rokit_home().join("rokit.toml");
        let text =
            if manifest.is_file() { fs::read_to_string(&manifest).map_err(|e| e.to_string())? } else { String::new() };
        run_command(&rokit, &global_tool_command(tool, &text, repair)?, &home)?;
    }
    // Verify the executable the manager will actually discover, including PATH conflicts.
    let version = tool_version(name, None)?;
    Ok(format!("{version} is available. Project version pins are unchanged."))
}

/// "Rokit" for rokit.toml, else "Aftman" for aftman.toml.
pub fn project_manager(path: &Path) -> Option<&'static str> {
    if path.join("rokit.toml").is_file() {
        Some("Rokit")
    } else if path.join("aftman.toml").is_file() {
        Some("Aftman")
    } else {
        None
    }
}

/// Installers that apply to a project: "Wally" when it has wally.toml, then its project_manager.
pub fn project_badges(path: &Path) -> Vec<&'static str> {
    let wally = path.join("wally.toml").is_file().then_some("Wally");
    wally.into_iter().chain(project_manager(path)).collect()
}

/// `install` with the project's tool manager. Rokit is preferred; it reads Aftman manifests too.
fn install_project_tools(path: &Path) -> Result<(), String> {
    let manager = project_manager(path).ok_or("This project has no rokit.toml or aftman.toml")?;
    let found = find_tool("Rokit").or_else(|| if manager == "Aftman" { find_tool("Aftman") } else { None });
    let executable = match found {
        Some(executable) => executable,
        None => ensure_rokit()?,
    };
    run_command(&executable, &["install"], path)
}

fn install_project(name: &str, path: &Path) -> Result<String, String> {
    if !path.is_dir() {
        return Err("The project folder no longer exists".into());
    }
    if name == "Rokit" || name == "Aftman" {
        install_project_tools(path)?;
        return Ok("Installed the project's pinned tools.".into());
    }
    if name != "Wally" || !path.join("wally.toml").is_file() {
        return Err("This project has no wally.toml".into());
    }
    if project_manager(path).is_some() {
        install_project_tools(path)?;
    }
    if find_tool("Wally").is_none() {
        install_global("Wally")?;
    }
    let wally = find_tool("Wally").ok_or("Wally could not be located after installation")?;
    run_command(&wally, &["install"], path)?;
    Ok("Installed the project's Wally packages.".into())
}

pub const BRIDGE_PLUGIN: &str = "RojoManagerRojo.rbxm";
/// What `rojo plugin install` writes.
const OFFICIAL_PLUGIN: &str = "RojoManagedPlugin.rbxm";
/// Rojo's Creator Store asset; see rojo's plugin/UploadDetails.json.
const CREATOR_STORE_ROJO: &str = "13916111004";
const BRIDGE_SCRIPT: &str = include_str!("../../../rojo_bridge/RojoManagerBridge.luau");
const PATCH_SCRIPT: &str = include_str!("../../../rojo_bridge/patch_rojo_plugin.luau");

/// %LOCALAPPDATA%\Roblox, which holds Studio's plugin folders.
fn roblox_folder() -> PathBuf {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::home_dir().unwrap_or_default().join("AppData").join("Local"));
    local.join("Roblox")
}

/// The Rojo plugin Rojo Manager installed in Studio's local Plugins folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum StudioPlugin {
    Missing,
    /// `rojo plugin install`'s copy, without the bridge.
    Official,
    /// The official plugin with the Rojo Manager bridge. `built` is None when an older Rojo
    /// Manager installed it without recording how.
    Bridge {
        built: Option<BridgeBuild>,
    },
}

/// What a bridge install was built from, recorded beside config.json.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeBuild {
    /// The Rojo version whose plugin it patched ("7.7.0").
    pub rojo: String,
    /// The bridge module's VERSION.
    pub bridge: u32,
}

/// The `VERSION` in RojoManagerBridge.luau; raise both together.
pub const BRIDGE_VERSION: u32 = 6;

fn bridge_record() -> PathBuf {
    crate::config::data_dir().join("bridge.json")
}

pub fn studio_plugin() -> StudioPlugin {
    let plugins = roblox_folder().join("Plugins");
    if plugins.join(BRIDGE_PLUGIN).is_file() {
        let built = fs::read(bridge_record()).ok().and_then(|data| serde_json::from_slice(&data).ok());
        StudioPlugin::Bridge { built }
    } else if plugins.join(OFFICIAL_PLUGIN).is_file() {
        StudioPlugin::Official
    } else {
        StudioPlugin::Missing
    }
}

pub fn creator_store_rojo_installed() -> bool {
    let Ok(entries) = fs::read_dir(roblox_folder()) else { return false };
    entries.flatten().any(|entry| entry.path().join("InstalledPlugins").join(CREATOR_STORE_ROJO).exists())
}

/// Download the official Rojo plugin matching the installed Rojo, add the Rojo Manager bridge with
/// Lune, and install it as a local Studio plugin. Rojo is never redistributed.
fn install_rojo_bridge() -> Result<String, String> {
    if find_tool("Rojo").is_none() {
        install_global("Rojo")?;
    }
    let output = tool_version("Rojo", None)?;
    let version = full_version(&output).ok_or("Could not read the installed Rojo version")?;
    if find_tool("Lune").is_none() {
        install_global("Lune")?;
    }
    let lune = find_tool("Lune").ok_or("Lune could not be located after installation")?;
    let tag = format!("v{version}");
    let temporary = tempfile::Builder::new().prefix("rojo-manager-bridge-").tempdir().map_err(|e| e.to_string())?;
    let folder = temporary.path();
    let (script, bridge) = (folder.join("patch_rojo_plugin.luau"), folder.join("RojoManagerBridge.luau"));
    fs::write(&script, PATCH_SCRIPT).map_err(|e| e.to_string())?;
    fs::write(&bridge, BRIDGE_SCRIPT).map_err(|e| e.to_string())?;
    let (original, patched) = (folder.join("Rojo.rbxm"), folder.join(BRIDGE_PLUGIN));
    let url = format!("https://github.com/rojo-rbx/rojo/releases/download/{tag}/Rojo.rbxm");
    println!("Downloading the official Rojo {tag} plugin from {url}");
    https::download(&url, &original, DOWNLOAD_TIMEOUT).map_err(|e| e.to_string())?;
    let args = [OsStr::new("run"), script.as_os_str(), original.as_os_str(), bridge.as_os_str(), patched.as_os_str()];
    run_command(&lune, &args, folder)?;
    let plugins = roblox_folder().join("Plugins");
    fs::create_dir_all(&plugins).map_err(|e| e.to_string())?;
    fs::copy(&patched, plugins.join(BRIDGE_PLUGIN)).map_err(|e| e.to_string())?;
    // Only used to tell when the bridge needs rebuilding; the plugin works without it.
    let built = BridgeBuild { rojo: version.to_string(), bridge: BRIDGE_VERSION };
    let _ = fs::write(bridge_record(), serde_json::to_vec(&built).map_err(|e| e.to_string())?);
    // The unpatched copy from "rojo plugin install" would load as a second Rojo.
    if let Err(error) = fs::remove_file(plugins.join(OFFICIAL_PLUGIN))
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.to_string());
    }
    let mut message = format!("Installed Rojo {tag} with the Rojo Manager bridge. Restart Studio to load it.");
    if creator_store_rojo_installed() {
        message.push_str(
            " Disable the Creator Store Rojo plugin in Studio (Plugins > Manage Plugins) so only one Rojo runs.",
        );
    }
    Ok(message)
}

/// A ToolInstaller request; the JSON matches 0.5's {"operation", "tool", "path"}.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "lowercase")]
pub enum Job {
    /// Install or update a tool in Rokit's global manifest.
    Global { tool: String },
    /// Install a project's Wally packages or its Rokit/Aftman pinned tools.
    Project { tool: String, path: String },
    /// Install Rojo's Studio plugin with the Rojo Manager bridge (tool is "Rojo").
    Bridge { tool: String },
    /// Install Rojo's Studio plugin (keeps the bridge when it is installed).
    Plugin { tool: String },
}

/// Run a job in the console, printing progress; returns the message to show when it is done.
pub fn execute_job(job: &Job) -> Result<String, String> {
    match job {
        // The bridge is built from one Rojo version, so updating Rojo rebuilds it to match.
        Job::Global { tool: name } if name == "Rojo" && matches!(studio_plugin(), StudioPlugin::Bridge { .. }) => {
            let updated = install_global(name)?;
            match install_rojo_bridge() {
                Ok(rebuilt) => Ok(format!("{updated} {rebuilt}")),
                Err(error) => Err(format!("{updated} The bridge could not be rebuilt: {error}")),
            }
        }
        Job::Global { tool: name } if tool(name).is_some() => install_global(name),
        Job::Project { tool, path } if matches!(tool.as_str(), "Wally" | "Rokit" | "Aftman") => {
            let path = Path::new(path);
            if !path.is_absolute() {
                return Err("A project requires an absolute folder path".into());
            }
            install_project(tool, path)
        }
        Job::Bridge { tool } if tool == "Rojo" => install_rojo_bridge(),
        Job::Plugin { tool } if tool == "Rojo" => {
            if matches!(studio_plugin(), StudioPlugin::Bridge { .. }) {
                return install_rojo_bridge(); // keep a single Rojo plugin, with the bridge
            }
            if find_tool("Rojo").is_none() {
                install_global("Rojo")?;
            }
            let rojo = find_tool("Rojo").ok_or("Rojo could not be located")?;
            run_command(&rojo, &["plugin", "install"], &home()?)?;
            Ok("Installed the Rojo Studio plugin. Restart Studio to load it.".into())
        }
        _ => Err("Unknown installation action".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;

    fn rojo() -> &'static Tool {
        &TOOLS[0]
    }

    #[test]
    fn versions_are_read_from_tool_output() {
        assert_eq!(parse_version("Rojo 7.4.4"), Some((7, 4, 4)));
        assert_eq!(parse_version("rojo 7.7.0-rc.1\n"), Some((7, 7, 0)));
        assert_eq!(parse_version("v1.2.x then 3.4.5"), Some((3, 4, 5)));
        assert_eq!(parse_version("version 1.2"), None);
        assert_eq!(full_version("rojo 7.7.0-rc.1\n"), Some("7.7.0-rc.1"));
        assert_eq!(full_version("v1.2.x then v3.4.5"), Some("3.4.5"));
        assert_eq!(full_version("7.4.4"), Some("7.4.4"));
    }

    #[test]
    fn global_commands_target_one_official_tool() {
        let command = |tool: &Tool, manifest: &str| global_tool_command(tool, manifest, false).unwrap();
        assert_eq!(command(rojo(), ""), ["add", "--global", "rojo-rbx/rojo", "rojo"]);
        let manifest = "[tools]\nrojo = \"rojo-rbx/rojo@7.4.4\"\nStyLua = \"JohnnyMorganz/StyLua@2.1.0\"\n";
        assert_eq!(command(rojo(), manifest), ["update", "--global", "rojo-rbx/rojo"]);
        assert_eq!(command(&TOOLS[3], manifest), ["update", "--global", "JohnnyMorganz/StyLua"]);
    }

    #[test]
    fn an_alias_from_another_repository_is_not_overwritten() {
        let error = global_tool_command(rojo(), "[tools]\nrojo = \"other/rojo@1.0.0\"\n", false).unwrap_err();
        assert!(error.contains("different tool: other/rojo@1.0.0"), "{error}");
    }

    #[test]
    fn repair_targets_only_the_broken_official_tool() {
        let command = global_tool_command(rojo(), "[tools]\nrojo = \"rojo-rbx/rojo@7.4.4\"\n", true).unwrap();
        assert_eq!(command, ["add", "--global", "--force", "rojo-rbx/rojo", "rojo"]);
    }

    #[test]
    fn rojo_release_states_include_offline_and_prerelease() {
        let outdated = compare_rojo("Rojo 7.4.4", Some("v7.7.0"));
        assert_eq!((outdated.state, outdated.action.as_deref()), (RojoState::Outdated, Some("Update Rojo")));
        assert_eq!(outdated.message, "Rojo 7.4.4 is installed. 7.7.0 is available.");
        assert_eq!(compare_rojo("Rojo 7.7.0", Some("v7.7.0")).state, RojoState::Current);
        assert_eq!(compare_rojo("Rojo 7.7.0-rc.1", Some("v7.7.0")).state, RojoState::Outdated);
        assert_eq!(compare_rojo("Rojo 7.8.0", Some("v7.7.0")).state, RojoState::Current);
        assert_eq!(compare_rojo("Rojo 7.7.0", None).action.as_deref(), Some("Retry"));
    }

    #[test]
    fn only_stable_api_releases_are_accepted() {
        let release = parse_release(json!({"tag_name": "v7.7.0", "assets": [
            {"name": "a.zip", "browser_download_url": "https://x", "digest": null}]}))
        .unwrap();
        assert_eq!((release.tag_name.as_str(), release.assets.len(), release.from_release_page), ("v7.7.0", 1, false));
        for value in [json!({"tag_name": "v1.0.0", "draft": true}), json!({"tag_name": "v1.0.0", "prerelease": true})] {
            assert_eq!(parse_release(value).unwrap_err(), "No stable release was returned");
        }
        assert!(parse_release(json!([])).is_err());
    }

    #[test]
    fn rate_limited_lookups_read_the_official_release_redirect() {
        let release =
            release_from_page("rojo-rbx/rojo", "https://github.com/rojo-rbx/rojo/releases/tag/v7.7.0").unwrap();
        assert_eq!((release.tag_name.as_str(), release.from_release_page), ("v7.7.0", true));
        assert_eq!(
            release_from_page("a/b", "https://github.com/A/B/releases/tag/v1%2B2?x=1").unwrap().tag_name,
            "v1+2"
        );
        for url in
            ["https://github.com/rojo-rbx/rojo/releases", "https://example.com/rojo-rbx/rojo/releases/tag/v1.0.0"]
        {
            assert!(release_from_page("rojo-rbx/rojo", url).is_err(), "{url}");
        }
    }

    #[test]
    fn rokit_downloads_only_its_official_windows_archive() {
        let asset = |name: &str, url: &str| Asset { name: name.into(), browser_download_url: url.into(), digest: None };
        let official = format!("{ROKIT_DOWNLOADS}v1.2.0/rokit-1.2.0-windows-x86_64.zip");
        let release = |assets, from_release_page| Release { tag_name: "v1.2.0".into(), assets, from_release_page };
        let api = release(
            vec![asset("rokit-1.2.0-linux-x86_64.zip", ""), asset("rokit-1.2.0-windows-x86_64.zip", &official)],
            false,
        );
        assert_eq!(rokit_asset(&api).unwrap().browser_download_url, official);
        assert_eq!(rokit_asset(&release(Vec::new(), true)).unwrap().browser_download_url, official);
        assert!(rokit_asset(&release(Vec::new(), false)).is_err());
        let moved = release(vec![asset("rokit-1.2.0-windows-x86_64.zip", "https://example.com/rokit.zip")], false);
        assert_eq!(rokit_asset(&moved).unwrap_err(), "Unexpected Rokit download location");
        let odd = Release { tag_name: "v1.2.0/../x".into(), assets: Vec::new(), from_release_page: true };
        assert_eq!(rokit_asset(&odd).unwrap_err(), "Unexpected Rokit stable release tag");
    }

    #[test]
    fn rokit_bootstrap_extracts_only_the_binary_and_checks_its_digest() {
        let folder = tempfile::tempdir().unwrap();
        let archive = folder.path().join("rokit.zip");
        let mut zipped = zip::ZipWriter::new(fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zipped.start_file("nested/rokit.exe", options).unwrap();
        zipped.write_all(b"fake test binary").unwrap();
        zipped.start_file("README.md", options).unwrap();
        zipped.write_all(b"must never be extracted").unwrap();
        zipped.finish().unwrap();
        let mut digest = String::from("sha256:");
        for byte in Sha256::digest(fs::read(&archive).unwrap()).iter() {
            write!(digest, "{byte:02x}").unwrap();
        }

        let executable = extract_rokit(&archive, Some(&digest), folder.path()).unwrap();
        assert_eq!(fs::read(&executable).unwrap(), b"fake test binary");
        let mut names: Vec<_> = fs::read_dir(folder.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        assert_eq!(names, ["rokit.exe", "rokit.zip"]);
        fs::remove_file(&executable).unwrap();
        let error = extract_rokit(&archive, Some("sha256:bad"), folder.path()).unwrap_err();
        assert_eq!(error, "The Rokit download checksum did not match");
        assert!(!executable.exists());
    }

    #[test]
    fn badges_list_wally_then_the_preferred_tool_manager() {
        let folder = tempfile::tempdir().unwrap();
        assert!(project_badges(folder.path()).is_empty());
        for file in ["wally.toml", "aftman.toml"] {
            fs::write(folder.path().join(file), "").unwrap();
        }
        assert_eq!(project_badges(folder.path()), ["Wally", "Aftman"]);
        fs::write(folder.path().join("rokit.toml"), "").unwrap();
        assert_eq!(project_badges(folder.path()), ["Wally", "Rokit"]);
    }

    #[test]
    fn the_bridge_version_matches_the_bridge_module() {
        let declared = format!("local VERSION = {BRIDGE_VERSION}");
        assert!(BRIDGE_SCRIPT.lines().any(|line| line.trim() == declared));
    }

    #[test]
    fn jobs_read_the_v05_request_format() {
        let job: Job =
            serde_json::from_value(json!({"operation": "project", "tool": "Wally", "path": "C:\\Game"})).unwrap();
        assert_eq!(job, Job::Project { tool: "Wally".into(), path: "C:\\Game".into() });
        assert_eq!(
            serde_json::to_value(Job::Plugin { tool: "Rojo".into() }).unwrap(),
            json!({"operation": "plugin", "tool": "Rojo"})
        );
    }

    #[test]
    fn invalid_jobs_are_refused_before_anything_runs() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().to_string_lossy().into_owned();
        let project = |tool: &str, path: &str| execute_job(&Job::Project { tool: tool.into(), path: path.into() });
        assert_eq!(execute_job(&Job::Global { tool: "rojo".into() }).unwrap_err(), "Unknown installation action");
        assert_eq!(execute_job(&Job::Bridge { tool: "Lune".into() }).unwrap_err(), "Unknown installation action");
        assert_eq!(project("Lune", &path).unwrap_err(), "Unknown installation action");
        assert_eq!(project("Wally", "Game").unwrap_err(), "A project requires an absolute folder path");
        let missing = folder.path().join("missing").to_string_lossy().into_owned();
        assert_eq!(project("Wally", &missing).unwrap_err(), "The project folder no longer exists");
        assert_eq!(project("Wally", &path).unwrap_err(), "This project has no wally.toml");
        assert_eq!(project("Rokit", &path).unwrap_err(), "This project has no rokit.toml or aftman.toml");
    }
}
