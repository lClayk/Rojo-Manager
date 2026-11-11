//! macOS: the main thread runs NSApplication (the menu bar icon needs it) as an accessory app,
//! with no Dock icon, and is woken through the main dispatch queue.

use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

static ON_WAKE: OnceLock<fn()> = OnceLock::new();

/// Set up the app on the main thread; `wake()` then calls `on_wake` on it.
pub fn init(on_wake: fn()) {
    let _ = ON_WAKE.set(on_wake);
    let mtm = MainThreadMarker::new().expect("the tray app runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

/// Callable from any thread. Work queued before `run()` runs once the app is running.
pub fn wake() {
    DispatchQueue::main().exec_async(|| {
        if let Some(on_wake) = ON_WAKE.get() {
            on_wake();
        }
    });
}

/// Run the app until `quit()`.
pub fn run() {
    let mtm = MainThreadMarker::new().expect("the tray app runs on the main thread");
    NSApplication::sharedApplication(mtm).run();
}

/// The host has shut everything down by now.
pub fn quit() {
    std::process::exit(0);
}

/// macOS lets the window process activate itself.
pub fn allow_foreground() {}

/// AppleScript string literal.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An error dialog, for when the app cannot start.
pub fn show_error(title: &str, text: &str) {
    let script = format!("display alert {} message {} as critical", quoted(title), quoted(text));
    let _ = Command::new("osascript").arg("-e").arg(script).status();
}

/// A notification in Notification Center.
pub fn notify(title: &str, text: &str) {
    let script = format!("display notification {} with title {}", quoted(text), quoted(title));
    let _ = Command::new("osascript").arg("-e").arg(script).spawn();
}

/// Wait up to `timeout` for a process to exit; returns at once if it already has.
pub fn wait_for_exit(pid: u32, timeout: Duration) {
    let end = Instant::now() + timeout;
    // SAFETY: signal 0 only checks that the process exists.
    while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(100));
    }
}
