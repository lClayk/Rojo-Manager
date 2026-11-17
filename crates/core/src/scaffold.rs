//! New Rojo projects on disk: a fresh one from `rojo init`, or one converted from a place file with
//! `rojo syncback`. Registering them is the manager's job.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value, json};

use crate::tooling;

/// The services a converted project syncs, as paths under the DataModel. Maps, Lighting, and
/// everything else stay in the place.
const CODE_SERVICES: [&str; 8] = [
    "ReplicatedFirst",
    "ReplicatedStorage",
    "ServerScriptService",
    "ServerStorage",
    "StarterGui",
    "StarterPack",
    "StarterPlayer/StarterPlayerScripts",
    "StarterPlayer/StarterCharacterScripts",
];

const GITIGNORE: &str =
    "# Place files\n/*.rbxl\n/*.rbxlx\n\n# Roblox Studio lock files\n/*.rbxl.lock\n/*.rbxlx.lock\n\nsourcemap.json\n";

/// Make a place project in `path`, a new or empty folder, with `rojo init`.
pub fn create(rojo: &Path, path: &Path) -> Result<(), String> {
    let existed = check_destination(path)?;
    let made = run(rojo, &["init".as_ref(), "--color".as_ref(), "never".as_ref(), path.as_os_str()], None)
        .and_then(|_| pin_rojo(path));
    if made.is_err() {
        // Only Rojo wrote here: the folder was new or empty.
        let _ = fs::remove_dir_all(path);
        if existed {
            let _ = fs::create_dir(path);
        }
    }
    made
}

/// Make a project in `path`, a new or empty folder, from the place file `file`: its code services
/// written out by `rojo syncback` (Rojo 7.7 or later), plus a .gitignore and a git repository.
/// Built in a folder beside `path` first, so a failure leaves nothing behind.
pub fn convert(rojo: &Path, file: &Path, path: &Path) -> Result<(), String> {
    let existed = check_destination(path)?;
    if !file.is_file() {
        return Err(format!("No place file at {}", file.display()));
    }
    let parent = path.parent().ok_or("Choose a folder, not a drive")?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).ok_or("Choose a folder, not a drive")?;
    let version = tooling::tool_version("Rojo", Some(parent))?;
    if tooling::parse_version(&version).is_none_or(|version| version < (7, 7, 0)) {
        return Err(format!("Converting a place needs Rojo 7.7 or later, and {version} is installed."));
    }
    let staging = parent.join(format!(".{name}.converting"));
    let _ = fs::remove_dir_all(&staging);
    let built = syncback(rojo, file, &staging, &name).and_then(|_| {
        if existed {
            fs::remove_dir(path).map_err(|e| format!("Could not use {}: {e}", path.display()))?;
        }
        fs::rename(&staging, path).map_err(|e| format!("Could not move the project into {}: {e}", path.display()))
    });
    if let Err(error) = built {
        let _ = fs::remove_dir_all(&staging);
        if existed && !path.exists() {
            let _ = fs::create_dir(path);
        }
        return Err(error);
    }
    pin_rojo(path)?;
    // Best effort, as rojo init does: a project without git still works.
    let _ = run("git".as_ref(), &["init".as_ref(), "--quiet".as_ref()], Some(path));
    Ok(())
}

/// Write the project into `folder` and sync the place back into it, dropping the services the
/// place lacks (syncback refuses a project node with nothing behind it) and those it left empty.
fn syncback(rojo: &Path, file: &Path, folder: &Path, name: &str) -> Result<(), String> {
    let mut services: Vec<&str> = CODE_SERVICES.to_vec();
    let project = folder.join("default.project.json");
    let write = |services: &[&str]| {
        let text = serde_json::to_string_pretty(&project_json(name, services)).map_err(|e| e.to_string())?;
        fs::write(&project, text + "\n").map_err(|e| format!("Could not write the project file: {e}"))
    };
    for service in &services {
        fs::create_dir_all(source(folder, service))
            .map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
    }
    fs::write(folder.join(".gitignore"), GITIGNORE).map_err(|e| e.to_string())?;
    loop {
        write(&services)?;
        let args = ["syncback".as_ref(), "--color".as_ref(), "never".as_ref(), "--non-interactive".as_ref()];
        let input = ["--input".as_ref(), file.as_os_str()];
        match run(rojo, &[&args[..], &input[..]].concat(), Some(folder)) {
            Ok(_) => break,
            Err(error) if drop_missing(&mut services, &error) => continue,
            Err(error) => return Err(syncback_error(&error)),
        }
    }
    services.retain(|service| fs::read_dir(source(folder, service)).is_ok_and(|mut entries| entries.next().is_some()));
    write(&services)?;
    // What is left empty is a service folder syncback had nothing for.
    for service in CODE_SERVICES.iter().filter(|service| !services.contains(*service)) {
        let _ = fs::remove_dir(source(folder, service));
    }
    let _ = fs::remove_dir(folder.join("src/StarterPlayer"));
    Ok(())
}

/// What to tell the user when syncback fails: what to fix for duplicate names, which it refuses
/// in a synced service, else Rojo's own error without its warnings.
fn syncback_error(error: &str) -> String {
    let named = error.split("The child '").nth(1).and_then(|rest| rest.split_once("' of '"));
    let named = named.and_then(|(child, rest)| Some((child, rest.split_once("' is duplicated")?.0)));
    if let Some((child, parent)) = named {
        return format!(
            "{parent} has more than one child named {child}, which Rojo cannot write as files. Rename them in \
             Studio, save the place, and convert again."
        );
    }
    let error = error.find("[ERROR").map_or(error, |at| &error[at..]);
    format!("Rojo could not convert the place: {}", error.trim_start_matches("[ERROR rojo] "))
}

/// The folder a service syncs to: "src/StarterPlayer/StarterPlayerScripts".
fn source(folder: &Path, service: &str) -> PathBuf {
    folder.join("src").join(service)
}

/// A project file syncing each of `services` from its folder under src.
fn project_json(name: &str, services: &[&str]) -> Value {
    let mut tree = Map::new();
    tree.insert("$className".into(), json!("DataModel"));
    for service in services {
        let mut node = &mut tree;
        for part in service.split('/') {
            node = node.entry(part).or_insert_with(|| json!({})).as_object_mut().expect("project nodes are objects");
        }
        node.insert("$path".into(), json!(format!("src/{service}")));
    }
    json!({ "name": name, "tree": tree })
}

/// Drop the service a syncback error names as "present only in a project file", with what is
/// under it. Returns false when the error is something else.
fn drop_missing(services: &mut Vec<&str>, error: &str) -> bool {
    let Some(child) = error.split("The child '").nth(1).and_then(|rest| rest.split('\'').next()) else {
        return false;
    };
    let before = services.len();
    services.retain(|service| !service.split('/').any(|part| part == child));
    services.len() < before
}

/// Fails unless `path` is a new folder or an empty one; answers whether it exists.
fn check_destination(path: &Path) -> Result<bool, String> {
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    match fs::read_dir(path).map(|mut entries| entries.next().is_none()) {
        Ok(true) => Ok(true),
        Ok(false) => Err(format!("{} is not empty. Choose a new or empty folder.", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Cannot use {}: {error}", path.display())),
    }
}

/// Pin the installed Rojo in a rokit.toml when Rokit is installed, so the project keeps working
/// with the version it was made with.
fn pin_rojo(path: &Path) -> Result<(), String> {
    if tooling::find_tool("Rokit").is_none() || path.join("rokit.toml").exists() {
        return Ok(());
    }
    let Some(version) = tooling::tool_version("Rojo", Some(path))
        .ok()
        .and_then(|text| tooling::full_version(&text).map(str::to_string))
    else {
        return Ok(());
    };
    let text = format!("[tools]\nrojo = \"rojo-rbx/rojo@{version}\"\n");
    fs::write(path.join("rokit.toml"), text).map_err(|e| format!("Could not write rokit.toml: {e}"))
}

/// Run a console program with no window; its last output line is the error when it fails.
fn run(program: &Path, args: &[&std::ffi::OsStr], cwd: Option<&Path>) -> Result<(), String> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, crate::CREATE_NO_WINDOW);
    let output = command.output().map_err(|e| format!("Could not run {}: {e}", program.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    let text: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    Err(if text.is_empty() { format!("exited with {}", output.status) } else { text.join(" ") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syncback_errors_say_what_to_fix() {
        let duplicated = "[WARN  rbx_binary] Unknown value type in 'Terrain'. Beginning syncback... [ERROR rojo] \
            Failed to syncback  because The child 'Admin' of 'ServerScriptService' is duplicated in the place file.";
        assert_eq!(
            syncback_error(duplicated),
            "ServerScriptService has more than one child named Admin, which Rojo cannot write as files. Rename \
             them in Studio, save the place, and convert again."
        );
        assert_eq!(
            syncback_error("[WARN  x] noise [ERROR rojo] Failed to read the input file"),
            "Rojo could not convert the place: Failed to read the input file"
        );
    }

    #[test]
    fn missing_services_are_dropped_from_the_project() {
        let mut services = CODE_SERVICES.to_vec();
        let error =
            "Failed to syncback because The child 'StarterPlayer' of Instance 'Game' is present only in a project file";
        assert!(drop_missing(&mut services, error));
        assert!(!services.iter().any(|service| service.starts_with("StarterPlayer")));
        assert!(!drop_missing(&mut services, "Failed to read the input file"));
        assert_eq!(
            project_json("Game", &["ServerScriptService"]),
            json!({"name": "Game", "tree": {"$className": "DataModel", "ServerScriptService": {"$path": "src/ServerScriptService"}}})
        );
        let nested = project_json("Game", &["StarterPlayer/StarterPlayerScripts"]);
        assert_eq!(
            nested["tree"]["StarterPlayer"]["StarterPlayerScripts"]["$path"],
            "src/StarterPlayer/StarterPlayerScripts"
        );
    }
}
