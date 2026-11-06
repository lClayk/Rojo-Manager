//! Everything the tools reach outside this process, behind a trait so their logic runs against a fake
//! in tests.

use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use rojo_manager_core::config::Place;
use rojo_manager_core::control::{self, RequestError};
use rojo_manager_core::rojo_api;
use rojo_manager_core::studio::{self, ResolveError};
use rojo_manager_core::studio_mcp::{self, StudioMcpError};
use serde_json::{Map, Value};

pub trait Backend: Sync {
    /// Call the running manager's control API.
    fn control(
        &self,
        method: &str,
        parts: &[&str],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, RequestError>;
    /// Start Rojo Manager hidden, detached from this process.
    fn launch_manager(&self) -> io::Result<()>;
    /// Whether a Rojo server answers on this port; the manager's watcher can lag a restart.
    fn rojo_answers(&self, port: u16) -> bool;
    /// Call one robloxstudio-mcp tool.
    fn studio_call(&self, tool: &str, arguments: &Value) -> Result<Map<String, Value>, StudioMcpError>;
    /// Run plugin-level Luau in one Studio instance and return its decoded return value.
    fn run_luau(&self, instance_id: &str, code: &str) -> Result<Value, StudioMcpError>;
    /// Look up the place and universe IDs for a place ID or link.
    fn resolve_place(&self, text: &str) -> Result<Place, ResolveError>;
    /// Hand a place to Roblox Studio.
    fn open_place(&self, place: &Place) -> io::Result<()>;
    /// Wait between polls.
    fn sleep(&self, duration: Duration);
}

/// The real backend: Rojo Manager's core, Studio, and the clock.
pub struct Core;

impl Backend for Core {
    fn control(
        &self,
        method: &str,
        parts: &[&str],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        control::request(method, parts, body, timeout)
    }

    /// Runs RojoManager.exe from this executable's folder with --hidden.
    fn launch_manager(&self) -> io::Result<()> {
        let mut command = Command::new(std::env::current_exe()?.with_file_name("RojoManager.exe"));
        command.arg("--hidden").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        }
        command.spawn()?;
        Ok(())
    }

    fn rojo_answers(&self, port: u16) -> bool {
        rojo_api::server_info(port, Duration::from_secs(1)).is_some()
    }

    fn studio_call(&self, tool: &str, arguments: &Value) -> Result<Map<String, Value>, StudioMcpError> {
        studio_mcp::call(tool, arguments, Duration::from_secs(60))
    }

    fn run_luau(&self, instance_id: &str, code: &str) -> Result<Value, StudioMcpError> {
        studio_mcp::run_luau(instance_id, code)
    }

    fn resolve_place(&self, text: &str) -> Result<Place, ResolveError> {
        studio::resolve_place(text)
    }

    fn open_place(&self, place: &Place) -> io::Result<()> {
        studio::open_place(place)
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
