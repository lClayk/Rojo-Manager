//! The sync safety net's store: copies of instances Rojo removed from Studio that existed only
//! there, sent by the bridge just before Rojo applied the removal. Each backup is a `.rbxm`, which
//! Studio can also insert by hand, beside a `.json` describing it, in data/backups/<project>/. The
//! newest KEEP per project are kept.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Backups kept per project; older ones are deleted as new ones arrive.
pub const KEEP: usize = 20;
/// Paths listed per backup, and characters per path; a bridge is untrusted.
const MAX_PATHS: usize = 1000;
const MAX_PATH_TEXT: usize = 500;

/// One backup, newest first in `list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backup {
    /// "<unix microseconds>-<hex>", which also sorts by time.
    pub id: String,
    /// Unix seconds.
    pub time: u64,
    /// Full names of the instances copied, as they were in Studio ("Workspace.Map").
    pub paths: Vec<String>,
    /// Instances Rojo removed that could not be copied, such as Terrain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Why the copy failed in Studio; such a backup has nothing to restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Backup {
    pub fn restorable(&self) -> bool {
        self.error.is_none() && !self.paths.is_empty()
    }
}

fn folder(project: &str) -> PathBuf {
    let name: String =
        project.chars().map(|c| if c.is_alphanumeric() || " -_".contains(c) { c } else { '_' }).collect();
    crate::config::data_dir().join("backups").join(name)
}

fn clean(paths: Vec<String>) -> Vec<String> {
    paths.into_iter().take(MAX_PATHS).map(|path| path.chars().take(MAX_PATH_TEXT).collect()).collect()
}

/// Store a backup for `project`: the `.rbxm` content when the copy worked, or the error when not.
pub fn save(
    project: &str,
    paths: Vec<String>,
    skipped: Vec<String>,
    data: Result<Vec<u8>, String>,
) -> Result<Backup, String> {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut random = [0u8; 3];
    let _ = getrandom::fill(&mut random);
    let hex: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let (id, time) = (format!("{:016}-{hex}", since.as_micros()), since.as_secs());
    let (data, error) = match data {
        Ok(data) => (Some(data), None),
        Err(error) => (None, Some(error.chars().take(MAX_PATH_TEXT).collect())),
    };
    let backup = Backup { id, time, paths: clean(paths), skipped: clean(skipped), error };
    let folder = folder(project);
    std::fs::create_dir_all(&folder).map_err(|e| format!("Could not create {}: {e}", folder.display()))?;
    if let Some(data) = data {
        std::fs::write(folder.join(format!("{}.rbxm", backup.id)), data).map_err(|e| e.to_string())?;
    }
    let meta = serde_json::to_vec_pretty(&backup).map_err(|e| e.to_string())?;
    std::fs::write(folder.join(format!("{}.json", backup.id)), meta).map_err(|e| e.to_string())?;
    prune(project);
    Ok(backup)
}

/// The project's backups, newest first.
pub fn list(project: &str) -> Vec<Backup> {
    let Ok(entries) = std::fs::read_dir(folder(project)) else { return Vec::new() };
    let mut backups: Vec<Backup> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok())
        .collect();
    backups.sort_by(|a, b| b.id.cmp(&a.id));
    backups
}

/// A backup and its `.rbxm` content; the newest restorable one without an `id`.
pub fn read(project: &str, id: Option<&str>) -> Result<(Backup, Vec<u8>), String> {
    let backups = list(project);
    let backup = match id {
        Some(id) => backups.into_iter().find(|backup| backup.id == id),
        None => backups.into_iter().find(Backup::restorable),
    };
    let backup = backup.ok_or_else(|| match id {
        Some(id) => format!("{project} has no backup {id}"),
        None => format!("{project} has no backups to restore"),
    })?;
    if !backup.restorable() {
        return Err(format!("Backup {} has nothing to restore: {}", backup.id, backup.error.as_deref().unwrap_or("")));
    }
    let data = std::fs::read(folder(project).join(format!("{}.rbxm", backup.id))).map_err(|e| e.to_string())?;
    Ok((backup, data))
}

/// Move a project's backups along with a rename.
pub fn rename(old: &str, new: &str) {
    let (from, to) = (folder(old), folder(new));
    if from.is_dir() && !to.exists() {
        let _ = std::fs::rename(from, to);
    }
}

fn prune(project: &str) {
    let folder = folder(project);
    for backup in list(project).into_iter().skip(KEEP) {
        let _ = std::fs::remove_file(folder.join(format!("{}.rbxm", backup.id)));
        let _ = std::fs::remove_file(folder.join(format!("{}.json", backup.id)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backups_are_listed_newest_first_and_pruned_to_keep() {
        let _guard = crate::isolated();
        let _ = std::fs::remove_dir_all(folder("MyGame"));
        let failed = save("MyGame", vec!["Workspace.Map".into()], vec![], Err("too large".into())).unwrap();
        assert!(!failed.restorable());
        let paths = vec!["Workspace.Map".into(), "x".repeat(MAX_PATH_TEXT + 10)];
        let first = save("MyGame", paths, vec!["Workspace.Terrain".into()], Ok(b"rbxm".to_vec())).unwrap();
        assert_eq!(first.paths[1].len(), MAX_PATH_TEXT);
        // The newest restorable backup, skipping the failed one, which is newer or equal in time.
        let (found, data) = read("MyGame", None).unwrap();
        assert_eq!((found.id.as_str(), data.as_slice()), (first.id.as_str(), b"rbxm".as_slice()));
        assert!(read("MyGame", Some(&failed.id)).is_err());
        for _ in 0..KEEP {
            save("MyGame", vec!["Workspace.Part".into()], vec![], Ok(vec![1])).unwrap();
        }
        let listed = list("MyGame");
        assert_eq!(listed.len(), KEEP);
        assert!(listed.windows(2).all(|pair| pair[0].id >= pair[1].id));
        assert!(!listed.iter().any(|backup| backup.id == first.id));
        rename("MyGame", "Game");
        assert_eq!((list("MyGame").len(), list("Game").len()), (0, KEEP));
    }
}
