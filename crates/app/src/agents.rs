//! Adding the rojo-manager MCP server to agents: one click for agents whose CLI has `mcp add`,
//! and a config snippet for every other MCP client.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const NAME: &str = "rojo-manager";

/// An agent whose CLI registers MCP servers itself, so the app never edits its config files.
#[derive(Clone, Copy)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    fn cli(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    /// Claude Code adds servers for the current folder only unless told otherwise.
    fn scope(self) -> &'static [&'static str] {
        match self {
            Agent::Claude => &["--scope", "user"],
            Agent::Codex => &[],
        }
    }

    /// The command to run by hand when the CLI is not on PATH.
    pub fn command_line(self, path: &str) -> String {
        let scope: String = self.scope().iter().map(|arg| format!("{arg} ")).collect();
        format!("{} mcp add {scope}{NAME} -- \"{path}\"", self.cli())
    }
}

pub enum Failure {
    /// The agent's CLI is not on PATH.
    Missing,
    Failed(String),
}

/// Registers rojo-manager with the agent. An existing entry is replaced, so a moved app gets its new path.
pub fn add(agent: Agent, path: &str) -> Result<(), Failure> {
    let cli = find(agent.cli()).ok_or(Failure::Missing)?;
    let scope = agent.scope();
    // Fails when there is nothing to remove, which is fine.
    let _ = run(&cli, &[&["mcp", "remove"], scope, &[NAME]].concat());
    let output = run(&cli, &[&["mcp", "add"], scope, &[NAME, "--", path]].concat())
        .map_err(|e| Failure::Failed(format!("Could not run {}: {e}", agent.cli())))?;
    if output.status.success() {
        return Ok(());
    }
    let text = [output.stderr, output.stdout].concat();
    let reason = String::from_utf8_lossy(&text)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned();
    Err(Failure::Failed(format!("{} mcp add failed: {reason}", agent.cli())))
}

/// The block Claude Desktop, Cursor, Windsurf, and most other MCP clients take in their config file.
pub fn config_snippet(path: &str) -> String {
    let config = serde_json::json!({ "mcpServers": { NAME: { "command": path } } });
    serde_json::to_string_pretty(&config).unwrap_or_default()
}

/// The CLI on PATH; npm installs it as a .cmd shim, other installers as an .exe.
fn find(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| ["exe", "cmd", "bat"].map(|ext| dir.join(format!("{name}.{ext}"))))
        .find(|file| file.is_file())
}

fn run(cli: &PathBuf, args: &[&str]) -> std::io::Result<Output> {
    let mut command = Command::new(cli);
    command.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, rojo_manager_core::CREATE_NO_WINDOW);
    command.output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_scopes_claude_to_the_user() {
        let path = r"C:\Apps\Rojo Manager\RojoManagerMCP.exe";
        assert_eq!(
            Agent::Claude.command_line(path),
            r#"claude mcp add --scope user rojo-manager -- "C:\Apps\Rojo Manager\RojoManagerMCP.exe""#
        );
        assert_eq!(
            Agent::Codex.command_line(path),
            r#"codex mcp add rojo-manager -- "C:\Apps\Rojo Manager\RojoManagerMCP.exe""#
        );
    }

    #[test]
    fn config_snippet_escapes_the_path() {
        let snippet = config_snippet(r"C:\Apps\RojoManagerMCP.exe");
        let parsed: serde_json::Value = serde_json::from_str(&snippet).unwrap();
        assert_eq!(parsed["mcpServers"]["rojo-manager"]["command"], r"C:\Apps\RojoManagerMCP.exe");
    }
}
