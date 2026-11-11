//! Hand a folder, URL, or protocol link (roblox-studio:) to the system: ShellExecuteW "open" on
//! Windows, `open` on macOS.

use std::io;

#[cfg(windows)]
pub fn open(target: &str) -> io::Result<()> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    let target = HSTRING::from(target);
    // SAFETY: every string is NUL-terminated and outlives the call.
    let result = unsafe { ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // Values above 32 mean success; Windows sets the last error otherwise.
    if result.0 as isize <= 32 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn open(target: &str) -> io::Result<()> {
    let status = std::process::Command::new("open").arg(target).status()?;
    if status.success() { Ok(()) } else { Err(io::Error::other(format!("open exited with {status}"))) }
}
