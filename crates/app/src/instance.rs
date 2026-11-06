//! One Rojo Manager per data folder. The first launch holds a named mutex and waits on a named
//! event; a later launch sets that event (asking the first to show its window) and exits.

use crate::host::{self, Msg};

/// FNV-1a of the data folder, so isolated profiles (ROJO_MANAGER_DATA_DIR) get their own instance.
fn names() -> (String, String) {
    let dir = rojo_manager_core::config::data_dir().to_string_lossy().to_lowercase();
    let hash = dir
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3));
    (format!("Local\\RojoManager-{hash:016x}"), format!("Local\\RojoManager-{hash:016x}-show"))
}

/// True when this is the first instance; it then forwards show requests as `Msg::Show`.
pub fn claim(show: bool) -> bool {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, SetEvent, WaitForSingleObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow};
    use windows::core::HSTRING;

    let (mutex, event) = names();
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
