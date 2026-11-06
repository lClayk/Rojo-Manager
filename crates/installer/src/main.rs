//! ToolInstaller: run one tool job from a request file in a console.
//!
//! Usage: `ToolInstaller <request.json>`. The outcome is published to result.json beside the
//! request before the window waits to be closed, so the app learns it right away.

use std::io::{self, Write};
use std::path::Path;

use rojo_manager_core::tooling::{self, Job};
use serde_json::json;

fn main() {
    let Some(request) = std::env::args_os().nth(1) else {
        eprintln!("Usage: ToolInstaller <request.json>");
        std::process::exit(2);
    };
    let request = Path::new(&request);
    let result = run(request);
    let (Ok(message) | Err(message)) = &result;
    println!("\n{message}");
    if let Err(error) = publish(request, &result) {
        eprintln!("Could not save the installation result: {error}");
    }
    print!("\nPress Enter to close this window.");
    let _ = io::stdout().flush();
    let _ = io::stdin().read_line(&mut String::new());
}

fn run(request: &Path) -> Result<String, String> {
    let data = std::fs::read(request).map_err(|e| e.to_string())?;
    let job: Job = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    tooling::execute_job(&job)
}

/// Write {"ok", "message"} to result.json beside the request, atomically through result.tmp.
fn publish(request: &Path, result: &Result<String, String>) -> io::Result<()> {
    let (ok, message) = match result {
        Ok(message) => (true, message),
        Err(message) => (false, message),
    };
    let temporary = request.with_file_name("result.tmp");
    std::fs::write(&temporary, json!({"ok": ok, "message": message}).to_string())?;
    std::fs::rename(&temporary, request.with_file_name("result.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_is_published_beside_the_request() {
        let folder = tempfile::tempdir().unwrap();
        let request = folder.path().join("request.json");
        std::fs::write(&request, r#"{"operation": "unknown", "tool": "Rojo"}"#).unwrap();
        let result = run(&request);
        assert!(result.is_err());
        publish(&request, &Err("download failed".into())).unwrap();
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(folder.path().join("result.json")).unwrap()).unwrap();
        assert_eq!(written, json!({"ok": false, "message": "download failed"}));
        assert!(!folder.path().join("result.tmp").exists());
    }
}
