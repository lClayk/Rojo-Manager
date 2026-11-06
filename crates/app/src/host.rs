//! The tray app: owns the engine, the tray icon, and the control servers. It runs a plain Win32
//! message loop (the tray icon needs one) and never starts GPUI. The window is a separate process
//! (`--window`) started on demand, so closing it gives all of its memory back.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use rojo_manager_core::control::{AUTO_CONNECT_PORT, AutoConnectServer, ControlHandler, ControlServer};
use rojo_manager_core::manager::{Control, Engine, Event, ServerState};
use rojo_manager_core::updates;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    ASFW_ANY, AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    MSG, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WNDCLASSW,
};
use windows::core::w;

use crate::instance;
use crate::ipc::{self, Call, Request, ToWindow};
use crate::tray::{self, TrayMenu, TrayState};

/// What the tray, second launches, the engine, and the window process ask of the tray app.
pub enum Msg {
    Show,
    StartAll,
    StopAll,
    Quit,
    /// The window process with this id exited.
    WindowClosed(u64),
    /// The engine's state changed; the tray may need redrawing.
    Changed,
    /// A problem to notify about while the window is closed.
    Alert(String),
    Toggle(String),
    OpenStudio(String),
    InstallUpdate,
    /// An update was installed: start the new version and exit.
    Restart,
}

/// How the app was started. `--after <pid>` waits for that process (the version an update
/// replaced) to exit; each `--start <name>` starts that project's server, as it ran before.
pub struct Launch {
    pub hidden: bool,
    after: Option<u32>,
    start: Vec<String>,
}

impl Launch {
    pub fn parse(mut args: impl Iterator<Item = String>) -> Launch {
        let mut launch = Launch { hidden: false, after: None, start: Vec::new() };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--hidden" => launch.hidden = true,
                "--after" => launch.after = args.next().and_then(|pid| pid.parse().ok()),
                "--start" => launch.start.extend(args.next()),
                _ => {}
            }
        }
        launch
    }
}

/// Messages queue here and wake the message loop through a message-only window.
static INBOX: OnceLock<(mpsc::Sender<Msg>, usize)> = OnceLock::new();
/// A Changed message is queued; engine changes come in bursts, and one redraw covers them.
static CHANGED: AtomicBool = AtomicBool::new(false);

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

/// Ask the tray app's thread to do something; callable from any thread.
pub fn post(msg: Msg) {
    if let Some((tx, hwnd)) = INBOX.get() {
        let _ = tx.send(msg);
        // SAFETY: the message window lives for the whole process.
        unsafe {
            let _ = PostMessageW(Some(HWND(*hwnd as *mut _)), WM_APP, WPARAM(0), LPARAM(0));
        }
    }
}

/// Lines for the open window process, if any.
type Sink = Arc<Mutex<Option<mpsc::Sender<String>>>>;

struct Host {
    engine: Arc<Engine>,
    inbox: mpsc::Receiver<Msg>,
    tray: Option<tray_icon::TrayIcon>,
    /// What the tray shows now, so it is only rebuilt on a change.
    shown: Option<TrayState>,
    /// The items of the menu shown now, updated in place.
    menu: Option<TrayMenu>,
    guid: u128,
    window: Option<(u64, Child)>,
    opened: u64,
    sink: Sink,
    control: Option<ControlServer>,
    auto_connect: Option<AutoConnectServer>,
}

impl Host {
    /// Handle the queued messages; false once the app should quit.
    fn drain(&mut self) -> bool {
        while let Ok(msg) = self.inbox.try_recv() {
            match msg {
                Msg::Show => self.show(),
                Msg::StartAll => self.in_background(|engine| {
                    engine.start_all();
                    Ok(())
                }),
                Msg::StopAll => self.in_background(|engine| {
                    engine.stop_all();
                    Ok(())
                }),
                Msg::Quit => return false,
                Msg::WindowClosed(id) => {
                    // Sent when the window starts closing and again when its process ends.
                    if self.window.as_ref().is_some_and(|(open, _)| *open == id) {
                        self.window = None;
                        *self.sink.lock().unwrap() = None;
                        // With the tray off, closing the window quits.
                        if !self.engine.settings().tray {
                            return false;
                        }
                    }
                    self.sync_tray();
                }
                Msg::Changed => {
                    CHANGED.store(false, Ordering::SeqCst);
                    self.sync_tray(); // the tray setting may have changed through the control API
                }
                Msg::Alert(message) => self.alert(&message),
                Msg::Toggle(name) => {
                    // muda flips a check item on click; show the engine's state until it changes.
                    self.shown = None;
                    self.refresh_tray();
                    let blocked =
                        self.engine.tray_projects().iter().any(|p| p.name == name && p.state == ServerState::Blocked);
                    if blocked {
                        self.show(); // the window explains the port conflict and can free it
                    } else {
                        self.in_background(move |engine| engine.toggle(&name));
                    }
                }
                Msg::OpenStudio(name) => self.in_background(move |engine| engine.open_studio(&name).map(|_| ())),
                Msg::InstallUpdate => self.in_background(|engine| {
                    engine.install_app_update()?;
                    post(Msg::Restart);
                    Ok(())
                }),
                Msg::Restart => match self.restart() {
                    Ok(()) => return false,
                    Err(error) => self.alert(&format!("Could not restart Rojo Manager: {error}")),
                },
            }
        }
        true
    }

    /// Run engine work off this thread; a failure becomes an alert.
    fn in_background(&self, work: impl FnOnce(&Arc<Engine>) -> Result<(), String> + Send + 'static) {
        let engine = Arc::clone(&self.engine);
        std::thread::spawn(move || {
            if let Err(error) = work(&engine) {
                post(Msg::Alert(error));
            }
        });
    }

    /// A Windows notification while the window is closed and notifications are on. With the
    /// window open, the engine has already shown the message there.
    fn alert(&self, message: &str) {
        if self.window.is_none()
            && self.engine.settings().notifications
            && let Some(tray) = &self.tray
        {
            tray::notify(tray, self.guid, "Rojo Manager", message);
        }
    }

    /// Focus the window, starting its process when it is not open.
    fn show(&mut self) {
        // SAFETY: plain call; lets the window process take the foreground.
        unsafe {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
        }
        if self.window.is_some() {
            if let Some(tx) = &*self.sink.lock().unwrap() {
                let _ = tx.send(ipc::line(&ToWindow::Activate));
            }
            return;
        }
        match self.open_window() {
            Ok(window) => self.window = Some(window),
            Err(error) => eprintln!("Could not open the window: {error}"),
        }
        self.sync_tray();
    }

    /// Start `RojoManager.exe --window`: one thread writes its input, one reads its calls and runs
    /// each on its own thread (some, like killing a port owner, take a while).
    fn open_window(&mut self) -> std::io::Result<(u64, Child)> {
        let mut child = Command::new(std::env::current_exe()?)
            .arg("--window")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let (Some(mut input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
            unreachable!("both are piped")
        };
        self.opened += 1;
        let window = self.opened;
        let (tx, lines) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in lines {
                if writeln!(input, "{line}").and_then(|()| input.flush()).is_err() {
                    break;
                }
            }
        });
        let engine = Arc::clone(&self.engine);
        let replies = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                let Ok(Request { id, call }) = serde_json::from_str(&line) else {
                    eprintln!("Unreadable message from the window: {line}");
                    continue;
                };
                if matches!(call, Call::Closing) {
                    post(Msg::WindowClosed(window));
                    continue;
                }
                let (engine, replies) = (Arc::clone(&engine), replies.clone());
                std::thread::spawn(move || {
                    let update = matches!(&call, Call::InstallAppUpdate);
                    let result = ipc::serve(&engine, call);
                    let restart = update && result.is_ok();
                    let _ = replies.send(ipc::line(&ToWindow::Reply { id, result }));
                    if restart {
                        post(Msg::Restart);
                    }
                });
            }
            post(Msg::WindowClosed(window));
        });
        *self.sink.lock().unwrap() = Some(tx);
        Ok((window, child))
    }

    /// Show the tray icon while "Keep running in the tray" is on (or no window is open), else remove it.
    fn sync_tray(&mut self) {
        let wanted = self.engine.settings().tray || self.window.is_none();
        if wanted && self.tray.is_none() {
            match tray::build(self.guid) {
                Ok(icon) => {
                    self.tray = Some(icon);
                    self.shown = None;
                    self.menu = None;
                }
                Err(error) => eprintln!("Could not show the tray icon: {error}"),
            }
        } else if !wanted {
            self.tray = None;
        }
        self.refresh_tray();
    }

    /// Bring the tray's menu, status dot, and tooltip up to date with the engine.
    fn refresh_tray(&mut self) {
        let Some(icon) = &self.tray else { return };
        let state = TrayState {
            projects: self.engine.tray_projects(),
            update: self.engine.available_update(),
            status: self.engine.settings().tray_status,
        };
        if self.shown.as_ref() == Some(&state) {
            return;
        }
        match tray::apply(icon, self.guid, &state, &mut self.menu) {
            Ok(()) => self.shown = Some(state),
            Err(error) => eprintln!("Could not update the tray icon: {error}"),
        }
    }

    /// Start the newly installed version, which waits for this one to exit, then resumes the
    /// servers running now and the window if it is open.
    fn restart(&self) -> std::io::Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("--after").arg(std::process::id().to_string());
        if self.window.is_none() {
            command.arg("--hidden");
        }
        let running = self.engine.tray_projects().into_iter().filter(|project| {
            matches!(project.state, ServerState::Starting | ServerState::Serving | ServerState::Connected)
        });
        for project in running {
            command.arg("--start").arg(project.name);
        }
        command.spawn().map(|_| ())
    }

    fn shutdown(mut self) {
        self.tray = None;
        *self.sink.lock().unwrap() = None;
        if let Some((_, mut child)) = self.window.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(server) = self.control.take() {
            server.close();
        }
        if let Some(server) = self.auto_connect.take() {
            server.close();
        }
        self.engine.shutdown();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg != WM_APP {
        // SAFETY: forwards this window's own message.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let quit = HOST.with(|host| host.borrow_mut().as_mut().is_some_and(|host| !host.drain()));
    if quit {
        if let Some(host) = HOST.with(|host| host.borrow_mut().take()) {
            host.shutdown();
        }
        // SAFETY: ends this thread's message loop.
        unsafe { PostQuitMessage(0) };
    }
    LRESULT(0)
}

/// A malformed config.json is reported and kept; the app does not start over it.
fn report_bad_settings(error: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    let path = rojo_manager_core::config::config_path();
    let text = format!(
        "Could not load settings at:\n{}\n\n{error}\n\nYour settings have not been changed. \
         Move the file aside to start fresh.",
        path.display()
    );
    // SAFETY: both strings outlive the call, and no owner window is needed.
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from("Rojo Manager could not open"), MB_OK | MB_ICONERROR);
    }
}

/// Wait up to `timeout` for a process to exit; returns at once if it already has.
fn wait_for_exit(pid: u32, timeout: Duration) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: the handle is checked, waited on, and closed here.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(process, timeout.as_millis() as u32);
            let _ = CloseHandle(process);
        }
    }
}

pub fn run(launch: Launch) {
    // After an update, the replaced version is still shutting down and holds the instance lock.
    if let Some(pid) = launch.after {
        wait_for_exit(pid, Duration::from_secs(20));
    }
    let config = match rojo_manager_core::config::load() {
        Ok(config) => config,
        Err(error) => {
            report_bad_settings(&error);
            std::process::exit(1);
        }
    };
    // SAFETY: registers and creates a message-only window owned by this thread.
    let hwnd = unsafe {
        let class = w!("RojoManagerHost");
        RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), lpszClassName: class, ..Default::default() });
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
    };
    let hwnd = hwnd.expect("the message window can be created");
    let (tx, inbox) = mpsc::channel();
    let _ = INBOX.set((tx, hwnd.0 as usize));

    // A second launch asks this one to show its window (unless it is a hidden autostart) and exits.
    if !instance::claim(!launch.hidden) {
        return;
    }
    let app_dir =
        std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.to_path_buf())).unwrap_or_default();
    updates::remove_replaced(&app_dir);
    let sink: Sink = Arc::default();
    let events = Arc::clone(&sink);
    let engine = Engine::new(config, app_dir, move |event| {
        match &event {
            Event::Changed if !CHANGED.swap(true, Ordering::SeqCst) => post(Msg::Changed),
            Event::Alert(message) => post(Msg::Alert(message.clone())),
            _ => {}
        }
        if let Some(tx) = &*events.lock().unwrap() {
            let _ = tx.send(ipc::line(&ToWindow::Event(event)));
        }
    });
    let handler: Arc<dyn ControlHandler> = Arc::new(Control(Arc::clone(&engine)));
    let control = ControlServer::start(Arc::clone(&handler))
        .inspect_err(|error| engine.notice(format!("Agent control is unavailable: {error}")))
        .ok();
    // Another Rojo Manager, or another program, may hold the port; auto-connect is then off here.
    let auto_connect = AutoConnectServer::start(handler, AUTO_CONNECT_PORT).ok();
    engine.run();
    for name in &launch.start {
        let _ = engine.start(name);
    }
    tray::forward_events();

    let mut host = Host {
        engine,
        inbox,
        tray: None,
        shown: None,
        menu: None,
        guid: tray::guid(),
        window: None,
        opened: 0,
        sink,
        control,
        auto_connect,
    };
    if launch.hidden {
        host.sync_tray();
    } else {
        host.show();
    }
    HOST.with(|slot| *slot.borrow_mut() = Some(host));

    let mut msg = MSG::default();
    // SAFETY: the standard message loop on the thread that owns the tray and message windows.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
