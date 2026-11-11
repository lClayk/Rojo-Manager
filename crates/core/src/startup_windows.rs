//! Windows: launch at sign-in through HKCU\Software\Microsoft\Windows\CurrentVersion\Run, and the
//! Start menu shortcut that makes the app show up in Windows search.
//!
//! Rojo Manager 0.5 used a RojoManager.lnk (or older .bat) in the Startup folder instead; those
//! count as enabled and are removed whenever startup is changed. A Run value for another copy of
//! the app (one that was moved, say) does not count, so turning startup on again fixes it.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IPersistFile,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
use windows::core::{Interface, PCWSTR};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "RojoManager";

pub fn startup_enabled() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    enabled_at(RUN_KEY, &exe, &legacy_files())
}

/// Enable or disable launching this executable at sign-in, with --hidden when `hidden`.
pub fn set_startup(enabled: bool, hidden: bool) -> io::Result<()> {
    let command = if enabled { Some(command(&std::env::current_exe()?, hidden)) } else { None };
    set_at(RUN_KEY, command.as_deref(), &legacy_files())
}

/// 0.5's shortcut and the batch file before it.
fn legacy_files() -> Vec<PathBuf> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return Vec::new();
    };
    let folder = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup");
    vec![folder.join("RojoManager.lnk"), folder.join("RojoManager.bat")]
}

/// Point the Start menu's "Rojo Manager" shortcut at this executable, so Windows search finds the
/// app wherever it was extracted. Runs at every launch, which also follows the app when it moves.
pub fn add_to_start_menu() -> io::Result<()> {
    let appdata = std::env::var_os("APPDATA").ok_or_else(|| io::Error::other("APPDATA is not set"))?;
    let shortcut = PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Rojo Manager.lnk");
    save_shortcut(&std::env::current_exe()?, &shortcut)
}

fn save_shortcut(exe: &Path, shortcut: &Path) -> io::Result<()> {
    // SAFETY: COM is initialized for this call only, and every string is NUL-terminated.
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let saved = (|| {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(wide(exe).as_ptr()))?;
            if let Some(folder) = exe.parent() {
                link.SetWorkingDirectory(PCWSTR(wide(folder).as_ptr()))?;
            }
            link.SetDescription(PCWSTR(wide("Run your Rojo servers from one tray app").as_ptr()))?;
            link.cast::<IPersistFile>()?.Save(PCWSTR(wide(shortcut).as_ptr()), true)
        })();
        if initialized {
            CoUninitialize();
        }
        saved.map_err(io::Error::other)
    }
}

/// "<exe>" or "<exe>" --hidden, quoted for paths with spaces.
fn command(exe: &Path, hidden: bool) -> OsString {
    let mut command = OsString::from("\"");
    command.push(exe);
    command.push(if hidden { "\" --hidden" } else { "\"" });
    command
}

/// True when the value launches `exe` (hidden or not), or a legacy file exists.
fn enabled_at(key: &str, exe: &Path, legacy: &[PathBuf]) -> bool {
    let (key, name) = (wide(key), wide(VALUE));
    let mut data = [0u16; 1024];
    let mut size = (data.len() * 2) as u32;
    // SAFETY: both strings are NUL-terminated, and `size` is the byte size of `data`.
    let found = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    let stored = String::from_utf16_lossy(&data[..(size as usize / 2).saturating_sub(1)]).to_lowercase();
    let ours = [command(exe, false), command(exe, true)].map(|command| command.to_string_lossy().to_lowercase());
    (found == ERROR_SUCCESS && ours.contains(&stored)) || legacy.iter().any(|file| file.exists())
}

/// Write `command` to the value, or delete the value when None, then remove the legacy files.
fn set_at(key: &str, command: Option<&OsStr>, legacy: &[PathBuf]) -> io::Result<()> {
    let (key, name) = (wide(key), wide(VALUE));
    let status = match command {
        Some(command) => {
            let data = wide(command);
            // SAFETY: the strings are NUL-terminated and `data` holds the byte count passed.
            unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    PCWSTR(key.as_ptr()),
                    PCWSTR(name.as_ptr()),
                    REG_SZ.0,
                    Some(data.as_ptr().cast()),
                    (data.len() * 2) as u32,
                )
            }
        }
        // SAFETY: both strings are NUL-terminated.
        None => match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(name.as_ptr())) } {
            ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            status => status,
        },
    };
    check(status)?;
    for file in legacy {
        match std::fs::remove_file(file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS { Ok(()) } else { Err(io::Error::from_raw_os_error(status.0 as i32)) }
}

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::RegDeleteKeyW;

    #[test]
    fn the_command_quotes_the_executable() {
        let exe = Path::new(r"C:\Program Files\Rojo Manager\RojoManager.exe");
        assert_eq!(command(exe, false), r#""C:\Program Files\Rojo Manager\RojoManager.exe""#);
        assert_eq!(command(exe, true), r#""C:\Program Files\Rojo Manager\RojoManager.exe" --hidden"#);
    }

    #[test]
    fn the_start_menu_shortcut_is_written() {
        let folder = tempfile::tempdir().unwrap();
        let shortcut = folder.path().join("Rojo Manager.lnk");
        save_shortcut(Path::new(r"C:\Rojo Manager\RojoManager.exe"), &shortcut).unwrap();
        let data = std::fs::read(&shortcut).unwrap();
        // A shell link starts with its 0x4C header size, and stores the target path.
        assert_eq!(data[..4], [0x4C, 0, 0, 0]);
        assert!(String::from_utf8_lossy(&data).contains("RojoManager.exe"));
    }

    #[test]
    fn startup_round_trips_and_clears_legacy_shortcuts() {
        // A scratch key, so the real Run key is never touched.
        let key = format!(r"Software\RojoManagerTest-{}", std::process::id());
        let folder = tempfile::tempdir().unwrap();
        let shortcut = folder.path().join("RojoManager.lnk");
        let legacy = [shortcut.clone(), folder.path().join("RojoManager.bat")];
        let exe = Path::new(r"C:\Rojo Manager\RojoManager.exe");
        assert!(!enabled_at(&key, exe, &legacy));
        std::fs::write(&shortcut, "").unwrap();
        assert!(enabled_at(&key, exe, &legacy));

        set_at(&key, Some(&command(exe, true)), &legacy).unwrap();
        assert!(!shortcut.exists());
        assert!(enabled_at(&key, exe, &legacy));
        // After the app moves, the old value no longer counts.
        assert!(!enabled_at(&key, Path::new(r"D:\Moved\RojoManager.exe"), &legacy));
        set_at(&key, None, &legacy).unwrap();
        assert!(!enabled_at(&key, exe, &legacy));
        set_at(&key, None, &legacy).unwrap(); // already off

        // SAFETY: the key name is NUL-terminated.
        let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide(&key).as_ptr())) };
        check(status).unwrap();
    }
}
