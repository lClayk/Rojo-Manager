//! Private per-user settings in %LOCALAPPDATA%\RojoManager\config.json on Windows,
//! ~/Library/Application Support/RojoManager/config.json on macOS, or $ROJO_MANAGER_DATA_DIR.
//! The file format is shared with Rojo Manager 0.5, so upgrading keeps every project.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A published Roblox place. Studio needs both IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    pub place_id: u64,
    pub universe_id: u64,
}

/// An unpublished place file that Rojo Manager builds from the project with `rojo build`, kept in
/// the data folder. The bridge names an unpublished place (`game.PlaceId` 0) by its file name, which
/// is how local places are told apart, so each file name is unique.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalPlace {
    pub file: String,
}

impl LocalPlace {
    /// A new, unused file name for a project's place, such as "MyGame-1a2b3c4d.rbxl".
    pub fn new(project: &str) -> LocalPlace {
        let stem: String =
            project.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).take(40).collect();
        let mut bytes = [0u8; 4];
        let _ = getrandom::fill(&mut bytes);
        let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        LocalPlace { file: format!("{stem}-{suffix}.rbxl") }
    }

    pub fn path(&self) -> PathBuf {
        data_dir().join("places").join(&self.file)
    }
}

/// What a project opens in Studio: a published place, or a local place file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PlaceLink {
    Published(Place),
    Local(LocalPlace),
}

impl PlaceLink {
    pub fn published(&self) -> Option<Place> {
        match self {
            PlaceLink::Published(place) => Some(*place),
            PlaceLink::Local(_) => None,
        }
    }

    /// Whether this is the place a bridge reports: its `game.PlaceId`, and its file name when that is 0.
    pub fn matches(&self, place_id: u64, place_name: Option<&str>) -> bool {
        match self {
            PlaceLink::Published(place) => place.place_id == place_id,
            PlaceLink::Local(local) => place_id == 0 && place_name == Some(local.file.as_str()),
        }
    }

    /// "place 123", or "local place MyGame-1a2b3c4d.rbxl", for messages.
    pub fn describe(&self) -> String {
        match self {
            PlaceLink::Published(place) => format!("place {}", place.place_id),
            PlaceLink::Local(local) => format!("local place {}", local.file),
        }
    }
}

/// A lane: a project made by `lane create` from a copy of another project (such as a git worktree),
/// with a local place of its own, for parallel work in its own Studio. It is saved like any project
/// and ends when removed, or once its folder is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lane {
    /// The project it was made from.
    pub parent: String,
    /// A label from whoever made it, such as "lane B".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub name: String,
    pub path: String,
    pub project_file: String,
    pub port: u16,
    pub autostart: bool,
    /// Connect Rojo when the linked place opens in Studio. Only stored when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto_connect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<PlaceLink>,
    /// Keep sourcemap.json current with `rojo sourcemap --watch` while the server runs. Only stored when true.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sourcemap: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<Lane>,
    /// Close the place's Studio, and stop the server unless it autostarts, after this many minutes
    /// with nothing happening in that Studio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_close: Option<u32>,
}

/// The longest idle time a project can be given: a week.
pub const MAX_IDLE_CLOSE: u32 = 7 * 24 * 60;

/// App preferences. Field names are the JSON keys agents use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub tray: bool,
    pub start_hidden: bool,
    pub version_check: bool,
    pub app_version_check: bool,
    /// On launch, install a newer app release (restarting into it), or else rebuild an outdated bridge.
    pub auto_update: bool,
    pub show_wally: bool,
    pub show_rokit: bool,
    pub show_aftman: bool,
    /// Color the tray icon by server state, and list it in its tooltip.
    pub tray_status: bool,
    /// A Windows notification when a server fails while the window is closed.
    pub notifications: bool,
    /// Start a server again when it crashes, a few times a minute at most.
    pub restart_crashed: bool,
    /// Accept Rojo's initial sync prompt when the bridge auto-connects.
    pub auto_connect_accept: bool,
    /// List places of one experience under a shared, collapsible header.
    pub group_experiences: bool,
    /// Offer agents get_connected_instances and execute_luau; off when another Studio MCP has them.
    pub studio_tools: bool,
    /// Back up instances that exist only in Studio before Rojo removes them (see backups.rs).
    pub backups: bool,
    /// Let projects close their Studio when it is idle. Off, no Studio is closed for it and no
    /// project can be given a time. Only the user changes this, never an agent.
    pub idle_close: bool,
    /// Delete Studio's auto-recovery copies of a local place together with that place. Off unless
    /// the user turns it on, so a removal never takes what Studio kept to recover from.
    pub remove_recovery: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tray: true,
            start_hidden: false,
            version_check: true,
            app_version_check: true,
            auto_update: false,
            show_wally: true,
            show_rokit: true,
            show_aftman: true,
            tray_status: true,
            notifications: true,
            restart_crashed: true,
            auto_connect_accept: true,
            group_experiences: true,
            studio_tools: true,
            backups: true,
            idle_close: true,
            remove_recovery: false,
        }
    }
}

impl Settings {
    pub const KEYS: [&'static str; 17] = [
        "tray",
        "start_hidden",
        "version_check",
        "app_version_check",
        "auto_update",
        "show_wally",
        "show_rokit",
        "show_aftman",
        "tray_status",
        "notifications",
        "restart_crashed",
        "auto_connect_accept",
        "group_experiences",
        "studio_tools",
        "backups",
        "idle_close",
        "remove_recovery",
    ];

    fn slot(&mut self, key: &str) -> Option<&mut bool> {
        Some(match key {
            "tray" => &mut self.tray,
            "start_hidden" => &mut self.start_hidden,
            "version_check" => &mut self.version_check,
            "app_version_check" => &mut self.app_version_check,
            "auto_update" => &mut self.auto_update,
            "show_wally" => &mut self.show_wally,
            "show_rokit" => &mut self.show_rokit,
            "show_aftman" => &mut self.show_aftman,
            "tray_status" => &mut self.tray_status,
            "notifications" => &mut self.notifications,
            "restart_crashed" => &mut self.restart_crashed,
            "auto_connect_accept" => &mut self.auto_connect_accept,
            "group_experiences" => &mut self.group_experiences,
            "studio_tools" => &mut self.studio_tools,
            "backups" => &mut self.backups,
            "idle_close" => &mut self.idle_close,
            "remove_recovery" => &mut self.remove_recovery,
            _ => return None,
        })
    }

    /// One preference by key; None when the key is unknown.
    pub fn get(mut self, key: &str) -> Option<bool> {
        self.slot(key).map(|slot| *slot)
    }

    /// Set one preference by key; false when the key is unknown.
    pub fn set(&mut self, key: &str, value: bool) -> bool {
        self.slot(key).map(|slot| *slot = value).is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub projects: Vec<Project>,
    pub settings: Settings,
    /// Experiences (universe IDs) whose places the window shows collapsed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collapsed: Vec<u64>,
    /// The window asked whether to keep the Studio tools beside another Studio MCP.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub studio_tools_asked: bool,
    /// When this profile first ran (Unix seconds), so the star prompt waits until the app is in use.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub first_run: u64,
    /// The window asked for a star on GitHub.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub star_asked: bool,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Config {
    pub fn project(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    pub fn project_mut(&mut self, name: &str) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.name == name)
    }
}

/// The data folder ROJO_MANAGER_DATA_DIR names. Set, the app runs an isolated profile, which
/// leaves what belongs to the user's own copy (the Start menu shortcut) alone.
pub fn isolated_dir() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("ROJO_MANAGER_DATA_DIR").filter(|dir| !dir.is_empty())?);
    Some(std::path::absolute(&dir).unwrap_or(dir))
}

pub fn data_dir() -> PathBuf {
    if let Some(dir) = isolated_dir() {
        return dir;
    }
    let home = std::env::home_dir().unwrap_or_default();
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Application Support")
    } else {
        std::env::var_os("LOCALAPPDATA").map_or_else(|| home.join("AppData").join("Local"), PathBuf::from)
    };
    base.join("RojoManager")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// A plain file name, with no folder in it.
fn is_file_name(name: &str) -> bool {
    !name.contains(['/', '\\']) && Path::new(name).file_name() == Some(name.as_ref())
}

fn is_project_file(name: &str) -> bool {
    name.ends_with(".project.json") && is_file_name(name)
}

/// Check a parsed config.json and normalize it. Unknown settings are dropped; anything malformed is an error.
pub fn validate(value: &Value) -> Result<Config, String> {
    let object = value.as_object().ok_or("Settings must contain a projects list")?;
    let mut settings = Settings::default();
    match object.get("settings") {
        None => {}
        Some(Value::Object(stored)) => {
            for key in Settings::KEYS {
                match stored.get(key) {
                    None => {}
                    Some(Value::Bool(on)) => {
                        settings.set(key, *on);
                    }
                    Some(_) => return Err(format!("Setting {key} must be true or false")),
                }
            }
        }
        Some(_) => return Err("Settings must be an object".into()),
    }
    let items = match object.get("projects") {
        None => &Vec::new(),
        Some(Value::Array(items)) => items,
        Some(_) => return Err("Settings must contain a projects list".into()),
    };
    let (mut names, mut ports, mut files, mut projects) = (HashSet::new(), HashSet::new(), HashSet::new(), Vec::new());
    for item in items {
        let item = item.as_object().ok_or("Each project must be an object")?;
        let name = item.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty());
        let name =
            name.filter(|n| names.insert(n.to_lowercase())).ok_or("Project names must be nonempty and unique")?;
        let path = item.get("path").and_then(Value::as_str).filter(|p| Path::new(p).is_absolute());
        let path = path.ok_or("Project folders must be absolute paths")?;
        let port = item.get("port").and_then(Value::as_u64).filter(|p| (1..=65535).contains(p));
        let port =
            port.filter(|p| ports.insert(*p)).ok_or("Project ports must be unique numbers between 1 and 65535")?;
        let project_file = match item.get("projectFile") {
            None => "default.project.json",
            Some(file) => {
                file.as_str().filter(|f| is_project_file(f)).ok_or("Project files must be .project.json filenames")?
            }
        };
        let flag = |key: &str, message: &str| match item.get(key) {
            None => Ok(false),
            Some(Value::Bool(on)) => Ok(*on),
            Some(_) => Err(message.to_string()),
        };
        let autostart = flag("autostart", "Project autostart must be true or false")?;
        let auto_connect = flag("autoConnect", "Project autoConnect must be true or false")?;
        let sourcemap = flag("sourcemap", "Project sourcemap must be true or false")?;
        let place = match item.get("place") {
            None | Some(Value::Null) => None,
            Some(place) if place.get("file").is_some() => {
                let file =
                    place.get("file").and_then(Value::as_str).filter(|f| f.ends_with(".rbxl") && is_file_name(f));
                let file = file.filter(|f| files.insert(f.to_lowercase()));
                let file = file.ok_or("Local places must be unique .rbxl file names")?;
                Some(PlaceLink::Local(LocalPlace { file: file.to_string() }))
            }
            Some(place) => {
                let id = |key| place.get(key).and_then(Value::as_u64).filter(|id| *id > 0);
                match (id("placeId"), id("universeId")) {
                    (Some(place_id), Some(universe_id)) => Some(PlaceLink::Published(Place { place_id, universe_id })),
                    _ => return Err("Project places must have positive place and universe IDs".into()),
                }
            }
        };
        let lane = match item.get("lane") {
            None | Some(Value::Null) => None,
            Some(lane) => {
                let text = |key| lane.get(key).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
                let parent = text("parent").filter(|_| matches!(place, Some(PlaceLink::Local(_))));
                let parent = parent.ok_or("Lanes must name their parent project and have a local place")?;
                let holder = text("holder").map(|holder| holder.chars().take(100).collect());
                Some(Lane { parent: parent.to_string(), holder })
            }
        };
        let idle_close = match item.get("idleClose") {
            None | Some(Value::Null) => None,
            Some(minutes) => {
                let minutes = minutes.as_u64().filter(|m| (1..=u64::from(MAX_IDLE_CLOSE)).contains(m));
                Some(minutes.ok_or("Project idleClose must be 1 to 10080 minutes")? as u32)
            }
        };
        projects.push(Project {
            name: name.to_string(),
            path: path.to_string(),
            project_file: project_file.to_string(),
            port: port as u16,
            autostart,
            auto_connect,
            place,
            sourcemap,
            lane,
            idle_close,
        });
    }
    // Only a window preference, so a bad entry is dropped instead of refusing the file.
    let collapsed = match object.get("collapsed") {
        Some(Value::Array(ids)) => ids.iter().filter_map(Value::as_u64).filter(|id| *id > 0).collect(),
        _ => Vec::new(),
    };
    let studio_tools_asked = object.get("studio_tools_asked").and_then(Value::as_bool).unwrap_or(false);
    let first_run = object.get("first_run").and_then(Value::as_u64).unwrap_or(0);
    let star_asked = object.get("star_asked").and_then(Value::as_bool).unwrap_or(false);
    Ok(Config { projects, settings, collapsed, studio_tools_asked, first_run, star_asked })
}

/// Read config.json; a missing file is an empty config. A malformed file is reported, never overwritten.
pub fn load() -> Result<Config, String> {
    let path = config_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error.to_string()),
    };
    let value: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    validate(&value)
}

/// Validate, then atomically replace config.json.
pub fn save(config: &Config) -> Result<(), String> {
    let clean = validate(&serde_json::to_value(config).map_err(|e| e.to_string())?)?;
    let path = config_path();
    let dir = path.parent().ok_or("Invalid settings folder")?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut file =
        tempfile::Builder::new().prefix("config-").suffix(".tmp").tempfile_in(dir).map_err(|e| e.to_string())?;
    let mut text = serde_json::to_string_pretty(&clean).map_err(|e| e.to_string())?;
    text.push('\n');
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(&path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An absolute project folder on this platform.
    const FOLDER: &str = if cfg!(windows) { r"C:\Games\Game" } else { "/Games/Game" };

    #[test]
    fn a_v05_config_round_trips() {
        let stored = json!({
            "projects": [{"name": "Game", "path": FOLDER, "projectFile": "default.project.json",
                          "port": 34872, "autostart": true, "autoConnect": true,
                          "place": {"placeId": 1, "universeId": 2}, "sourcemap": true},
                         {"name": "Lobby", "path": FOLDER, "projectFile": "default.project.json",
                          "port": 34873, "autostart": false, "place": {"file": "Lobby-0123abcd.rbxl"},
                          "lane": {"parent": "Game", "holder": "lane B"}, "idleClose": 30}],
            "settings": {"tray": false, "unknown": 1}
        });
        let config = validate(&stored).unwrap();
        assert!(!config.settings.tray && config.settings.show_wally);
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["projects"], stored["projects"]);
        assert!(saved["settings"].get("unknown").is_none());
        assert!(saved.get("collapsed").is_none());
        let collapsed = validate(&json!({"collapsed": [2, "x", 0]})).unwrap().collapsed;
        assert_eq!(collapsed, [2]);
    }

    #[test]
    fn duplicates_and_bad_values_are_refused() {
        let project = |name: &str, port: u16| json!({"name": name, "path": FOLDER, "port": port});
        assert!(validate(&json!({"projects": [project("A", 1), project("a", 2)]})).is_err());
        assert!(validate(&json!({"projects": [project("A", 1), project("B", 1)]})).is_err());
        assert!(validate(&json!({"projects": [{"name": "A", "path": "relative", "port": 1}]})).is_err());
        assert!(
            validate(
                &json!({"projects": [{"name": "A", "path": FOLDER, "port": 1, "projectFile": "..\\a.project.json"}]})
            )
            .is_err()
        );
        assert!(validate(&json!({"settings": {"tray": "yes"}})).is_err());
        let local = |name: &str, port: u16, file: &str| json!({"name": name, "path": FOLDER, "port": port, "place": {"file": file}});
        assert!(validate(&json!({"projects": [local("A", 1, "..\\a.rbxl")]})).is_err());
        assert!(validate(&json!({"projects": [local("A", 1, "a.txt")]})).is_err());
        assert!(validate(&json!({"projects": [local("A", 1, "a.rbxl"), local("B", 2, "A.rbxl")]})).is_err());
        let lane = json!({"name": "A", "path": FOLDER, "port": 1, "lane": {"parent": "Game"}});
        assert!(validate(&json!({"projects": [lane]})).is_err()); // a lane needs a local place
        let idle = |minutes: u64| json!({"name": "A", "path": FOLDER, "port": 1, "idleClose": minutes});
        assert!(validate(&json!({"projects": [idle(0)]})).is_err());
        assert!(validate(&json!({"projects": [idle(10081)]})).is_err());
        assert_eq!(validate(&json!({})).unwrap(), Config::default());
    }
}
