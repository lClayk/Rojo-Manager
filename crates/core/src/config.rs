//! Private per-user settings in %LOCALAPPDATA%\RojoManager\config.json (or $ROJO_MANAGER_DATA_DIR).
//! The file format is shared with Rojo Manager 0.5, so upgrading keeps every project.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The Roblox place a project opens in Studio. Studio needs both IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Place {
    pub place_id: u64,
    pub universe_id: u64,
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
    pub place: Option<Place>,
}

/// App preferences. Field names are the JSON keys agents use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub tray: bool,
    pub start_hidden: bool,
    pub version_check: bool,
    pub app_version_check: bool,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            tray: true,
            start_hidden: false,
            version_check: true,
            app_version_check: true,
            show_wally: true,
            show_rokit: true,
            show_aftman: true,
            tray_status: true,
            notifications: true,
            restart_crashed: true,
            auto_connect_accept: true,
        }
    }
}

impl Settings {
    pub const KEYS: [&'static str; 11] = [
        "tray",
        "start_hidden",
        "version_check",
        "app_version_check",
        "show_wally",
        "show_rokit",
        "show_aftman",
        "tray_status",
        "notifications",
        "restart_crashed",
        "auto_connect_accept",
    ];

    fn slot(&mut self, key: &str) -> Option<&mut bool> {
        Some(match key {
            "tray" => &mut self.tray,
            "start_hidden" => &mut self.start_hidden,
            "version_check" => &mut self.version_check,
            "app_version_check" => &mut self.app_version_check,
            "show_wally" => &mut self.show_wally,
            "show_rokit" => &mut self.show_rokit,
            "show_aftman" => &mut self.show_aftman,
            "tray_status" => &mut self.tray_status,
            "notifications" => &mut self.notifications,
            "restart_crashed" => &mut self.restart_crashed,
            "auto_connect_accept" => &mut self.auto_connect_accept,
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
}

impl Config {
    pub fn project(&self, name: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.name == name)
    }

    pub fn project_mut(&mut self, name: &str) -> Option<&mut Project> {
        self.projects.iter_mut().find(|p| p.name == name)
    }
}

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ROJO_MANAGER_DATA_DIR").filter(|dir| !dir.is_empty()) {
        let dir = PathBuf::from(dir);
        return std::path::absolute(&dir).unwrap_or(dir);
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default();
        home.join("AppData").join("Local")
    });
    base.join("RojoManager")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

fn is_project_file(name: &str) -> bool {
    name.ends_with(".project.json") && !name.contains(['/', '\\']) && Path::new(name).file_name() == Some(name.as_ref())
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
    let (mut names, mut ports, mut projects) = (HashSet::new(), HashSet::new(), Vec::new());
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
        let place = match item.get("place") {
            None | Some(Value::Null) => None,
            Some(place) => {
                let id = |key| place.get(key).and_then(Value::as_u64).filter(|id| *id > 0);
                match (id("placeId"), id("universeId")) {
                    (Some(place_id), Some(universe_id)) => Some(Place { place_id, universe_id }),
                    _ => return Err("Project places must have positive place and universe IDs".into()),
                }
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
        });
    }
    Ok(Config { projects, settings })
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

    #[test]
    fn a_v05_config_round_trips() {
        let stored = json!({
            "projects": [{"name": "Game", "path": "C:\\Games\\Game", "projectFile": "default.project.json",
                          "port": 34872, "autostart": true, "autoConnect": true,
                          "place": {"placeId": 1, "universeId": 2}}],
            "settings": {"tray": false, "unknown": 1}
        });
        let config = validate(&stored).unwrap();
        assert!(!config.settings.tray && config.settings.show_wally);
        let saved = serde_json::to_value(&config).unwrap();
        assert_eq!(saved["projects"][0], stored["projects"][0]);
        assert!(saved["settings"].get("unknown").is_none());
    }

    #[test]
    fn duplicates_and_bad_values_are_refused() {
        let project = |name: &str, port: u16| json!({"name": name, "path": "C:\\x", "port": port});
        assert!(validate(&json!({"projects": [project("A", 1), project("a", 2)]})).is_err());
        assert!(validate(&json!({"projects": [project("A", 1), project("B", 1)]})).is_err());
        assert!(validate(&json!({"projects": [{"name": "A", "path": "relative", "port": 1}]})).is_err());
        assert!(
            validate(
                &json!({"projects": [{"name": "A", "path": "C:\\x", "port": 1, "projectFile": "..\\a.project.json"}]})
            )
            .is_err()
        );
        assert!(validate(&json!({"settings": {"tray": "yes"}})).is_err());
        assert_eq!(validate(&json!({})).unwrap(), Config::default());
    }
}
