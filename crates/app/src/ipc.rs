//! The window process talks to the tray app over its stdin and stdout, one JSON message per line.
//! The window sends engine calls; the tray app answers each by id and pushes engine events.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use rojo_manager_core::config::Place;
use rojo_manager_core::manager::{Engine, Event, ProjectView, Snapshot, Update};
use rojo_manager_core::tooling::Job;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The engine calls the window makes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum Call {
    Snapshot,
    Logs {
        name: String,
    },
    Toggle {
        name: String,
    },
    StartAll,
    StopAll,
    PortOwner {
        name: String,
    },
    KillPortOwner {
        name: String,
        pid: u32,
    },
    AddProjects {
        path: PathBuf,
    },
    Update {
        name: String,
        update: Update,
    },
    Remove {
        name: String,
    },
    OpenStudio {
        name: String,
    },
    OpenFolder {
        name: String,
    },
    SetSetting {
        key: String,
        value: bool,
    },
    SetCollapsed {
        universe_id: u64,
        collapsed: bool,
    },
    CheckRojo,
    CheckAppUpdate,
    RefreshToolVersions,
    RunToolJob {
        job: Job,
    },
    /// Download and swap in the latest release; the tray app restarts into it on success.
    InstallAppUpdate,
    /// The window is closing. The tray app handles this itself: a show request from now on opens
    /// a new window instead of reaching this one while its process winds down.
    Closing,
}

/// Window to tray app.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

/// Tray app to window.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToWindow {
    Reply {
        id: u64,
        result: Result<Value, String>,
    },
    Event(Event),
    /// A second launch or the tray asked to show the window, which is already open.
    Activate,
    /// The tray's update item: ask before installing, as the window's own button does.
    ConfirmUpdate,
}

pub fn line(message: &impl Serialize) -> String {
    serde_json::to_string(message).expect("IPC messages serialize")
}

/// Run one call on the tray app's engine.
pub fn serve(engine: &Arc<Engine>, call: Call) -> Result<Value, String> {
    fn json<T: Serialize>(result: Result<T, String>) -> Result<Value, String> {
        result.and_then(|value| serde_json::to_value(value).map_err(|error| error.to_string()))
    }
    match call {
        Call::Snapshot => json(Ok(engine.snapshot())),
        Call::Logs { name } => json(Ok(engine.logs(&name))),
        Call::Toggle { name } => json(engine.toggle(&name)),
        Call::PortOwner { name } => json(engine.port_owner(&name)),
        Call::KillPortOwner { name, pid } => json(engine.kill_port_owner(&name, pid)),
        Call::AddProjects { path } => json(engine.add_projects(&path)),
        Call::Update { name, update } => json(engine.update(&name, update)),
        Call::Remove { name } => json(engine.remove(&name)),
        Call::OpenStudio { name } => json(engine.open_studio(&name)),
        Call::OpenFolder { name } => json(engine.open_folder(&name)),
        Call::SetSetting { key, value } => json(engine.set_setting(&key, value)),
        Call::SetCollapsed { universe_id, collapsed } => json(engine.set_collapsed(universe_id, collapsed)),
        Call::RunToolJob { job } => json(engine.run_tool_job(job)),
        Call::InstallAppUpdate => json(engine.install_app_update()),
        // Calls whose answer nobody waits for.
        Call::StartAll => {
            engine.start_all();
            Ok(Value::Null)
        }
        Call::StopAll => {
            engine.stop_all();
            Ok(Value::Null)
        }
        Call::CheckRojo => {
            engine.check_rojo(true);
            Ok(Value::Null)
        }
        Call::CheckAppUpdate => {
            engine.check_app_update(true);
            Ok(Value::Null)
        }
        Call::RefreshToolVersions => {
            engine.refresh_tool_versions();
            Ok(Value::Null)
        }
        Call::Closing => Ok(Value::Null),
    }
}

type Pending = Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>;

/// The window's handle on the engine in the tray app. Methods mirror `Engine`'s; the ones that
/// return a value block until the tray app answers, so call them off the UI thread.
pub struct Client {
    out: Mutex<std::io::Stdout>,
    next: AtomicU64,
    pending: Arc<Pending>,
}

impl Client {
    /// Read the tray app's messages on a thread: replies complete calls, the rest go to `on_message`.
    /// The window process exits when the tray app goes away.
    pub fn start(on_message: impl Fn(ToWindow) + Send + 'static) -> Arc<Client> {
        let pending: Arc<Pending> = Arc::default();
        let replies = Arc::clone(&pending);
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                match serde_json::from_str::<ToWindow>(&line) {
                    Ok(ToWindow::Reply { id, result }) => {
                        if let Some(reply) = replies.lock().unwrap().remove(&id) {
                            let _ = reply.send(result);
                        }
                    }
                    Ok(message) => on_message(message),
                    Err(error) => eprintln!("Unreadable message from Rojo Manager: {error}"),
                }
            }
            std::process::exit(0);
        });
        Arc::new(Client { out: Mutex::new(std::io::stdout()), next: AtomicU64::new(1), pending })
    }

    fn write(&self, id: u64, call: Call) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{}", line(&Request { id, call })).and_then(|()| out.flush());
    }

    /// A call whose answer nobody waits for.
    fn send(&self, call: Call) {
        self.write(self.next.fetch_add(1, Ordering::Relaxed), call);
    }

    fn call<T: DeserializeOwned>(&self, call: Call) -> Result<T, String> {
        let (reply, answer) = mpsc::channel();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        // Registered before sending so a fast reply finds it.
        self.pending.lock().unwrap().insert(id, reply);
        self.write(id, call);
        let value = answer.recv().map_err(|_| "Rojo Manager stopped".to_string())??;
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    pub fn snapshot(&self) -> Result<Snapshot, String> {
        self.call(Call::Snapshot)
    }
    pub fn logs(&self, name: &str) -> Result<Vec<(u64, String)>, String> {
        self.call(Call::Logs { name: name.into() })
    }
    pub fn toggle(&self, name: &str) -> Result<(), String> {
        self.call(Call::Toggle { name: name.into() })
    }
    pub fn start_all(&self) {
        self.send(Call::StartAll);
    }
    pub fn stop_all(&self) {
        self.send(Call::StopAll);
    }
    pub fn port_owner(&self, name: &str) -> Result<Option<(u32, String)>, String> {
        self.call(Call::PortOwner { name: name.into() })
    }
    pub fn kill_port_owner(&self, name: &str, pid: u32) -> Result<(), String> {
        self.call(Call::KillPortOwner { name: name.into(), pid })
    }
    pub fn add_projects(&self, path: &Path) -> Result<Vec<String>, String> {
        self.call(Call::AddProjects { path: path.into() })
    }
    pub fn update(&self, name: &str, update: Update) -> Result<ProjectView, String> {
        self.call(Call::Update { name: name.into(), update })
    }
    pub fn remove(&self, name: &str) -> Result<(), String> {
        self.call(Call::Remove { name: name.into() })
    }
    pub fn open_studio(&self, name: &str) -> Result<Place, String> {
        self.call(Call::OpenStudio { name: name.into() })
    }
    pub fn open_folder(&self, name: &str) -> Result<(), String> {
        self.call(Call::OpenFolder { name: name.into() })
    }
    pub fn set_setting(&self, key: &str, value: bool) -> Result<(), String> {
        self.call(Call::SetSetting { key: key.into(), value })
    }
    pub fn set_collapsed(&self, universe_id: u64, collapsed: bool) -> Result<(), String> {
        self.call(Call::SetCollapsed { universe_id, collapsed })
    }
    pub fn check_rojo(&self) {
        self.send(Call::CheckRojo);
    }
    pub fn check_app_update(&self) {
        self.send(Call::CheckAppUpdate);
    }
    pub fn refresh_tool_versions(&self) {
        self.send(Call::RefreshToolVersions);
    }
    pub fn run_tool_job(&self, job: Job) -> Result<(), String> {
        self.call(Call::RunToolJob { job })
    }
    pub fn install_app_update(&self) -> Result<String, String> {
        self.call(Call::InstallAppUpdate)
    }
    pub fn closing(&self) {
        self.send(Call::Closing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updates_keep_the_difference_between_unchanged_and_unlinked_places() {
        let round_trip = |update: Update| -> Update {
            let request = line(&Request { id: 7, call: Call::Update { name: "A".into(), update } });
            match serde_json::from_str::<Request>(&request).unwrap().call {
                Call::Update { update, .. } => update,
                call => panic!("{call:?}"),
            }
        };
        assert_eq!(round_trip(Update { port: Some(1), ..Update::default() }).place, None);
        assert_eq!(round_trip(Update { place: Some(None), ..Update::default() }).place, Some(None));
        let place = Place { place_id: 2, universe_id: 3 };
        assert_eq!(round_trip(Update { place: Some(Some(place)), ..Update::default() }).place, Some(Some(place)));
    }
}
