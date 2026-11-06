//! The tray icon: a menu to run each project's server without opening the window, a status dot,
//! and Windows notifications. A left click shows the window.

use std::sync::LazyLock;

use image::RgbaImage;
use rojo_manager_core::manager::{ServerState, TrayProject};
use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::host::{self, Msg};
use crate::theme::{GREEN, RED, YELLOW};

/// What the tray shows; it is rebuilt only when this changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayState {
    pub projects: Vec<TrayProject>,
    /// A newer release's version, offered in the menu.
    pub update: Option<String>,
    /// Color the icon by server state ("Show server status on the tray icon").
    pub status: bool,
}

/// The icon's identity for Windows, derived from the executable's path: Windows ties a tray GUID
/// to one path, so each copy of the app gets its own.
pub fn guid() -> u128 {
    let path = std::env::current_exe().unwrap_or_default().to_string_lossy().to_lowercase();
    let fnv =
        |seed: u64| path.bytes().fold(seed, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3));
    (u128::from(fnv(0xcbf2_9ce4_8422_2325)) << 64) | u128::from(fnv(0x8422_2325_cbf2_9ce4))
}

pub fn build(guid: u128) -> Result<TrayIcon, Box<dyn std::error::Error>> {
    // An icon left behind by a crashed run would make adding this GUID again fail.
    shell_notify(guid, None, windows::Win32::UI::Shell::NIM_DELETE, |_| {});
    Ok(TrayIconBuilder::new()
        .with_id("tray")
        .with_guid(guid)
        .with_tooltip("Rojo Manager")
        .with_menu_on_left_click(false)
        .with_icon(icon(None)?)
        .build()?)
}

/// Show `state` on the tray icon: its menu, the status dot, and the tooltip. `menu` holds the
/// menu shown now; it is updated in place while only server states change.
pub fn apply(
    tray: &TrayIcon,
    guid: u128,
    state: &TrayState,
    menu: &mut Option<TrayMenu>,
) -> Result<(), Box<dyn std::error::Error>> {
    match menu {
        Some(shown) if shown.layout == Layout::of(state) => shown.update(state),
        _ => {
            let (built, items) = TrayMenu::build(state)?;
            tray.set_menu(Some(Box::new(built)));
            *menu = Some(items);
        }
    }
    tray.set_icon(Some(icon(state.status.then(|| dot(&state.projects)).flatten())?))?;
    // tray-icon's set_tooltip leaves out the GUID, which Windows needs to find this icon.
    let text = tooltip(&state.projects);
    shell_notify(guid, Some(tray), windows::Win32::UI::Shell::NIM_MODIFY, |data| {
        data.uFlags |= windows::Win32::UI::Shell::NIF_TIP | windows::Win32::UI::Shell::NIF_SHOWTIP;
        copy_into(&mut data.szTip, &text);
    });
    Ok(())
}

/// What a project's server is doing, as the menu says it.
fn describe(state: ServerState) -> Option<&'static str> {
    match state {
        ServerState::Off => None,
        ServerState::Starting => Some("starting"),
        ServerState::Serving => Some("running"),
        ServerState::Connected => Some("connected"),
        ServerState::External => Some("running elsewhere"),
        ServerState::Blocked => Some("port in use"),
    }
}

/// A menu shows "&" as a shortcut marker unless doubled.
fn label(name: &str) -> String {
    name.replace('&', "&&")
}

fn project_text(project: &TrayProject) -> String {
    match describe(project.state) {
        Some(state) => format!("{}  ({state})", label(&project.name)),
        None => label(&project.name),
    }
}

fn project_on(project: &TrayProject) -> bool {
    !matches!(project.state, ServerState::Off | ServerState::Blocked)
}

fn any_running(state: &TrayState) -> bool {
    state.projects.iter().any(|project| project.state != ServerState::Off)
}

/// The menu's shape: which items it has. Only a change here rebuilds it.
#[derive(PartialEq, Eq)]
struct Layout {
    projects: Vec<(String, bool)>,
    update: Option<String>,
}

impl Layout {
    fn of(state: &TrayState) -> Layout {
        let projects = state.projects.iter().map(|project| (project.name.clone(), project.linked)).collect();
        Layout { projects, update: state.update.clone() }
    }
}

/// The menu items that follow server states. A pick names its item by id, and a rebuilt menu
/// gets new ids, so a menu open while a server starts keeps working only if its items stay.
pub struct TrayMenu {
    layout: Layout,
    projects: Vec<CheckMenuItem>,
    stop_all: MenuItem,
}

impl TrayMenu {
    fn build(state: &TrayState) -> tray_icon::menu::Result<(Menu, TrayMenu)> {
        let projects: Vec<CheckMenuItem> = state
            .projects
            .iter()
            .map(|project| {
                let id = format!("toggle:{}", project.name);
                CheckMenuItem::with_id(id, project_text(project), true, project_on(project), None)
            })
            .collect();
        let places: Vec<MenuItem> = state
            .projects
            .iter()
            .filter(|project| project.linked)
            .map(|project| MenuItem::with_id(format!("studio:{}", project.name), label(&project.name), true, None))
            .collect();
        let stop_all = MenuItem::with_id("stop_all", "Stop All Servers", any_running(state), None);

        let menu = Menu::new();
        menu.append(&MenuItem::with_id("show", "Show Rojo Manager", true, None))?;
        if !projects.is_empty() {
            menu.append(&PredefinedMenuItem::separator())?;
            for project in &projects {
                menu.append(project)?;
            }
        }
        if !places.is_empty() {
            let items: Vec<&dyn IsMenuItem> = places.iter().map(|item| item as &dyn IsMenuItem).collect();
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&Submenu::with_items("Open in Studio", true, &items)?)?;
        }
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&MenuItem::with_id("start_all", "Start All Servers", !projects.is_empty(), None))?;
        menu.append(&stop_all)?;
        if let Some(version) = &state.update {
            menu.append(&PredefinedMenuItem::separator())?;
            menu.append(&MenuItem::with_id("update", format!("Update to {version}"), true, None))?;
        }
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&MenuItem::with_id("quit", "Quit", true, None))?;
        Ok((menu, TrayMenu { layout: Layout::of(state), projects, stop_all }))
    }

    fn update(&self, state: &TrayState) {
        for (item, project) in self.projects.iter().zip(&state.projects) {
            item.set_text(project_text(project));
            item.set_checked(project_on(project));
        }
        self.stop_all.set_enabled(any_running(state));
    }
}

/// "Rojo Manager" plus a count of running and connected servers.
fn tooltip(projects: &[TrayProject]) -> String {
    let count = |states: &[ServerState]| projects.iter().filter(|p| states.contains(&p.state)).count();
    let counts = [
        (count(&[ServerState::Serving, ServerState::Connected, ServerState::External]), "running"),
        (count(&[ServerState::Connected]), "connected"),
        (count(&[ServerState::Blocked]), "port in use"),
    ];
    let parts: Vec<String> =
        counts.iter().filter(|(count, _)| *count > 0).map(|(count, what)| format!("{count} {what}")).collect();
    if parts.is_empty() { "Rojo Manager".into() } else { format!("Rojo Manager: {}", parts.join(", ")) }
}

/// The dot's color: a taken port first, then a connected Studio, then any running server.
fn dot(projects: &[TrayProject]) -> Option<u32> {
    let any = |state: ServerState| projects.iter().any(|p| p.state == state);
    if any(ServerState::Blocked) {
        Some(RED)
    } else if any(ServerState::Connected) {
        Some(GREEN)
    } else if projects.iter().any(|p| p.state.running() || p.state == ServerState::Starting) {
        Some(YELLOW)
    } else {
        None
    }
}

const SIZE: u32 = 32;

static BASE: LazyLock<RgbaImage> = LazyLock::new(|| {
    let png = image::load_from_memory(include_bytes!("../../../assets/icon.png")).expect("the app icon is a PNG");
    png.resize(SIZE, SIZE, image::imageops::FilterType::Lanczos3).into_rgba8()
});

fn icon(color: Option<u32>) -> Result<tray_icon::Icon, tray_icon::BadIcon> {
    tray_icon::Icon::from_rgba(badged(color).into_raw(), SIZE, SIZE)
}

/// The app icon, with a status dot in its lower right corner when `color` is set.
fn badged(color: Option<u32>) -> RgbaImage {
    let mut image = BASE.clone();
    if let Some(color) = color {
        let [_, r, g, b] = color.to_be_bytes();
        let (center, radius, ring) = (24.5_f32, 7.0_f32, 2.0_f32);
        let blend = |base: u8, top: u8, amount: f32| (f32::from(base) * (1. - amount) + f32::from(top) * amount) as u8;
        for (x, y, pixel) in image.enumerate_pixels_mut() {
            let distance = ((x as f32 + 0.5 - center).powi(2) + (y as f32 + 0.5 - center).powi(2)).sqrt();
            // A black ring separates the dot from the icon; both edges are antialiased.
            let fill = (radius - distance + 0.5).clamp(0., 1.);
            let outer = (radius + ring - distance + 0.5).clamp(0., 1.);
            let [pr, pg, pb, pa] = pixel.0;
            let (pr, pg, pb) = (blend(pr, 0, outer), blend(pg, 0, outer), blend(pb, 0, outer));
            pixel.0 = [blend(pr, r, fill), blend(pg, g, fill), blend(pb, b, fill), pa.max((outer * 255.) as u8)];
        }
    }
    image
}

/// Show a Windows notification from the tray icon. Quiet hours and Focus are respected.
pub fn notify(tray: &TrayIcon, guid: u128, title: &str, text: &str) {
    use windows::Win32::UI::Shell::{NIF_INFO, NIIF_RESPECT_QUIET_TIME, NIIF_WARNING, NIM_MODIFY};
    shell_notify(guid, Some(tray), NIM_MODIFY, |data| {
        data.uFlags |= NIF_INFO;
        data.dwInfoFlags = NIIF_WARNING | NIIF_RESPECT_QUIET_TIME;
        copy_into(&mut data.szInfoTitle, title);
        copy_into(&mut data.szInfo, text);
    });
}

/// Shell_NotifyIconW on the icon with this GUID.
fn shell_notify(
    guid: u128,
    tray: Option<&TrayIcon>,
    message: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE,
    fill: impl FnOnce(&mut windows::Win32::UI::Shell::NOTIFYICONDATAW),
) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{NIF_GUID, NOTIFYICONDATAW, Shell_NotifyIconW};
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: HWND(tray.map_or(std::ptr::null_mut(), |tray| tray.window_handle() as *mut _)),
        uFlags: NIF_GUID,
        guidItem: windows::core::GUID::from_u128(guid),
        ..Default::default()
    };
    fill(&mut data);
    // SAFETY: `data` is fully initialized and outlives the call.
    unsafe {
        let _ = Shell_NotifyIconW(message, &data);
    }
}

/// Copy `text` into a fixed UTF-16 buffer, cut to fit and NUL-terminated.
fn copy_into(buffer: &mut [u16], text: &str) {
    let units: Vec<u16> = text.encode_utf16().take(buffer.len() - 1).collect();
    buffer[..units.len()].copy_from_slice(&units);
    buffer[units.len()] = 0;
}

/// Route tray clicks and menu picks to the tray app. Set once; it covers every tray icon built later.
pub fn forward_events() {
    TrayIconEvent::set_event_handler(Some(|event| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            host::post(Msg::Show);
        }
    }));
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        let id = event.id.as_ref();
        let msg = if let Some(name) = id.strip_prefix("toggle:") {
            Msg::Toggle(name.to_string())
        } else if let Some(name) = id.strip_prefix("studio:") {
            Msg::OpenStudio(name.to_string())
        } else {
            match id {
                "show" => Msg::Show,
                "start_all" => Msg::StartAll,
                "stop_all" => Msg::StopAll,
                "update" => Msg::InstallUpdate,
                "quit" => Msg::Quit,
                _ => return,
            }
        };
        host::post(msg);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, state: ServerState) -> TrayProject {
        TrayProject { name: name.into(), state, linked: false }
    }

    #[test]
    fn the_dot_and_tooltip_follow_the_most_important_state() {
        let projects = [project("A", ServerState::Serving), project("B", ServerState::Connected)];
        assert_eq!(dot(&projects), Some(GREEN));
        assert_eq!(tooltip(&projects), "Rojo Manager: 2 running, 1 connected");
        let blocked = [project("A", ServerState::Serving), project("B", ServerState::Blocked)];
        assert_eq!(dot(&blocked), Some(RED));
        assert_eq!(dot(&[project("A", ServerState::Off)]), None);
        assert_eq!(tooltip(&[project("A", ServerState::Off)]), "Rojo Manager");
    }

    #[test]
    fn the_menu_lists_projects_with_their_state_and_linked_places() {
        use tray_icon::menu::MenuItemKind;
        let lobby = TrayProject { linked: true, ..project("Lobby & Co", ServerState::Serving) };
        let projects = vec![lobby, project("Obby", ServerState::Off)];
        let state = TrayState { projects, update: Some("0.7.1".into()), status: true };
        let text = |item: &MenuItemKind| match item {
            MenuItemKind::MenuItem(item) => item.text(),
            MenuItemKind::Check(item) => format!("{} [{}]", item.text(), if item.is_checked() { "x" } else { " " }),
            MenuItemKind::Submenu(menu) => {
                let items: Vec<String> =
                    menu.items().iter().filter_map(|i| i.as_menuitem().map(|i| i.text())).collect();
                format!("{} > {}", menu.text(), items.join(", "))
            }
            _ => "-".into(),
        };
        let (menu, items) = TrayMenu::build(&state).unwrap();
        let texts: Vec<String> = menu.items().iter().map(text).collect();
        assert_eq!(
            texts,
            [
                "Show Rojo Manager",
                "-",
                "Lobby && Co  (running) [x]",
                "Obby [ ]",
                "-",
                "Open in Studio > Lobby && Co",
                "-",
                "Start All Servers",
                "Stop All Servers",
                "-",
                "Update to 0.7.1",
                "-",
                "Quit",
            ]
        );

        // A server stopping updates the shown items instead of replacing them.
        let mut stopped = state.clone();
        stopped.projects[0].state = ServerState::Off;
        assert!(items.layout == Layout::of(&stopped));
        items.update(&stopped);
        assert_eq!(text(&menu.items()[2]), "Lobby && Co [ ]");
        assert!(!items.stop_all.is_enabled());
    }

    #[test]
    fn the_status_dot_sits_in_the_lower_right_corner() {
        let [_, r, g, b] = GREEN.to_be_bytes();
        let image = badged(Some(GREEN));
        assert_eq!(image.get_pixel(24, 24).0, [r, g, b, 255]);
        assert_eq!(image.get_pixel(8, 8), BASE.get_pixel(8, 8));
        assert_eq!(badged(None), *BASE);
    }

    #[test]
    fn notification_text_is_cut_to_fit_and_terminated() {
        let mut buffer = [1u16; 4];
        copy_into(&mut buffer, "abcdef");
        assert_eq!(buffer, [97, 98, 99, 0]);
    }
}
