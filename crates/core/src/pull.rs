//! Pull: write what Studio holds for a project's synced instances back into its files. The bridge
//! sends those instances, Lune puts them into a place built from the project, and `rojo syncback`
//! (Rojo 7.7 or later) writes the difference. The project file itself is never changed.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tooling;

const ASSEMBLE_SCRIPT: &str = include_str!("assemble_place.luau");
/// The most synced folders a project may have for a pull.
const MAX_ROOTS: usize = 200;
/// Files listed per kind of change in a pull's result.
const MAX_LISTED: usize = 200;
/// The copy of the project file syncback runs on; syncback rewrites the project file it is given.
const PROJECT_COPY: &str = ".rojo-manager-pull.project.json";
/// The place syncback reads: the project as built, with Studio's instances in it.
const INPUT: &str = "input.rbxl";
/// Properties Studio gives instances that say nothing about the project. With them, syncback writes
/// a .meta.json beside each script and folder, and rewrites every script because its id differs.
const STUDIO_PROPERTIES: [&str; 3] = ["SerializedOverrides", "ScriptGuid", "ExplorerIconContent"];

/// What a pull wrote, or with `preview` would write, as paths in the project folder.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pulled {
    pub preview: bool,
    pub written: Vec<String>,
    pub removed: Vec<String>,
    /// Synced instances that Studio does not have; their files are left alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// What did not come back after the pull: Rojo's state in Studio when it is not Connected
    /// again, or that the server did not start. Both are stopped while files are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rojo: Option<String>,
}

impl Pulled {
    /// Studio and the files already match.
    pub fn is_empty(&self) -> bool {
        self.written.is_empty() && self.removed.is_empty()
    }
}

/// One run of root instances in a capture, as the bridge lists them: the instance at `path`, or
/// with `children` the children of the service there.
#[derive(Debug, Deserialize, Serialize)]
pub struct Entry {
    pub path: Vec<String>,
    pub count: u64,
    #[serde(default)]
    pub children: bool,
}

/// The instances a place project syncs from folders: the path of names of each topmost `$path`
/// node under the DataModel.
pub fn capture_roots(project: &Value) -> Result<Vec<Vec<String>>, String> {
    let tree = project.get("tree").and_then(Value::as_object).ok_or("The project file has no tree")?;
    if tree.get("$className").and_then(Value::as_str) != Some("DataModel") || tree.contains_key("$path") {
        return Err("Pull needs a place project: one whose tree is a DataModel with services under it.".into());
    }
    fn walk(node: &serde_json::Map<String, Value>, path: &mut Vec<String>, roots: &mut Vec<Vec<String>>) {
        for (name, child) in node.iter().filter(|(name, _)| !name.starts_with('$')) {
            let Some(child) = child.as_object() else { continue };
            path.push(name.clone());
            if child.contains_key("$path") {
                roots.push(path.clone());
            } else {
                walk(child, path, roots);
            }
            path.pop();
        }
    }
    let mut roots = Vec::new();
    walk(tree, &mut Vec::new(), &mut roots);
    match roots.len() {
        0 => Err("The project syncs no folders, so there is nothing to pull.".into()),
        count if count > MAX_ROOTS => Err(format!("The project syncs {count} folders; pull handles {MAX_ROOTS}.")),
        _ => Ok(roots),
    }
}

/// Check what Pull needs before Studio is asked for anything: Rojo 7.7 or later, and Lune.
pub fn check_tools(project_folder: &Path) -> Result<(), String> {
    let version = tooling::tool_version("Rojo", Some(project_folder))?;
    if tooling::parse_version(&version).is_none_or(|version| version < (7, 7, 0)) {
        return Err(format!("Pull needs Rojo 7.7 or later, and {version} is installed."));
    }
    tooling::find_tool("Lune").map(|_| ()).ok_or_else(|| "Pull needs Lune. Install it in Settings > Tools.".into())
}

/// Studio's instances put into a place built from the project, ready for `rojo syncback`.
pub struct Prepared {
    rojo: PathBuf,
    folder: PathBuf,
    project: PathBuf,
    /// Holds the place; deleted with this.
    work: tempfile::TempDir,
}

/// Build the project and put the captured instances (`capture`, an .rbxm, empty when no instance
/// was sent) into that place. Nothing in the project folder is touched yet.
pub fn prepare(folder: &Path, project_file: &str, capture: &[u8], entries: &[Entry]) -> Result<Prepared, String> {
    let rojo = tooling::find_tool("Rojo").ok_or("Rojo is not installed.")?;
    let lune = tooling::find_tool("Lune").ok_or("Pull needs Lune. Install it in Settings > Tools.")?;
    let work = tempfile::Builder::new().prefix("rojo-manager-pull-").tempdir().map_err(|e| e.to_string())?;
    let file = |name: &str| work.path().join(name);
    let write =
        |name: &str, data: &[u8]| fs::write(file(name), data).map_err(|e| format!("Could not write {name}: {e}"));
    write("assemble.luau", ASSEMBLE_SCRIPT.as_bytes())?;
    write("manifest.json", &serde_json::to_vec(entries).map_err(|e| e.to_string())?)?;
    if !capture.is_empty() {
        write("capture.rbxm", capture)?;
    }
    let project = folder.join(project_file);
    let base = file("base.rbxl");
    run(&rojo, &["build".as_ref(), project.as_os_str(), "--output".as_ref(), base.as_os_str()], folder)
        .map_err(|error| format!("Rojo could not build the project: {error}"))?;
    let assemble = [file("assemble.luau"), base, file("capture.rbxm"), file("manifest.json"), file(INPUT)];
    let mut args: Vec<&OsStr> = vec!["run".as_ref()];
    args.extend(assemble.iter().map(|path| path.as_os_str()));
    run(&lune, &args, work.path()).map_err(|error| format!("Could not put Studio's instances together: {error}"))?;
    Ok(Prepared { rojo, folder: folder.to_path_buf(), project, work })
}

impl Prepared {
    /// Write the difference between Studio's instances and the project's files into the files, or
    /// with `preview` only list it.
    pub fn syncback(&self, preview: bool) -> Result<Pulled, String> {
        let copy = self.folder.join(PROJECT_COPY);
        fs::write(&copy, project_copy(&self.project)?)
            .map_err(|e| format!("Could not write in the project folder: {e}"))?;
        let input = self.work.path().join(INPUT);
        let mut args: Vec<&OsStr> =
            ["syncback", "--color", "never", "--non-interactive", "--list"].map(OsStr::new).to_vec();
        if preview {
            args.push("--dry-run".as_ref());
        }
        args.extend(["--input".as_ref(), input.as_os_str(), copy.as_os_str()]);
        let listed = run(&self.rojo, &args, &self.folder);
        let _ = fs::remove_file(&copy);
        let listed = listed.map_err(|error| crate::scaffold::syncback_error(&error, "pull"))?;
        let (written, removed) = parse_list(&listed);
        Ok(Pulled { preview, written, removed, missing: Vec::new(), rojo: None })
    }
}

/// The project file's text for syncback to run on: the same project, ignoring `STUDIO_PROPERTIES`.
/// A project without a name gets its folder's, as Rojo names it, since Rojo only does that for a
/// file called default.project.json.
fn project_copy(project: &Path) -> Result<Vec<u8>, String> {
    let data = fs::read(project).map_err(|e| format!("Could not read the project file: {e}"))?;
    let mut value: Value = serde_json::from_slice(&data).map_err(|e| format!("The project file is not JSON: {e}"))?;
    let object = value.as_object_mut().ok_or("The project file is not an object")?;
    if let Some(folder) = project.parent().and_then(Path::file_name) {
        object.entry("name").or_insert_with(|| json!(folder.to_string_lossy()));
    }
    let rules = object.entry("syncbackRules").or_insert_with(|| json!({}));
    let ignored = rules.as_object_mut().map(|rules| rules.entry("ignoreProperties").or_insert_with(|| json!({})));
    if let Some(ignored) = ignored.and_then(Value::as_object_mut) {
        let list = ignored.entry("Instance").or_insert_with(|| json!([]));
        if let Some(list) = list.as_array_mut() {
            list.extend(STUDIO_PROPERTIES.map(Value::from));
        }
    }
    serde_json::to_vec(&value).map_err(|e| e.to_string())
}

/// What `rojo syncback --list` names: ("Writing <path>", "Removing <path>"). It also names the
/// folders above each of those, which are left out, as is the project copy.
fn parse_list(output: &str) -> (Vec<String>, Vec<String>) {
    let mut paths: Vec<(bool, String)> = Vec::new();
    for line in output.lines().map(str::trim) {
        let (written, path) = match (line.strip_prefix("Writing "), line.strip_prefix("Removing ")) {
            (Some(path), _) => (true, path),
            (_, Some(path)) => (false, path),
            _ => continue,
        };
        let path = path.replace('\\', "/");
        if path != PROJECT_COPY {
            paths.push((written, path));
        }
    }
    let above = |folder: &str| {
        paths.iter().any(|(_, path)| path.strip_prefix(folder).is_some_and(|rest| rest.starts_with('/')))
    };
    let list = |written: bool| {
        let kept = paths.iter().filter(|(kind, path)| *kind == written && !above(path));
        kept.map(|(_, path)| path.clone()).take(MAX_LISTED).collect()
    };
    (list(true), list(false))
}

/// Run a console program with no window and return its standard output; its output is the error
/// when it fails.
fn run(program: &Path, args: &[&OsStr], cwd: &Path) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd).stdin(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, crate::CREATE_NO_WINDOW);
    let output = command.output().map_err(|e| format!("Could not run {}: {e}", program.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return Ok(stdout);
    }
    let text = stdout + &String::from_utf8_lossy(&output.stderr);
    let text: Vec<&str> = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect();
    Err(if text.is_empty() { format!("exited with {}", output.status) } else { text.join(" ") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_roots_are_the_topmost_synced_nodes_of_a_place_project() {
        let project = json!({"name": "Game", "tree": {
            "$className": "DataModel",
            "ReplicatedStorage": {"Shared": {"$path": "src/shared", "Inner": {"$path": "src/inner"}}},
            "StarterGui": {"$path": "src/gui"},
            "Workspace": {"$properties": {"Gravity": 50}, "Baseplate": {"$className": "Part"}}
        }});
        assert_eq!(capture_roots(&project).unwrap(), [vec!["ReplicatedStorage", "Shared"], vec!["StarterGui"]]);
        let model = json!({"name": "Lib", "tree": {"$path": "src"}});
        assert!(capture_roots(&model).unwrap_err().contains("place project"));
        let empty = json!({"tree": {"$className": "DataModel", "Workspace": {}}});
        assert!(capture_roots(&empty).unwrap_err().contains("nothing to pull"));
    }

    #[test]
    fn the_syncback_list_drops_the_folders_above_a_change_and_the_project_copy() {
        let output = "Writing .rojo-manager-pull.project.json\nWriting src/shared/Hud.rbxm\nWriting src\\shared\n\
                      Writing src/gui\nWriting src/gui/Empty\nRemoving src/gui/Old.luau\nFinished";
        let (written, removed) = parse_list(output);
        assert_eq!(written, ["src/shared/Hud.rbxm", "src/gui/Empty"]); // a new empty folder stays
        assert_eq!(removed, ["src/gui/Old.luau"]);
    }

    #[test]
    fn the_project_copy_ignores_studios_own_properties_and_keeps_existing_rules() {
        let folder = tempfile::tempdir().unwrap();
        let project = folder.path().join("default.project.json");
        let rules = json!({"ignoreProperties": {"Instance": ["Tags"]}, "ignoreTrees": ["Workspace/Map"]});
        fs::write(&project, json!({"name": "Game", "tree": {}, "syncbackRules": rules}).to_string()).unwrap();
        let copy: Value = serde_json::from_slice(&project_copy(&project).unwrap()).unwrap();
        let ignored = &copy["syncbackRules"]["ignoreProperties"]["Instance"];
        assert_eq!(*ignored, json!(["Tags", "SerializedOverrides", "ScriptGuid", "ExplorerIconContent"]));
        assert_eq!(copy["syncbackRules"]["ignoreTrees"], json!(["Workspace/Map"]));
        assert_eq!(copy["name"], "Game");
        // Without a name, Rojo would refuse the copy for not being called default.project.json.
        fs::write(&project, json!({"tree": {}}).to_string()).unwrap();
        let copy: Value = serde_json::from_slice(&project_copy(&project).unwrap()).unwrap();
        assert_eq!(copy["name"], folder.path().file_name().unwrap().to_string_lossy().as_ref());
    }
}
