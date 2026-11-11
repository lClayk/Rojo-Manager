//! macOS: the main thread runs NSApplication (the menu bar icon needs it) as an accessory app,
//! with no Dock icon, and is woken through the main dispatch queue.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate};

use crate::host::{Msg, post};

static ON_WAKE: OnceLock<fn()> = OnceLock::new();

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and AppDelegate does not implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RojoManagerAppDelegate"]
    struct AppDelegate;

    unsafe impl NSObjectProtocol for AppDelegate {}

    unsafe impl NSApplicationDelegate for AppDelegate {
        /// Opening the app again from Finder, Spotlight, or Launchpad while it runs shows the window.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_handle_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            post(Msg::Show);
            true
        }
    }
);

/// Set up the app on the main thread; `wake()` then calls `on_wake` on it.
pub fn init(on_wake: fn()) {
    let _ = ON_WAKE.set(on_wake);
    let mtm = MainThreadMarker::new().expect("the tray app runs on the main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // SAFETY: NSObject's init on a freshly allocated object.
    let delegate: Retained<AppDelegate> = unsafe { msg_send![AppDelegate::alloc(mtm), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // NSApplication holds its delegate weakly; this one lives as long as the app.
    std::mem::forget(delegate);
}

/// Add the folders Rokit, Aftman, Cargo, Homebrew, and similar installers use to PATH. An app
/// opened from Finder gets only the system's short PATH, which misses Rojo, agent CLIs, and the
/// node their scripts run on. Called first thing in main, before any other thread starts.
pub fn extend_path() {
    let home = std::env::home_dir().unwrap_or_default();
    let mut folders: Vec<PathBuf> =
        std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
    let usual = [".rokit/bin", ".aftman/bin", ".cargo/bin", ".local/bin"].map(|folder| home.join(folder));
    for folder in usual.into_iter().chain(["/opt/homebrew/bin".into(), "/usr/local/bin".into()]) {
        if !folders.contains(&folder) {
            folders.push(folder);
        }
    }
    if let Ok(path) = std::env::join_paths(folders) {
        // SAFETY: no other thread is running yet to read the environment at the same time.
        unsafe { std::env::set_var("PATH", path) };
    }
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
