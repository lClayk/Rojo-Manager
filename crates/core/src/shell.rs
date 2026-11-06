//! Hand a folder, URL, or protocol link (roblox-studio:) to Windows (ShellExecuteW "open").

use std::io;

use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, PCWSTR, w};

pub fn open(target: &str) -> io::Result<()> {
    let target = HSTRING::from(target);
    // SAFETY: every string is NUL-terminated and outlives the call.
    let result = unsafe { ShellExecuteW(None, w!("open"), &target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    // Values above 32 mean success; Windows sets the last error otherwise.
    if result.0 as isize <= 32 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
