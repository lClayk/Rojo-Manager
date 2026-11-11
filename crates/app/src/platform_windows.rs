//! Windows: a plain Win32 message loop (the tray icon needs one), woken through a message-only
//! window.

use std::sync::OnceLock;
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WNDCLASSW,
};
use windows::core::w;

/// The message window and what to call when it is woken.
static LOOP: OnceLock<(usize, fn())> = OnceLock::new();

/// Set up the loop on this thread; `wake()` then calls `on_wake` on it.
pub fn init(on_wake: fn()) {
    // SAFETY: registers and creates a message-only window owned by this thread.
    let hwnd = unsafe {
        let class = w!("RojoManagerHost");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() });
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    let hwnd = hwnd.expect("the message window can be created");
    let _ = LOOP.set((hwnd.0 as usize, on_wake));
}

/// Callable from any thread.
pub fn wake() {
    if let Some((hwnd, _)) = LOOP.get() {
        // SAFETY: the message window lives for the whole process.
        unsafe {
            let _ = PostMessageW(Some(HWND(*hwnd as *mut _)), WM_APP, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg != WM_APP {
        // SAFETY: forwards this window's own message.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    if let Some((_, on_wake)) = LOOP.get() {
        on_wake();
    }
    LRESULT(0)
}

/// Run the loop until `quit()`.
pub fn run() {
    let mut msg = MSG::default();
    // SAFETY: the standard message loop on the thread that owns the tray and message windows.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

pub fn quit() {
    // SAFETY: ends this thread's message loop.
    unsafe { PostQuitMessage(0) };
}

/// Let the window process take the foreground, which this process may have just been given.
pub fn allow_foreground() {
    // SAFETY: plain call.
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// An error dialog, for when the app cannot start.
pub fn show_error(title: &str, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    // SAFETY: both strings outlive the call, and no owner window is needed.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR);
    }
}

/// Wait up to `timeout` for a process to exit; returns at once if it already has.
pub fn wait_for_exit(pid: u32, timeout: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: the handle is checked, waited on, and closed here.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(process, timeout.as_millis() as u32);
            let _ = CloseHandle(process);
        }
    }
}
