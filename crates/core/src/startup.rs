//! Launch at sign-in: the Run key on Windows (startup_windows.rs), a LaunchAgent on macOS
//! (startup_macos.rs). Both expose `startup_enabled()`, `set_startup(enabled, hidden)`, and
//! `add_to_start_menu()`, which only Windows needs.

#[cfg(target_os = "macos")]
#[path = "startup_macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "startup_windows.rs"]
mod platform;

pub use platform::{add_to_start_menu, set_startup, startup_enabled};
