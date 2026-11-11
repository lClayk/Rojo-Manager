//! Local control API that lets RojoManagerMCP drive the running manager. The wire
//! format is 0.5's, so 0.5's MCP server and this one are interchangeable.
//!
//! The manager serves it on a random loopback port and records the port and a random token in
//! control.json beside the settings file. Requests need that token, and browser requests (which
//! carry an Origin header) are refused, so only local programs run by this user can reach it.
//!
//! AutoConnectServer is the one exception: the Rojo bridge in Studio cannot read control.json, so it
//! polls a fixed port without a token. It reports its place and Rojo's state there, and receives the
//! auto-connect server and the commands agents send it through this API. Bridges in Edit mode and in
//! playtest servers also send Studio's Output there.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub const ACTIONS: [&str; 7] = ["start", "stop", "restart", "open-studio", "link-place", "update", "remove"];
pub const MAX_BODY: usize = 16384;
/// A bridge poll can carry a command's result, such as the output of agent Luau.
pub const MAX_BRIDGE_BODY: usize = 1024 * 1024;
/// Fixed so the bridge can find it; project ports start at 34872.
pub const AUTO_CONNECT_PORT: u16 = 34871;

/// A request the manager understood but refused; `status` is the HTTP status to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlError {
    pub message: String,
    pub status: u16,
}

impl ControlError {
    pub fn new(message: impl Into<String>, status: u16) -> Self {
        ControlError { message: message.into(), status }
    }

    /// 400 Bad Request.
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(message, 400)
    }
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What the servers call. Implementations are thread-safe; requests arrive on server threads.
///
/// - GET  /projects                  {"projects": projects()}
/// - POST /projects                  act(None, "add", body)
/// - POST /projects/<name>/<action>  act(Some(name), action, body), action in ACTIONS
/// - GET  /projects/<name>/logs      logs(name)
/// - GET  /settings, POST /settings  settings(None) or settings(Some(body))
/// - GET  /updates, POST /updates/<target>  updates(None) or updates(Some(target)), target app or bridge
/// - GET  /studio, POST /studio/<instanceId>  studio(None) or studio(Some((instanceId, body)))
/// - GET  /rojo-plugins/<count>      report_rojo_plugins(count) (AutoConnectServer, no token)
/// - POST /bridge/<id>               poll_bridge(id, body) (AutoConnectServer, no token)
/// - POST /output                    report_output(body) (AutoConnectServer, no token)
///
/// Bodies are JSON objects ({} when empty). Errors reply {"error": message} with their status.
pub trait ControlHandler: Send + Sync + 'static {
    fn projects(&self) -> Vec<Value>;
    fn act(&self, project: Option<&str>, action: &str, body: &Map<String, Value>) -> Result<Value, ControlError>;
    fn settings(&self, changes: Option<&Map<String, Value>>) -> Result<Value, ControlError>;
    /// {"project", "state", "lines"}: the project's recent Rojo output, oldest first.
    fn logs(&self, project: &str) -> Result<Value, ControlError>;
    /// How many Rojo plugins the bridge found running in Studio.
    fn report_rojo_plugins(&self, count: u32);
    /// A bridge's poll. Answers {"port", "accept"}: the place's auto-connect server (null for none)
    /// and whether to accept Rojo's sync prompt, plus "command" once one is queued for this bridge.
    fn poll_bridge(&self, id: u64, poll: BridgePoll) -> Value;
    /// New lines of Studio's Output from a bridge.
    fn report_output(&self, output: StudioOutput);
    /// {"bridge", "instances"}: whether the bridge is installed and the Studio windows it reports from;
    /// or runs one command ({"action", ...}) in one of them and returns its result.
    fn studio(&self, command: Option<(&str, &Map<String, Value>)>) -> Result<Value, ControlError>;
    /// {"app", "bridge"}: whether each is up to date, or installs one and says what happened.
    fn updates(&self, install: Option<&str>) -> Result<Value, ControlError>;
}

/// A bridge's poll: its place, the server its Rojo is connected to (0 for none), the Rojo plugin's
/// status, and the result of the command the previous poll handed it.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgePoll {
    pub place_id: u64,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub rojo: Value,
    #[serde(default)]
    pub result: Option<CommandResult>,
}

/// A command's outcome: its return value, or the error message when not `ok`.
#[derive(Debug, Deserialize)]
pub struct CommandResult {
    pub id: u64,
    pub ok: bool,
    #[serde(default)]
    pub value: Value,
}

/// New lines of Studio's Output, as a bridge in one of a place's DataModels sends them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioOutput {
    pub place_id: u64,
    pub context: OutputContext,
    pub lines: Vec<OutputReport>,
}

/// The DataModel a line of Output came from: Edit mode, or a playtest's server. A playtest's client
/// cannot make HTTP requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputContext {
    Edit,
    Server,
}

/// A line's MessageType, least severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputKind {
    Output,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Deserialize)]
pub struct OutputReport {
    pub text: String,
    pub kind: OutputKind,
}

pub fn control_path() -> PathBuf {
    crate::config::data_dir().join("control.json")
}

/// The token-protected API on a random 127.0.0.1 port, served on background threads. Starting
/// writes control.json ({port, token, pid}) atomically; closing removes it if it is still ours.
pub struct ControlServer {
    pub port: u16,
    pub token: String,
    served: Served,
}

impl ControlServer {
    pub fn start(handler: Arc<dyn ControlHandler>) -> io::Result<ControlServer> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let token = new_token()?;
        let expected = token.clone();
        let served = Served::start(listener, WORKERS, move |request| match route(&*handler, &expected, request) {
            Ok(reply) => (200, reply),
            Err(error) => (error.status, json!({ "error": error.message })),
        })?;
        write_control_file(port, &token)?;
        Ok(ControlServer { port, token, served })
    }

    /// Stop serving (requests in progress still finish), then remove control.json unless another
    /// manager instance has taken it over since.
    pub fn close(self) {
        drop(self.served);
        let path = control_path();
        let stored = std::fs::read(&path).ok().and_then(|data| serde_json::from_slice::<Value>(&data).ok());
        if stored.is_some_and(|info| info.get("token").and_then(Value::as_str) == Some(self.token.as_str())) {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Answers the bridges' polls and plugin counts on 127.0.0.1:`port`. Binding fails when another
/// program, or another Rojo Manager, holds the port (no address reuse).
pub struct AutoConnectServer {
    served: Served,
}

impl AutoConnectServer {
    pub fn start(handler: Arc<dyn ControlHandler>, port: u16) -> io::Result<AutoConnectServer> {
        // std sets no SO_REUSEADDR on Windows, so binding a taken port fails here.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        // Each open Studio holds one worker with its poll, so this serves more at once.
        let served = Served::start(listener, 16, move |request| {
            if header(request, "Origin").is_some() {
                return (404, json!({}));
            }
            let post = *request.method() == tiny_http::Method::Post;
            let url = request_path(request).trim_matches('/').to_string();
            let (route, rest) = url.split_once('/').unwrap_or((url.as_str(), ""));
            let number = rest.bytes().all(|b| b.is_ascii_digit()).then(|| rest.parse::<u64>().ok()).flatten();
            match (post, route, number) {
                (false, "rojo-plugins", Some(count)) => {
                    handler.report_rojo_plugins(u32::try_from(count).unwrap_or(u32::MAX));
                    (200, json!({}))
                }
                (true, "output", None) if rest.is_empty() => {
                    let output = body(request, MAX_BRIDGE_BODY).and_then(|body| {
                        serde_json::from_value(Value::Object(body)).map_err(|e| ControlError::bad(e.to_string()))
                    });
                    match output {
                        Ok(output) => {
                            handler.report_output(output);
                            (200, json!({}))
                        }
                        Err(error) => (error.status, json!({ "error": error.message })),
                    }
                }
                (true, "bridge", Some(id)) => {
                    let poll = body(request, MAX_BRIDGE_BODY).and_then(|body| {
                        serde_json::from_value(Value::Object(body)).map_err(|e| ControlError::bad(e.to_string()))
                    });
                    match poll {
                        Ok(poll) => (200, handler.poll_bridge(id, poll)),
                        Err(error) => (error.status, json!({ "error": error.message })),
                    }
                }
                _ => (404, json!({})),
            }
        })?;
        Ok(AutoConnectServer { served })
    }

    /// Stop serving; requests in progress still finish, and the port is free once they have.
    pub fn close(self) {
        drop(self.served);
    }
}

/// Worker threads of the control API. Handlers may block (a Rojo start, a Studio command), so a few
/// run at once.
const WORKERS: usize = 4;

/// A tiny_http server answering every request with JSON from `answer` on worker threads.
struct Served {
    server: Arc<tiny_http::Server>,
    workers: usize,
}

impl Served {
    fn start(
        listener: TcpListener,
        workers: usize,
        answer: impl Fn(&mut tiny_http::Request) -> (u16, Value) + Send + Sync + 'static,
    ) -> io::Result<Served> {
        let server = Arc::new(tiny_http::Server::from_listener(listener, None).map_err(io::Error::other)?);
        let served = Served { server, workers };
        let answer = Arc::new(answer);
        for _ in 0..workers {
            let (server, answer) = (served.server.clone(), answer.clone());
            thread::Builder::new().name("control".into()).spawn(move || {
                while let Ok(mut request) = server.recv() {
                    // A panicking handler fails its request, not the worker.
                    let answered = panic::catch_unwind(AssertUnwindSafe(|| answer(&mut request)));
                    let (status, body) = answered.unwrap_or_else(|_| (500, json!({ "error": "Internal error" })));
                    reply(request, status, &body);
                }
            })?;
        }
        Ok(served)
    }
}

impl Drop for Served {
    /// Idle workers stop at once and busy ones after their request, without waiting here: a
    /// handler may need a lock the closing thread holds. The last worker out closes the listener.
    fn drop(&mut self) {
        for _ in 0..self.workers {
            self.server.unblock();
        }
    }
}

/// Send the whole reply in one write; split writes stall on Nagle plus delayed ACKs.
fn reply(request: tiny_http::Request, status: u16, body: &Value) {
    let data = body.to_string();
    let reason = tiny_http::StatusCode(status).default_reason_phrase();
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    );
    let mut writer = request.into_writer();
    let _ = writer.write_all((head + &data).as_bytes()).and_then(|()| writer.flush());
}

/// The request's path. HTTP/1.1 servers must also accept the absolute form a client may send
/// ("http://localhost:34871/bridge/1"), as Lune's HTTP client does.
fn request_path(request: &tiny_http::Request) -> &str {
    let url = request.url();
    match url.split_once("://") {
        Some((_, rest)) if !url.starts_with('/') => rest.find('/').map_or("", |slash| &rest[slash..]),
        _ => url,
    }
}

fn header<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
    request.headers().iter().find(|header| header.field.equiv(name)).map(|header| header.value.as_str())
}

/// Check the Origin and token, then dispatch to the handler.
fn route(handler: &dyn ControlHandler, token: &str, request: &mut tiny_http::Request) -> Result<Value, ControlError> {
    if header(request, "Origin").is_some() {
        return Err(ControlError::new("Browser requests are not allowed", 403));
    }
    let supplied = header(request, "Authorization").unwrap_or_default();
    if !same_bytes(supplied.strip_prefix("Bearer ").unwrap_or(supplied).as_bytes(), token.as_bytes()) {
        return Err(ControlError::new("Missing or invalid token", 401));
    }
    let parts: Vec<String> = request_path(request).trim_matches('/').split('/').map(percent_decode).collect();
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    let get = match request.method() {
        tiny_http::Method::Get => true,
        tiny_http::Method::Post => false,
        _ => return Err(ControlError::new("Unknown route", 404)),
    };
    match (get, parts.as_slice()) {
        (true, ["projects"]) => Ok(json!({ "projects": handler.projects() })),
        (false, ["projects"]) => handler.act(None, "add", &body(request, MAX_BODY)?),
        (false, ["projects", name, action]) if ACTIONS.contains(action) => {
            handler.act(Some(name), action, &body(request, MAX_BODY)?)
        }
        (true, ["projects", name, "logs"]) => handler.logs(name),
        (true, ["settings"]) => handler.settings(None),
        (false, ["settings"]) => handler.settings(Some(&body(request, MAX_BODY)?)),
        (true, ["studio"]) => handler.studio(None),
        (false, ["studio", instance]) => handler.studio(Some((instance, &body(request, MAX_BRIDGE_BODY)?))),
        (true, ["updates"]) => handler.updates(None),
        (false, ["updates", target]) => handler.updates(Some(target)),
        _ => Err(ControlError::new("Unknown route", 404)),
    }
}

/// The request's JSON object body, {} when empty; at most `limit` bytes.
fn body(request: &mut tiny_http::Request, limit: usize) -> Result<Map<String, Value>, ControlError> {
    let length = match header(request, "Content-Length") {
        None => 0,
        Some(length) => length.trim().parse::<usize>().map_err(|_| ControlError::bad("Invalid Content-Length"))?,
    };
    if length > limit {
        return Err(ControlError::new("Request body is too large", 413));
    }
    if length == 0 {
        return Ok(Map::new());
    }
    let mut data = Vec::new();
    let read = request.as_reader().take(length as u64).read_to_end(&mut data);
    match read.ok().and_then(|_| serde_json::from_slice(&data).ok()) {
        Some(Value::Object(body)) => Ok(body),
        Some(_) => Err(ControlError::bad("Request body must be a JSON object")),
        None => Err(ControlError::bad("Request body must be JSON")),
    }
}

/// Compare without stopping at the first difference, so timing does not reveal the token.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |difference, (x, y)| difference | (x ^ y)) == 0
}

/// 32 random bytes as hex.
fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_control_file(port: u16, token: &str) -> io::Result<()> {
    let path = control_path();
    let dir = path.parent().ok_or_else(|| io::Error::other("Invalid settings folder"))?;
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::Builder::new().prefix("control-").suffix(".tmp").tempfile_in(dir)?;
    serde_json::to_writer(&mut file, &json!({ "port": port, "token": token, "pid": std::process::id() }))?;
    file.persist(&path)?;
    Ok(())
}

/// Decode %XX escapes; invalid UTF-8 becomes U+FFFD.
fn percent_decode(part: &str) -> String {
    let bytes = part.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes.get(i + 1..i + 3).filter(|hex| bytes[i] == b'%' && hex.iter().all(u8::is_ascii_hexdigit));
        match hex.and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()) {
            Some(byte) => {
                decoded.push(byte);
                i += 3;
            }
            None => {
                decoded.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Escape all but unreserved characters, so a name with slashes stays one path part.
fn percent_encode(part: &str) -> String {
    part.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => char::from(byte).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[derive(Debug)]
pub enum RequestError {
    /// The manager is not running, control.json is unreadable, or it did not answer.
    Unreachable(String),
    /// The manager answered with an error.
    Refused(ControlError),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequestError::Unreachable(message) => f.write_str(message),
            RequestError::Refused(error) => f.write_str(&error.message),
        }
    }
}

/// Call the running manager, e.g. request("POST", &["projects", name, "start"], None, timeout).
/// Path parts are percent-encoded.
pub fn request(method: &str, parts: &[&str], body: Option<&Value>, timeout: Duration) -> Result<Value, RequestError> {
    let unreadable = |error: String| RequestError::Unreachable(format!("Unreadable control.json: {error}"));
    let path = control_path();
    let info: Value = match std::fs::read(&path) {
        Ok(data) => serde_json::from_slice(&data).map_err(|error| unreadable(error.to_string()))?,
        Err(error) => return Err(RequestError::Unreachable(format!("{}: {error}", path.display()))),
    };
    let port = info.get("port").and_then(Value::as_u64).and_then(|port| u16::try_from(port).ok());
    let (Some(port), Some(token)) = (port, info.get("token").and_then(Value::as_str)) else {
        return Err(unreadable("no port and token".into()));
    };
    let path = format!("/{}", parts.iter().map(|part| percent_encode(part)).collect::<Vec<_>>().join("/"));
    let authorization = format!("Bearer {token}");
    let response =
        crate::localnet::request(port, method, &path, body, &[("Authorization", &authorization)], timeout, None)
            .map_err(|error| RequestError::Unreachable(error.to_string()))?;
    match serde_json::from_slice::<Value>(&response.body).ok() {
        Some(reply @ Value::Object(_)) if (200..300).contains(&response.status) => Ok(reply),
        reply => {
            let message = match reply.as_ref().and_then(|reply| reply.get("error")) {
                Some(Value::String(message)) if !message.is_empty() => message.clone(),
                _ => format!("HTTP {}", response.status),
            };
            Err(RequestError::Refused(ControlError::new(message, response.status)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Point control.json at a temporary folder, one test at a time.
    fn isolated() -> MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
        // SAFETY: only tests in this module change the environment, and they hold LOCK.
        unsafe { std::env::set_var("ROJO_MANAGER_DATA_DIR", dir.path()) };
        guard
    }

    /// Records every call as JSON.
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<Value>>,
    }

    impl ControlHandler for Fake {
        fn projects(&self) -> Vec<Value> {
            vec![json!({"name": "Game/Lobby"})]
        }

        fn act(&self, project: Option<&str>, action: &str, body: &Map<String, Value>) -> Result<Value, ControlError> {
            self.calls.lock().unwrap().push(json!([project, action, body]));
            if project == Some("Missing") {
                return Err(ControlError::new("No project named Missing", 404));
            }
            Ok(json!({"project": {"name": project}}))
        }

        fn settings(&self, changes: Option<&Map<String, Value>>) -> Result<Value, ControlError> {
            self.calls.lock().unwrap().push(json!(["settings", changes]));
            Ok(json!({"settings": {"tray": true}}))
        }

        fn logs(&self, project: &str) -> Result<Value, ControlError> {
            Ok(json!({"project": project, "lines": ["a"]}))
        }

        fn report_rojo_plugins(&self, count: u32) {
            self.calls.lock().unwrap().push(json!(["rojo-plugins", count]));
        }

        fn poll_bridge(&self, id: u64, poll: BridgePoll) -> Value {
            self.calls.lock().unwrap().push(json!(["bridge", id, poll.place_id, poll.port]));
            match poll.place_id {
                42 => json!({"port": 34880, "accept": true}),
                _ => json!({"port": null, "accept": false}),
            }
        }

        fn report_output(&self, output: StudioOutput) {
            let lines: Vec<(&str, OutputKind)> = output.lines.iter().map(|l| (l.text.as_str(), l.kind)).collect();
            self.calls.lock().unwrap().push(json!(["output", output.place_id, output.context, lines]));
        }

        fn studio(&self, command: Option<(&str, &Map<String, Value>)>) -> Result<Value, ControlError> {
            self.calls.lock().unwrap().push(json!(["studio", command]));
            Ok(json!({}))
        }

        fn updates(&self, install: Option<&str>) -> Result<Value, ControlError> {
            self.calls.lock().unwrap().push(json!(["updates", install]));
            Ok(json!({}))
        }
    }

    fn status(port: u16, method: &str, path: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> u16 {
        crate::localnet::request(port, method, path, body, headers, Duration::from_secs(5), None).unwrap().status
    }

    const TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn requests_need_the_token_and_browsers_are_refused() {
        let _guard = isolated();
        let server = ControlServer::start(Arc::new(Fake::default())).unwrap();
        let port = server.port;
        let bearer = format!("Bearer {}", server.token);
        let token = ("Authorization", bearer.as_str());
        assert_eq!(status(port, "GET", "/projects", None, &[]), 401);
        assert_eq!(status(port, "GET", "/projects", None, &[("Authorization", "Bearer wrong")]), 401);
        assert_eq!(status(port, "GET", "/projects", None, &[token, ("Origin", "https://example.invalid")]), 403);
        assert_eq!(status(port, "GET", "/projects", None, &[token]), 200);
        assert_eq!(status(port, "GET", "/nowhere", None, &[token]), 404);
        assert_eq!(status(port, "POST", "/settings", Some(&json!([1])), &[token]), 400);
        let large = json!({"path": "x".repeat(MAX_BODY)});
        assert_eq!(status(port, "POST", "/projects", Some(&large), &[token]), 413);
        server.close();
    }

    #[test]
    fn the_client_routes_actions_and_names_containing_slashes() {
        let _guard = isolated();
        let fake = Arc::new(Fake::default());
        let server = ControlServer::start(fake.clone()).unwrap();
        assert_eq!(
            request("GET", &["projects"], None, TIMEOUT).unwrap(),
            json!({"projects": [{"name": "Game/Lobby"}]})
        );
        let restarted = request("POST", &["projects", "Game/Lobby", "restart"], None, TIMEOUT).unwrap();
        assert_eq!(restarted, json!({"project": {"name": "Game/Lobby"}}));
        request("POST", &["projects", "Game/Lobby", "link-place"], Some(&json!({"place": "42"})), TIMEOUT).unwrap();
        request("POST", &["projects"], Some(&json!({"path": "C:/Games"})), TIMEOUT).unwrap();
        assert_eq!(request("GET", &["settings"], None, TIMEOUT).unwrap(), json!({"settings": {"tray": true}}));
        request("POST", &["settings"], Some(&json!({"tray": false})), TIMEOUT).unwrap();
        match request("POST", &["projects", "Missing", "start"], None, TIMEOUT) {
            Err(RequestError::Refused(error)) => assert_eq!(error, ControlError::new("No project named Missing", 404)),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            request("POST", &["projects", "Game/Lobby", "delete"], None, TIMEOUT),
            Err(RequestError::Refused(_))
        ));
        assert_eq!(
            *fake.calls.lock().unwrap(),
            [
                json!(["Game/Lobby", "restart", {}]),
                json!(["Game/Lobby", "link-place", {"place": "42"}]),
                json!([null, "add", {"path": "C:/Games"}]),
                json!(["settings", null]),
                json!(["settings", {"tray": false}]),
                json!(["Missing", "start", {}]),
            ]
        );
        server.close();
    }

    #[test]
    fn close_removes_only_its_own_control_file() {
        let _guard = isolated();
        ControlServer::start(Arc::new(Fake::default())).unwrap().close();
        assert!(!control_path().exists());
        assert!(matches!(request("GET", &["projects"], None, TIMEOUT), Err(RequestError::Unreachable(_))));
        let stale = ControlServer::start(Arc::new(Fake::default())).unwrap();
        let newer = ControlServer::start(Arc::new(Fake::default())).unwrap();
        stale.close(); // must not delete the newer instance's file
        let stored: Value = serde_json::from_slice(&std::fs::read(control_path()).unwrap()).unwrap();
        assert_eq!(stored["token"], json!(newer.token));
        assert_eq!(stored["port"], json!(newer.port));
        newer.close();
    }

    #[test]
    fn bridge_server_answers_polls_without_a_token_but_not_browsers_and_owns_its_port() {
        let fake = Arc::new(Fake::default());
        let server = AutoConnectServer::start(fake.clone(), 0).unwrap();
        let port = server.served.server.server_addr().to_ip().unwrap().port();
        let send = |method: &str, path: &str, body: Option<&Value>, headers: &[(&str, &str)]| {
            let response = crate::localnet::request(port, method, path, body, headers, TIMEOUT, None).unwrap();
            (response.status, serde_json::from_slice::<Value>(&response.body).unwrap())
        };
        assert_eq!(send("GET", "/rojo-plugins/2", None, &[]).0, 200);
        assert_eq!(send("GET", &format!("http://localhost:{port}/rojo-plugins/2"), None, &[]).0, 200);
        assert_eq!(send("GET", "/rojo-plugins/x", None, &[]).0, 404);
        let poll = json!({"placeId": 42, "port": 0, "rojo": {"status": "NotConnected"}});
        assert_eq!(send("POST", "/bridge/9", Some(&poll), &[]), (200, json!({"port": 34880, "accept": true})));
        assert_eq!(send("POST", "/bridge/9", Some(&json!({"port": 0})), &[]).0, 400);
        assert_eq!(send("POST", "/bridge/9", Some(&poll), &[("Origin", "https://example.invalid")]).0, 404);
        assert_eq!(send("GET", "/bridge/9/42/0", None, &[]).0, 404);
        let output = json!({"placeId": 42, "context": "server", "lines": [{"text": "boom", "kind": "error"}]});
        assert_eq!(send("POST", "/output", Some(&output), &[]).0, 200);
        assert_eq!(
            send("POST", "/output", Some(&json!({"placeId": 42, "context": "client", "lines": []})), &[]).0,
            400
        );
        assert_eq!(fake.calls.lock().unwrap().last(), Some(&json!(["output", 42, "server", [["boom", "error"]]])));
        let error = AutoConnectServer::start(Arc::new(Fake::default()), port).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        server.close();
    }

    #[test]
    fn path_parts_round_trip_through_percent_encoding() {
        assert_eq!(percent_encode("Game/Lobby ü"), "Game%2FLobby%20%C3%BC");
        assert_eq!(percent_decode("Game%2FLobby%20%C3%BC"), "Game/Lobby ü");
        assert_eq!(percent_decode("100%+%zz%4"), "100%+%zz%4");
    }
}
