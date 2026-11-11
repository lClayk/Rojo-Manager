//! macOS: launch at sign-in through a LaunchAgent in ~/Library/LaunchAgents, which launchd runs
//! once when the user logs in.

use std::io;
use std::path::{Path, PathBuf};

const LABEL: &str = "com.lclayk.rojomanager";

fn agent() -> PathBuf {
    std::env::home_dir().unwrap_or_default().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

pub fn startup_enabled() -> bool {
    agent().is_file()
}

/// Enable or disable launching this executable at sign-in, with --hidden when `hidden`.
pub fn set_startup(enabled: bool, hidden: bool) -> io::Result<()> {
    let plist = if enabled { Some(plist(&std::env::current_exe()?, hidden)) } else { None };
    set_at(&agent(), plist.as_deref())
}

/// Windows search needs a Start menu shortcut; Spotlight finds the app bundle on its own.
pub fn add_to_start_menu() -> io::Result<()> {
    Ok(())
}

/// Write the agent, or remove it when None.
fn set_at(agent: &Path, plist: Option<&str>) -> io::Result<()> {
    match plist {
        Some(plist) => {
            if let Some(folder) = agent.parent() {
                std::fs::create_dir_all(folder)?;
            }
            std::fs::write(agent, plist)
        }
        None => match std::fs::remove_file(agent) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        },
    }
}

fn plist(exe: &Path, hidden: bool) -> String {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let mut arguments = format!("<string>{}</string>", escape(&exe.to_string_lossy()));
    if hidden {
        arguments.push_str("<string>--hidden</string>");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array>{arguments}</array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_runs_the_executable_and_round_trips() {
        let text = plist(Path::new("/Applications/Rojo & Co.app/Contents/MacOS/RojoManager"), true);
        assert!(text.contains(
            "<array><string>/Applications/Rojo &amp; Co.app/Contents/MacOS/RojoManager</string><string>--hidden</string></array>"
        ));
        // A scratch folder, so the real LaunchAgents folder is never touched.
        let folder = tempfile::tempdir().unwrap();
        let agent = folder.path().join("LaunchAgents").join("test.plist");
        set_at(&agent, Some(&text)).unwrap();
        assert!(agent.is_file());
        set_at(&agent, None).unwrap();
        assert!(!agent.exists());
        set_at(&agent, None).unwrap(); // already off
    }
}
