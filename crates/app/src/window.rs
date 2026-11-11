//! The window process (`RojoManager --window`): the GPUI window, drawing what the tray app's
//! engine reports. It exits when the window closes, or when the tray app goes away.

use std::borrow::Cow;
use std::sync::Arc;

use gpui_kit::component::TitleBar;
use gpui_kit::*;

use crate::ipc::{Client, ToWindow};
use crate::theme;
use crate::view::MainView;

// Cmd+W and Cmd+Q; the tray-only app has no menu bar to hold them.
#[cfg(target_os = "macos")]
actions!(rojo_manager, [CloseWindow, QuitApp]);

/// The app's icons first (assets/icons), then the component library's own.
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let icon: Option<&'static [u8]> = match path {
            "icons/plus.svg" => Some(include_bytes!("../assets/icons/plus.svg")),
            "icons/play.svg" => Some(include_bytes!("../assets/icons/play.svg")),
            "icons/square.svg" => Some(include_bytes!("../assets/icons/square.svg")),
            "icons/settings.svg" => Some(include_bytes!("../assets/icons/settings.svg")),
            "icons/x.svg" => Some(include_bytes!("../assets/icons/x.svg")),
            "icons/circle-arrow-up.svg" => Some(include_bytes!("../assets/icons/circle-arrow-up.svg")),
            "icons/triangle-alert.svg" => Some(include_bytes!("../assets/icons/triangle-alert.svg")),
            "icons/chevron-down.svg" => Some(include_bytes!("../assets/icons/chevron-down.svg")),
            "icons/chevron-right.svg" => Some(include_bytes!("../assets/icons/chevron-right.svg")),
            _ => None,
        };
        match icon {
            Some(bytes) => Ok(Some(Cow::Borrowed(bytes))),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

pub fn run() {
    let (tx, rx) = async_channel::unbounded::<ToWindow>();
    let client = Client::start(move |message| {
        let _ = tx.send_blocking(message);
    });

    let closing = Arc::clone(&client);
    #[cfg(target_os = "macos")]
    let keys = Arc::clone(&client);
    gpui_kit::application().with_assets(Assets).run(move |cx| {
        gpui_kit::init(cx);
        theme::apply(cx);
        #[cfg(target_os = "macos")]
        {
            cx.bind_keys([KeyBinding::new("cmd-w", CloseWindow, None), KeyBinding::new("cmd-q", QuitApp, None)]);
            let close = Arc::clone(&keys);
            cx.on_action(move |_: &CloseWindow, cx| {
                close.closing();
                cx.quit();
            });
            cx.on_action(move |_: &QuitApp, _| keys.quit());
        }
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("Rojo Manager".into()),
                // Centered in the app's taller title bar.
                traffic_light_position: Some(point(px(16.), px(15.))),
                ..TitleBar::title_bar_options()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(760.), px(820.)), cx))),
            window_min_size: Some(size(px(420.), px(360.))),
            app_id: Some("com.lclayk.rojomanager".into()),
            ..TitleBar::window_options()
        };
        let (handle, view) =
            match gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| MainView::new(client, window, cx))) {
                Ok(opened) => opened,
                Err(error) => {
                    eprintln!("Could not open the window: {error}");
                    cx.quit();
                    return;
                }
            };
        let _ = handle.update(cx, |_, window, cx| {
            #[cfg(windows)]
            throttle_background_paints(window);
            // macOS leaves a new window behind the app in front, where its traffic lights do not
            // show. It comes forward once its first frame is drawn, so it never shows up empty.
            #[cfg(target_os = "macos")]
            window.on_next_frame(|window, _| window.on_next_frame(|_, cx| cx.activate(true)));
            window.on_window_should_close(cx, move |_, _| {
                closing.closing();
                true
            });
        });
        let view = view.downgrade();
        cx.spawn(async move |cx| {
            while let Ok(message) = rx.recv().await {
                cx.update(|cx| match message {
                    ToWindow::Event(event) => {
                        view.update(cx, |view, cx| view.on_event(event, cx)).ok();
                    }
                    ToWindow::ConfirmUpdate => {
                        view.update(cx, |view, cx| view.confirm_update(cx)).ok();
                    }
                    ToWindow::Activate => {
                        #[cfg(target_os = "macos")]
                        cx.activate(true);
                        let _ = handle.update(cx, |_, window, _| window.activate_window());
                    }
                    ToWindow::Reply { .. } => {}
                });
            }
        })
        .detach();
        cx.on_window_closed(|cx, _| cx.quit()).detach();
    });
}

/// GPUI asks the window to repaint on every display refresh, and a paint with nothing new is cheap
/// but not free. While another window is in front, at most 10 a second are handled; the rest are
/// dropped, so changes still show within 0.1 s.
#[cfg(windows)]
fn throttle_background_paints(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::SetWindowSubclass;
    let Ok(RawWindowHandle::Win32(handle)) = HasWindowHandle::window_handle(window).map(|handle| handle.as_raw())
    else {
        return;
    };
    // SAFETY: the window belongs to this thread and outlives the subclass, which Windows removes with it.
    unsafe {
        let _ = SetWindowSubclass(HWND(handle.hwnd.get() as *mut _), Some(throttle), 1, 0);
    }
}

#[cfg(windows)]
unsafe extern "system" fn throttle(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _: usize,
    _: usize,
) -> windows::Win32::Foundation::LRESULT {
    use std::cell::Cell;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::Graphics::Gdi::ValidateRect;
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, WM_PAINT};
    thread_local! {
        static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
    }
    // SAFETY: plain calls on this thread's own window, from its window procedure.
    unsafe {
        if message == WM_PAINT && GetForegroundWindow() != hwnd {
            let recent = LAST.get().is_some_and(|last| last.elapsed() < Duration::from_millis(100));
            if recent {
                let _ = ValidateRect(Some(hwnd), None);
                return LRESULT(0);
            }
            LAST.set(Some(Instant::now()));
        }
        DefSubclassProc(hwnd, message, wparam, lparam)
    }
}
