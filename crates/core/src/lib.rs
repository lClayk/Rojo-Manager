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
