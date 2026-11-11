//! Compare the installed app version with its public GitHub releases, and install a newer one.

use std::fmt::Write as _;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::https::{self, HttpsError};
use crate::tooling::{self, Release};

pub const REPOSITORY: &str = "lClayk/Rojo-Manager";
pub const RELEASES_URL: &str = "https://github.com/lClayk/Rojo-Manager/releases";
const DOWNLOADS: &str = "https://github.com/lClayk/Rojo-Manager/releases/download/";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
/// This platform's release zip ends in `-<ASSET>.zip`. BUNDLE is the folder inside it that holds the
/// app, and MAIN the app's executable inside that folder.
#[cfg(windows)]
const ASSET: &str = "windows-x64";
#[cfg(windows)]
const BUNDLE: &str = "RojoManager";
#[cfg(windows)]
const MAIN: &str = "RojoManager.exe";
#[cfg(target_os = "macos")]
const ASSET: &str = "macos";
#[cfg(target_os = "macos")]
const BUNDLE: &str = "Rojo Manager.app";
#[cfg(target_os = "macos")]
const MAIN: &str = "Contents/MacOS/RojoManager";
/// Files an update moved aside end with this; they are deleted on the next start.
const ASIDE: &str = ".old";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateState {
    Checking,
    Disabled,
    Current,
    Outdated,
    /// Downloading and swapping in the newer release.
    Installing,
    Unavailable,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub state: UpdateState,
    pub message: String,
    /// Built here from the tag, never taken from release metadata.
    pub url: String,
}

/// `v?MAJOR.MINOR.PATCH` with no leading zeros and nothing else, like a release tag.
pub fn stable_version(tag: &str) -> Option<(u32, u32, u32)> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.').map(|part| {
        let canonical =
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) && (part == "0" || !part.starts_with('0'));
        if canonical { part.parse().ok() } else { None }
    });
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// Whether the stable release `tag` is newer than the installed version. A beta build, such as
/// "0.8.0-beta.1", comes before 0.8.0. None when either version cannot be read.
fn is_newer(tag: &str, installed: &str) -> Option<bool> {
    let latest = stable_version(tag)?;
    let (number, beta) = installed.split_once('-').map_or((installed, false), |(number, _)| (number, true));
    let current = stable_version(number)?;
    Some(latest > current || (latest == current && beta))
}

/// Check GitHub's latest release, which never includes pre-releases, so betas are not offered.
pub fn inspect_update(enabled: bool, installed: &str) -> UpdateStatus {
    if !enabled {
        return status(UpdateState::Disabled, "Automatic app update checks are off.".into(), RELEASES_URL.into());
    }
    update_status(tooling::fetch_latest_release(REPOSITORY), installed)
}

fn status(state: UpdateState, message: String, url: String) -> UpdateStatus {
    UpdateStatus { state, message, url }
}

/// Compare the latest release lookup with the installed version.
fn update_status(release: Result<Release, HttpsError>, installed: &str) -> UpdateStatus {
    let failed =
        || status(UpdateState::Error, "Could not check for app updates. Try again later.".into(), RELEASES_URL.into());
    let tag = match release {
        Ok(release) => release.tag_name,
        Err(HttpsError::Status(401 | 403 | 404)) => {
            let message = "Could not access public releases. Try again later or open the release page.";
            return status(UpdateState::Unavailable, message.into(), RELEASES_URL.into());
        }
        Err(_) => return failed(),
    };
    let Some(newer) = is_newer(&tag, installed) else {
        return failed();
    };
    // Construct the link here, never open a URL supplied by release metadata. A stable tag needs no escaping.
    let url = format!("{RELEASES_URL}/tag/{tag}");
    if newer {
        let message = format!("Rojo Manager {} is available. Installed: {installed}.", tag.trim_start_matches('v'));
        return status(UpdateState::Outdated, message, url);
    }
    status(UpdateState::Current, format!("Rojo Manager {installed} is up to date."), url)
}

/// Where the app's files start, given the executables' folder: that folder on Windows, the .app
/// bundle on macOS.
fn install_root(app_dir: &Path) -> &Path {
    if cfg!(target_os = "macos") && app_dir.ends_with("Contents/MacOS") {
        return app_dir.parent().and_then(Path::parent).unwrap_or(app_dir);
    }
    app_dir
}

/// Download the latest release and swap it into the app's folder (`app_dir` holds the
/// executables). On Windows, running executables are renamed aside and removed by
/// `remove_replaced` on the next start. A macOS .app is replaced whole. Returns the version.
pub fn install_latest(app_dir: &Path, installed: &str) -> Result<String, String> {
    let root = install_root(app_dir);
    let release = tooling::fetch_latest_release(REPOSITORY).map_err(|_| "Could not reach GitHub. Try again later.")?;
    let tag = release.tag_name.clone();
    if is_newer(&tag, installed) != Some(true) {
        return Err(format!("Rojo Manager {installed} is up to date."));
    }
    let version = tag.trim_start_matches('v');
    // The link is built here, never taken from release metadata; the API adds the checksum.
    let name = format!("RojoManager-{version}-{ASSET}.zip");
    let digest = release.assets.iter().find(|asset| asset.name == name).and_then(|asset| asset.digest.clone());
    let folder = tempfile::Builder::new().prefix("rojo-manager-update-").tempdir().map_err(|e| e.to_string())?;
    let archive = folder.path().join(&name);
    https::download(format!("{DOWNLOADS}{tag}/{name}").as_str(), &archive, DOWNLOAD_TIMEOUT)
        .map_err(|e| format!("Could not download {name}: {e}"))?;
    let data = fs::read(&archive).map_err(|e| e.to_string())?;
    check_digest(&data, digest.as_deref())?;
    // A bundle is staged beside itself, so it can be renamed into place.
    let bundle = root.extension().is_some_and(|extension| extension == "app");
    let staging = if bundle { root.parent().unwrap_or(root) } else { root };
    let staged = tempfile::Builder::new().prefix(".update-").tempdir_in(staging).map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "Rojo Manager's folder is not writable. Move it to a folder you own, or download the release.".into()
        }
        _ => e.to_string(),
    })?;
    unpack(&data, staged.path())?;
    if bundle {
        replace_bundle(staged.path(), root)?
    } else {
        swap(staged.path(), root)?
    }
    Ok(version.to_string())
}

/// Compare with GitHub's "sha256:<hex>" asset digest, when the release has one.
fn check_digest(data: &[u8], digest: Option<&str>) -> Result<(), String> {
    let Some(expected) = digest.and_then(|digest| digest.strip_prefix("sha256:")) else { return Ok(()) };
    let mut actual = String::new();
    for byte in Sha256::digest(data).iter() {
        let _ = write!(actual, "{byte:02x}");
    }
    if actual.eq_ignore_ascii_case(expected) { Ok(()) } else { Err("The download's checksum did not match".into()) }
}

/// Write the zip's BUNDLE folder into `into`, keeping executables executable. Entries outside it
/// are skipped.
fn unpack(data: &[u8], into: &Path) -> Result<(), String> {
    let mut zipped = zip::ZipArchive::new(Cursor::new(data)).map_err(|e| e.to_string())?;
    for index in 0..zipped.len() {
        let mut file = zipped.by_index(index).map_err(|e| e.to_string())?;
        let Some(path) = file.enclosed_name() else { continue };
        let Ok(relative) = path.strip_prefix(BUNDLE) else { continue };
        if file.is_dir() || relative.as_os_str().is_empty() {
            continue;
        }
        let target = into.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut output = fs::File::create(&target).map_err(|e| e.to_string())?;
        std::io::copy(&mut file, &mut output).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        if let Some(mode) = file.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            output.set_permissions(fs::Permissions::from_mode(mode & 0o777)).map_err(|e| e.to_string())?;
        }
    }
    if !into.join(MAIN).is_file() {
        return Err("The download does not contain Rojo Manager".into());
    }
    Ok(())
}

/// Every file under `under`, relative to `root`.
fn files(root: &Path, under: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(under)? {
        let entry = entry?;
        let path = entry.path();
        // file_type does not follow links, so a linked folder is never walked into.
        if entry.file_type()?.is_dir() {
            files(root, &path, found)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            found.push(relative.to_path_buf());
        }
    }
    Ok(())
}

/// Move each staged file into `app_dir`, renaming the file it replaces aside. On failure the
/// files already moved are put back; on success the ones aside are deleted, except running
/// executables, which `remove_replaced` deletes on the next start.
fn swap(staged: &Path, app_dir: &Path) -> Result<(), String> {
    let mut relative = Vec::new();
    files(staged, staged, &mut relative).map_err(|e| e.to_string())?;
    let mut done: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
    let result = relative.iter().try_for_each(|relative| {
        let target = app_dir.join(relative);
        let aside = if target.exists() {
            let mut name = target.file_name().unwrap_or_default().to_os_string();
            name.push(format!(".{}{ASIDE}", std::process::id()));
            let aside = target.with_file_name(name);
            fs::rename(&target, &aside)?;
            Some(aside)
        } else {
            None
        };
        done.push((target.clone(), aside));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(staged.join(relative), &target)
    });
    if let Err(error) = result {
        for (target, aside) in done.into_iter().rev() {
            let _ = fs::remove_file(&target);
            if let Some(aside) = aside {
                let _ = fs::rename(&aside, &target);
            }
        }
        return Err(format!("Could not replace the app's files: {error}"));
    }
    for aside in done.into_iter().filter_map(|(_, aside)| aside) {
        let _ = fs::remove_file(aside);
    }
    Ok(())
}

/// Replace a whole .app bundle with `staged`, so no file of the old version stays behind to break
/// the bundle's signature. The old bundle is renamed aside and deleted (macOS allows that while it
/// runs); if the new one cannot take its place, the old one is put back.
fn replace_bundle(staged: &Path, bundle: &Path) -> Result<(), String> {
    let mut name = bundle.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}{ASIDE}", std::process::id()));
    let aside = bundle.with_file_name(name);
    fs::rename(bundle, &aside).map_err(|e| format!("Could not replace the app: {e}"))?;
    if let Err(error) = fs::rename(staged, bundle) {
        let _ = fs::rename(&aside, bundle);
        return Err(format!("Could not replace the app: {error}"));
    }
    let _ = fs::remove_dir_all(&aside);
    Ok(())
}

/// A name `swap` gives a replaced executable: `<name>.exe.<pid>.old`.
fn moved_aside(name: &str) -> bool {
    let split = name.strip_suffix(ASIDE).and_then(|rest| rest.rsplit_once('.'));
    split.is_some_and(|(file, pid)| {
        file.to_ascii_lowercase().ends_with(".exe") && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
    })
}

/// Delete the executables an earlier update moved aside while they ran. Only `app_dir` itself is
/// looked at, and only names `swap` gives, since the app may sit in a folder with other files.
/// macOS replaces the whole bundle at once, so this finds nothing there.
pub fn remove_replaced(app_dir: &Path) {
    let Ok(entries) = fs::read_dir(app_dir) else { return };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_file()) && moved_aside(&entry.file_name().to_string_lossy()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn bundle(files: &[(&str, &str)]) -> Vec<u8> {
        let mut zipped = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, text) in files {
            zipped.start_file(*name, options).unwrap();
            zipped.write_all(text.as_bytes()).unwrap();
        }
        zipped.finish().unwrap().into_inner()
    }

    #[test]
    fn an_update_replaces_the_app_files_and_removes_the_old_ones() {
        let app = tempfile::tempdir().unwrap();
        let main = app.path().join(MAIN);
        fs::create_dir_all(main.parent().unwrap()).unwrap();
        fs::write(&main, "old").unwrap();
        fs::write(app.path().join("notes.txt"), "mine").unwrap();
        let data = bundle(&[
            (format!("{BUNDLE}/{MAIN}").as_str(), "new"),
            (format!("{BUNDLE}/licenses/LICENSE.txt").as_str(), "license"),
            (format!("{BUNDLE}/../escape.txt").as_str(), "outside"),
            ("other/ignored.txt", "outside"),
        ]);
        let staged = tempfile::tempdir_in(app.path()).unwrap();
        unpack(&data, staged.path()).unwrap();
        swap(staged.path(), app.path()).unwrap();
        assert_eq!(fs::read_to_string(&main).unwrap(), "new");
        assert_eq!(fs::read_to_string(app.path().join("licenses").join("LICENSE.txt")).unwrap(), "license");
        assert_eq!(fs::read_to_string(app.path().join("notes.txt")).unwrap(), "mine");
        assert!(!app.path().join("escape.txt").exists() && !app.path().join("ignored.txt").exists());
        let name = main.file_name().unwrap().display();
        assert!(!main.with_file_name(format!("{name}.{}.old", std::process::id())).exists());
    }

    #[test]
    fn a_bundle_update_replaces_the_whole_bundle() {
        let parent = tempfile::tempdir().unwrap();
        let app = parent.path().join("Rojo Manager.app");
        fs::create_dir_all(app.join("Contents")).unwrap();
        fs::write(app.join("Contents").join("obsolete"), "old").unwrap();
        let staged = parent.path().join(".update-1");
        fs::create_dir_all(staged.join("Contents")).unwrap();
        fs::write(staged.join("Contents").join("Info.plist"), "new").unwrap();
        replace_bundle(&staged, &app).unwrap();
        assert_eq!(fs::read_to_string(app.join("Contents").join("Info.plist")).unwrap(), "new");
        assert!(!app.join("Contents").join("obsolete").exists() && !staged.exists());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 1, "the old bundle is deleted");
    }

    #[test]
    fn only_executables_an_update_moved_aside_are_removed_at_start() {
        let app = tempfile::tempdir().unwrap();
        let names = ["RojoManager.exe.4242.old", "notes.old", "backup.v2.old", "save.2024.old", "RojoManager.exe"];
        for name in names {
            fs::write(app.path().join(name), "").unwrap();
        }
        fs::create_dir(app.path().join("photos")).unwrap();
        fs::write(app.path().join("photos").join("RojoManager.exe.1.old"), "").unwrap();
        remove_replaced(app.path());
        let mut left: Vec<_> = fs::read_dir(app.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        left.sort();
        assert_eq!(left, ["RojoManager.exe", "backup.v2.old", "notes.old", "photos", "save.2024.old"]);
        assert!(app.path().join("photos").join("RojoManager.exe.1.old").exists());
    }

    #[test]
    fn a_download_without_the_app_or_with_a_wrong_checksum_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let error = unpack(&bundle(&[(format!("{BUNDLE}/README.md").as_str(), "")]), folder.path()).unwrap_err();
        assert_eq!(error, "The download does not contain Rojo Manager");
        assert!(check_digest(b"data", Some("sha256:00")).is_err());
        assert!(check_digest(b"data", None).is_ok());
    }

    fn latest(tag: &str) -> Result<Release, HttpsError> {
        Ok(Release { tag_name: tag.into(), assets: Vec::new(), from_release_page: false })
    }

    #[test]
    fn stable_versions_are_plain_release_tags() {
        assert_eq!(stable_version("v0.10.0"), Some((0, 10, 0)));
        assert_eq!(stable_version("1.2.3"), Some((1, 2, 3)));
        for tag in ["v0.3.0-rc.1", "../../", "v01.2.3", "1.2", "1.2.3.4", "vv1.2.3", ""] {
            assert_eq!(stable_version(tag), None, "{tag}");
        }
    }

    #[test]
    fn newer_release_compares_numeric_components() {
        let status = update_status(latest("v0.10.0"), "0.9.0");
        assert_eq!(status.state, UpdateState::Outdated);
        assert_eq!(status.message, "Rojo Manager 0.10.0 is available. Installed: 0.9.0.");
        assert_eq!(status.url, format!("{RELEASES_URL}/tag/v0.10.0"));
    }

    #[test]
    fn equal_or_older_releases_do_not_prompt_for_a_downgrade() {
        for tag in ["v0.2.0", "v0.1.0"] {
            assert_eq!(update_status(latest(tag), "0.2.0").state, UpdateState::Current);
        }
    }

    #[test]
    fn a_beta_build_is_offered_its_stable_release_only() {
        assert_eq!(update_status(latest("v0.8.0"), "0.8.0-beta.1").state, UpdateState::Outdated);
        assert_eq!(update_status(latest("v0.7.5"), "0.8.0-beta.1").state, UpdateState::Current);
        assert_eq!(update_status(latest("v0.7.5"), "0.7.5-beta.2").state, UpdateState::Outdated);
    }

    #[test]
    fn disabled_check_does_not_use_network() {
        assert_eq!(inspect_update(false, "0.2.0").state, UpdateState::Disabled);
    }

    #[test]
    fn private_offline_and_untrusted_releases_are_never_reported_as_current() {
        assert_eq!(update_status(Err(HttpsError::Status(404)), "0.2.0").state, UpdateState::Unavailable);
        assert_eq!(update_status(Err(HttpsError::Transport("offline".into())), "0.2.0").state, UpdateState::Error);
        for tag in ["v0.3.0-rc.1", "../../"] {
            assert_eq!(update_status(latest(tag), "0.2.0").state, UpdateState::Error, "{tag}");
        }
    }
}
