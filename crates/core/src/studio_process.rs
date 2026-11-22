//! One Roblox Studio process, found through its bridge's connection: open it hidden, show or hide
//! its windows, and end it. Hiding is Windows only; elsewhere Studio opens visible.

use std::io;
use std::path::Path;

/// How long a Studio opened hidden has its new windows hidden: its splash and startup dialogs.
#[cfg(windows)]
const STARTUP: std::time::Duration = std::time::Duration::from_secs(45);

#[cfg(windows)]
mod platform {
    use std::collections::HashMap;
    use std::io;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows::Win32::System::Threading::{GetProcessId, OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GW_OWNER, GetForegroundWindow, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
        IsWindowVisible, SW_HIDE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };
    use windows::core::{BOOL, HSTRING, w};

    /// The windows `hide` hid, by process, so `show` brings back the same ones.
    static HIDDEN: Mutex<Option<HashMap<u32, Vec<isize>>>> = Mutex::new(None);
    /// Processes opened hidden that are still starting; `show` takes one out to end its hiding.
    static STARTING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

    fn owner(window: HWND) -> u32 {
        let mut pid = 0u32;
        // SAFETY: `pid` is a valid out pointer.
        unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
        pid
    }

    /// Every top-level window of a process.
    fn windows_of(pid: u32) -> Vec<HWND> {
        unsafe extern "system" fn each(window: HWND, found: LPARAM) -> BOOL {
            // SAFETY: `found` is the Vec passed to EnumWindows below, alive for the whole call.
            unsafe { (*(found.0 as *mut Vec<HWND>)).push(window) };
            BOOL(1)
        }
        let mut windows: Vec<HWND> = Vec::new();
        // SAFETY: the callback only pushes to `windows`, which outlives the call.
        let _ = unsafe { EnumWindows(Some(each), LPARAM(&mut windows as *mut Vec<HWND> as isize)) };
        windows.retain(|window| owner(*window) == pid);
        windows
    }

    fn visible(window: HWND) -> bool {
        // SAFETY: a plain query; a closed window reads as not visible.
        unsafe { IsWindowVisible(window) }.as_bool()
    }

    /// Studio's main window: the unowned one titled "<place> - Roblox Studio".
    fn main_window(pid: u32) -> Option<HWND> {
        windows_of(pid).into_iter().find(|window| {
            let mut text = [0u16; 2048];
            // SAFETY: `text` is a writable buffer of its own length; GetWindow only reads.
            let (length, owned) = unsafe {
                (GetWindowTextW(*window, &mut text), GetWindow(*window, GW_OWNER).is_ok_and(|o| !o.is_invalid()))
            };
            !owned && String::from_utf16_lossy(&text[..length.max(0) as usize]).ends_with(" - Roblox Studio")
        })
    }

    /// Open a file or link without showing its window; answers the process it started, if it started one.
    pub fn open_hidden(target: &str) -> io::Result<Option<u32>> {
        let target = HSTRING::from(target);
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: w!("open"),
            lpFile: windows::core::PCWSTR(target.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        // SAFETY: `info` is fully set up and its strings outlive the call.
        unsafe { ShellExecuteExW(&mut info) }.map_err(io::Error::other)?;
        if info.hProcess.is_invalid() {
            return Ok(None);
        }
        // SAFETY: the handle came from ShellExecuteExW and is closed here.
        let pid = unsafe { GetProcessId(info.hProcess) };
        let _ = unsafe { CloseHandle(info.hProcess) };
        Ok((pid != 0).then_some(pid))
    }

    /// Whether Studio's main window is hidden; None while it has none.
    pub fn hidden(pid: u32) -> Option<bool> {
        main_window(pid).map(|window| !visible(window))
    }

    /// Hide the process's visible windows. With `remember`, `show` brings the same ones back.
    pub fn hide(pid: u32, remember: bool) {
        let shown: Vec<HWND> = windows_of(pid).into_iter().filter(|window| visible(*window)).collect();
        for window in &shown {
            // SAFETY: a plain call; a window closed meanwhile is ignored.
            let _ = unsafe { ShowWindow(*window, SW_HIDE) };
        }
        if remember && !shown.is_empty() {
            let mut hidden = HIDDEN.lock().unwrap_or_else(|p| p.into_inner());
            hidden.get_or_insert_default().entry(pid).or_default().extend(shown.iter().map(|w| w.0 as isize));
        }
    }

    /// Show Studio's main window and the windows `hide` hid, and bring it to the front.
    pub fn show(pid: u32) {
        STARTING.lock().unwrap_or_else(|p| p.into_inner()).retain(|starting| *starting != pid);
        let remembered = HIDDEN.lock().unwrap_or_else(|p| p.into_inner()).as_mut().and_then(|h| h.remove(&pid));
        let remembered = remembered.unwrap_or_default().into_iter().map(|window| HWND(window as *mut _));
        let main = main_window(pid);
        for window in remembered.chain(main) {
            // SAFETY: plain calls; a window that is gone or belongs to another process now is skipped.
            if unsafe { IsWindow(Some(window)) }.as_bool() && owner(window) == pid {
                let _ = unsafe { ShowWindow(window, SW_SHOW) };
            }
        }
        if let Some(main) = main {
            // SAFETY: a plain call; Windows may refuse, which only leaves the window behind others.
            let _ = unsafe { SetForegroundWindow(main) };
        }
    }

    /// Whether the user is in one of this process's windows. A hidden window can stay the
    /// foreground one, such as a dialog of a Studio opened hidden, and nobody is in that.
    pub fn foreground(pid: u32) -> bool {
        // SAFETY: a plain query.
        let window = unsafe { GetForegroundWindow() };
        !window.is_invalid() && visible(window) && owner(window) == pid
    }

    /// Wait up to `timeout` for the process to exit; true once it is gone.
    fn exited(pid: u32, timeout: Duration) -> bool {
        // SAFETY: plain Win32 calls; the handle is closed below. A process that cannot be opened is gone.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else { return true };
        let waited = unsafe { WaitForSingleObject(handle, timeout.as_millis() as u32) };
        let _ = unsafe { CloseHandle(handle) };
        waited.0 == 0
    }

    /// Keep hiding the windows a Studio opened hidden shows while it starts: until `until`, or
    /// until it is shown or exits.
    pub fn hide_while_starting(pid: u32, until: Instant) {
        let starting = || STARTING.lock().unwrap_or_else(|p| p.into_inner());
        starting().push(pid);
        std::thread::spawn(move || {
            while Instant::now() < until && !exited(pid, Duration::from_millis(100)) {
                // Checked and hidden under the lock `show` takes first, so a show is never undone.
                let starting = starting();
                if !starting.contains(&pid) {
                    break;
                }
                // Not remembered: Studio hides most of these itself, and a later show would bring
                // those back as stray windows.
                hide(pid, false);
            }
            starting().retain(|other| *other != pid);
        });
    }
}

/// Open `target`, a place file or a roblox-studio: link, in Studio. With `hidden`, Studio shows no
/// window, and its splash and startup dialogs are hidden too; the answer is its process when one
/// was started hidden. Where hiding is not supported, Studio opens visible.
pub fn open(target: &str, hidden: bool) -> io::Result<Option<u32>> {
    #[cfg(windows)]
    if hidden {
        let pid = platform::open_hidden(target)?;
        if let Some(pid) = pid {
            platform::hide_while_starting(pid, std::time::Instant::now() + STARTUP);
        }
        return Ok(pid);
    }
    let _ = hidden;
    crate::shell::open(target).map(|()| None)
}

/// Whether hiding Studio's windows is supported here.
pub const CAN_HIDE: bool = cfg!(windows);

/// Whether `pid` is a running Roblox Studio.
pub fn is_studio(pid: u32) -> bool {
    crate::localnet::process_name(pid).is_some_and(|name| name.to_lowercase().starts_with("robloxstudio"))
}

/// Whether this Studio's main window is hidden; None when it has none yet, or off Windows.
pub fn hidden(pid: u32) -> Option<bool> {
    #[cfg(windows)]
    return platform::hidden(pid);
    #[cfg(not(windows))]
    {
        let _ = pid;
        None
    }
}

/// Hide or show this Studio's windows. Errors where that is not supported.
pub fn set_hidden(pid: u32, hide: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        if hide {
            platform::hide(pid, true)
        } else {
            platform::show(pid)
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (pid, hide);
        Err("Hiding Studio is Windows only for now.".into())
    }
}

/// Whether the user is working in this Studio right now. Always false off Windows.
pub fn foreground(pid: u32) -> bool {
    #[cfg(windows)]
    return platform::foreground(pid);
    #[cfg(not(windows))]
    {
        let _ = pid;
        false
    }
}

/// End this Studio. It is not asked first: a place Rojo synced always has changes, so Studio would
/// only stop to ask about saving. Unsaved changes are lost; Studio's auto-recovery files stay.
pub fn close(pid: u32) -> io::Result<()> {
    crate::localnet::kill_process(pid)
}

/// The folders Studio writes auto-recovery files to.
fn recovery_folders() -> Vec<std::path::PathBuf> {
    let home = std::env::home_dir().unwrap_or_default();
    if cfg!(target_os = "macos") {
        let support = home.join("Library").join("Application Support").join("Roblox");
        return vec![
            support.join("RobloxStudio").join("AutoSaves"),
            home.join("Documents").join("ROBLOX").join("AutoSaves"),
        ];
    }
    let local = std::env::var_os("LOCALAPPDATA").map_or_else(|| home.join("AppData").join("Local"), Into::into);
    vec![
        local.join("Roblox").join("RobloxStudio").join("AutoSaves"),
        home.join("Documents").join("ROBLOX").join("AutoSaves"),
    ]
}

/// Whether `file` is one of Studio's auto-recovery copies of the place named `stem`:
/// "<stem>_AutoRecovery_<n>.rbxl", in any case.
fn is_recovery_of(file: &str, stem: &str) -> bool {
    let (file, stem) = (file.to_lowercase(), stem.to_lowercase());
    let number = file.strip_prefix(&stem).and_then(|rest| rest.strip_prefix("_autorecovery_")?.strip_suffix(".rbxl"));
    number.is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

/// Delete Studio's auto-recovery copies of a place file that is being deleted, which a Studio closed
/// by force leaves behind. A local place's name is unique, so only its own copies match.
pub fn remove_recovery(place: &Path) {
    let Some(stem) = place.file_stem().map(|stem| stem.to_string_lossy().into_owned()) else { return };
    for folder in recovery_folders() {
        for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
            if is_recovery_of(&entry.file_name().to_string_lossy(), &stem) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_places_own_recovery_files_match() {
        assert!(is_recovery_of("MyGame-1a2b3c4d_AutoRecovery_0.rbxl", "MyGame-1a2b3c4d"));
        assert!(is_recovery_of("mygame-1a2b3c4d_autorecovery_12.RBXL", "MyGame-1a2b3c4d"));
        assert!(!is_recovery_of("MyGame-1a2b3c4d_AutoRecovery_.rbxl", "MyGame-1a2b3c4d"));
        assert!(!is_recovery_of("MyGame-1a2b3c4d-2_AutoRecovery_0.rbxl", "MyGame-1a2b3c4d"));
        assert!(!is_recovery_of("MyGame-1a2b3c4d.rbxl", "MyGame-1a2b3c4d"));
    }
}
