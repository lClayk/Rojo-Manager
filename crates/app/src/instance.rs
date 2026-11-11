//! One Rojo Manager per data folder. On Windows the first launch holds a named mutex and waits on
//! a named event; a later launch sets that event (asking the first to show its window) and exits.
//! On macOS a file lock plays the mutex and a Unix socket the event.

use crate::host::{self, Msg};

/// FNV-1a of the data folder, so isolated profiles (ROJO_MANAGER_DATA_DIR) get their own instance.
fn hash() -> u64 {
    let dir = rojo_manager_core::config::data_dir().to_string_lossy().to_lowercase();
    dir.bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// True when this is the first instance; it then forwards show requests as `Msg::Show`.
#[cfg(windows)]
pub fn claim(show: bool) -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    use windows::core::HSTRING;

    let hash = hash();
    let (mutex, event) = (format!(r"Local\RojoManager-{hash:016x}"), format!(r"Local\RojoManager-{hash:016x}-show"));
    // SAFETY: plain Win32 calls with owned strings; the handles live for the whole process.
    unsafe {
        let Ok(_mutex) = CreateMutexW(None, true, &HSTRING::from(mutex)) else { return true };
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if show && let Ok(handle) = OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(event)) {
                // This launch came from the person, so it may hand the foreground to the window.
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let _ = SetEvent(handle);
            }
            return false;
        }
        let Ok(handle) = CreateEventW(None, false, false, &HSTRING::from(event)) else { return true };
        let handle = handle.0 as usize;
        std::thread::spawn(move || {
            let handle = windows::Win32::Foundation::HANDLE(handle as *mut _);
            while WaitForSingleObject(handle, INFINITE).0 == 0 {
                host::post(Msg::Show);
            }
        });
    }
    true
}

/// True when this is the first instance; it then forwards show requests as `Msg::Show`.
#[cfg(target_os = "macos")]
pub fn claim(show: bool) -> bool {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::{UnixListener, UnixStream};

    let base = std::env::temp_dir().join(format!("RojoManager-{:016x}", hash()));
    let socket = base.with_extension("sock");
    let Ok(lock) = std::fs::File::create(base.with_extension("lock")) else { return true };
    // SAFETY: a plain call on an open file. The lock lasts as long as this process.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        if show && let Ok(mut first) = UnixStream::connect(&socket) {
            let _ = first.write_all(b"show");
        }
        return false;
    }
    std::mem::forget(lock);
    // A socket left by an instance that has exited answers nothing; replace it.
    let _ = std::fs::remove_file(&socket);
    let Ok(listener) = UnixListener::bind(&socket) else { return true };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut request = String::new();
            if stream.take(16).read_to_string(&mut request).is_ok() && request == "show" {
                host::post(Msg::Show);
            }
        }
    });
    true
}
