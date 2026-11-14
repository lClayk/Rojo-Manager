//! Rojo Manager's engine, shared by the tray app, the MCP server, and the tool installer.
//! Nothing here depends on the UI, so the app runs fully with no window loaded.
//!
//! Errors that reach people (toasts, agent replies) are plain `String` messages written for them.

pub mod config;
pub mod control;
pub mod https;
pub mod localnet;
pub mod manager;
pub mod rojo_api;
pub mod shell;
pub mod startup;
pub mod studio;
pub mod studio_link;
pub mod tooling;
pub mod updates;

/// The app version, read by every executable.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Hides the console window a GUI process would otherwise flash when it starts a console program.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `name` as an executable file name: "name.exe" on Windows, "name" elsewhere.
pub fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Point the data folder at a temporary folder shared by the tests, one test at a time.
#[cfg(test)]
pub(crate) fn isolated() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: Mutex<()> = Mutex::new(());
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
    // SAFETY: only tests change the environment, and they hold LOCK.
    unsafe { std::env::set_var("ROJO_MANAGER_DATA_DIR", dir.path()) };
    guard
}
