//! One project in the list: name, status, main button, folder, and its inline settings and links.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rojo_manager_core::config::Place;
use rojo_manager_core::manager::{Engine, ProjectView, Snapshot, Update};
use rojo_manager_core::studio::ResolveError;
use rojo_manager_core::tooling::Job;

use super::{Confirm, Field, Logs, MainView};
use crate::ipc::Client;
use crate::logic::{self, MainAction, StatusColor};
use crate::theme::*;
use crate::widgets::{Button, link_hover, switch_link, tip_link};

fn dot(color: StatusColor) -> u32 {
    match color {
        StatusColor::Idle => IDLE,
        StatusColor::Yellow => YELLOW,
        StatusColor::Green => GREEN,
        StatusColor::Blue => BLUE,
        StatusColor::Red => RED,
    }
}

type RowAction = fn(&mut MainView, &str, &mut Window, &mut Context<MainView>);

/// A click handler that runs `f` for the named project.
fn on_click(
    name: &str,
    cx: &mut Context<MainView>,
    f: RowAction,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let name = name.to_string();
    cx.listener(move |this, _: &ClickEvent, window, cx| f(this, &name, window, cx))
}

pub fn render(
    view: &MainView,
    p: &ProjectView,
    snapshot: &Snapshot,
    _: &mut Window,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let status = logic::row_status(p.state);
    let (parent, folder) = logic::split_path(&p.path);
    let editing = view.editing.as_ref().filter(|e| e.project == p.name).map(|e| (e.field, e.input.clone()));
    let editor = |field: Field, width: Pixels| {
        editing.as_ref().filter(|(f, _)| *f == field).map(|(_, input)| {
            div().w(width).max_w_full().child(Input::new(input).small().text_size(TEXT_SM)).into_any_element()
        })
    };
    let group: SharedString = format!("row-{}", p.name).into();

    let title = editor(Field::Name, px(320.)).unwrap_or_else(|| {
        div()
            .id(format!("name-{}", p.name))
            .min_w_0()
            .mx(px(-4.))
            .px(px(4.))
            .rounded(px(4.))
            .truncate()
            .text_size(TEXT_LG)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(TEXT))
            .hover(|s| s.text_color(rgb(BLUE)))
            .tooltip(crate::widgets::tooltip(format!("{}\nRename. Enter saves; Escape cancels.", p.name)))
            .on_click(on_click(&p.name, cx, |this, name, window, cx| {
                this.edit(name, Field::Name, name.to_string(), window, cx)
            }))
            .child(p.name.clone())
            .into_any_element()
    });

    let top = div()
        .flex()
        .items_center()
        .gap(px(12.))
        .child(div().flex().flex_1().min_w_0().items_center().child(title))
        .child(
            div()
                .id(format!("status-{}", p.name))
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap(px(6.))
                .text_size(TEXT_SM)
                .text_color(rgb(MUTED))
                .tooltip(crate::widgets::tooltip(status.tip))
                .child(div().size(px(8.)).rounded_full().bg(rgb(dot(status.color))))
                .child(status.label),
        )
        .child(
            Button::new(format!("main-{}", p.name))
                .label(status.action.label())
                .width(72.)
                .tip(if status.action == MainAction::Kill {
                    "Force-close what holds this port".to_string()
                } else {
                    format!("Start or stop {}", p.name)
                })
                .on_click(on_click(&p.name, cx, main_action)),
        );

    let path = div()
        .id(format!("path-{}", p.name))
        .flex()
        .max_w_full()
        .min_w_0()
        .mt(px(2.))
        .mx(px(-4.))
        .px(px(4.))
        .rounded(px(4.))
        .text_size(TEXT_SM)
        .text_color(rgb(SECONDARY))
        .hover(|s| s.text_color(rgb(TEXT)))
        .tooltip(crate::widgets::tooltip(format!("{}\nOpen project folder", p.path)))
        .on_click(on_click(&p.name, cx, |this, name, _, cx| {
            let name = name.to_string();
            this.run(cx, move |engine| engine.open_folder(&name), |_, (), _| {});
        }))
        .child(div().min_w_0().truncate().child(parent.to_string()))
        .child(div().flex_shrink_0().child(folder.to_string()));

    let port = editor(Field::Port, px(72.)).unwrap_or_else(|| {
        tip_link(format!("port-{}", p.name), format!("Port {}", p.port), "Edit port. Enter saves; Escape cancels.")
            .on_click(on_click(&p.name, cx, |this, name, window, cx| {
                let port = this.project(name).map(|p| p.port.to_string()).unwrap_or_default();
                this.edit(name, Field::Port, port, window, cx);
            }))
            .into_any_element()
    });
    let place = editor(Field::Place, px(320.)).unwrap_or_else(|| {
        tip_link(
            format!("place-{}", p.name),
            match &p.place {
                Some(place) => format!("Place {}", place.place_id),
                None => "Set place".to_string(),
            },
            "Roblox place this project opens in Studio. Paste a place ID or link; an experience link uses its start place. Clear it to unset.",
        )
        .on_click(on_click(&p.name, cx, |this, name, window, cx| {
            let place = this.project(name).and_then(|p| p.place).map(|p| p.place_id.to_string()).unwrap_or_default();
            this.edit(name, Field::Place, place, window, cx);
        }))
        .into_any_element()
    });

    let links = div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(2.))
        .gap_y(px(4.))
        .mt(px(6.))
        .mx(px(-6.))
        .child(port)
        .child(
            switch_link(format!("autostart-{}", p.name), "Autostart", p.autostart, "Start this project when Rojo Manager opens")
                .on_click(on_click(&p.name, cx, |this, name, _, cx| this.toggle_flag(name, Flag::Autostart, cx))),
        )
        .when(p.place.is_some(), |d| {
            d.child(
                switch_link(
                    format!("auto-connect-{}", p.name),
                    "Auto-connect",
                    p.auto_connect,
                    "Connect Rojo when this place opens in Studio while the server is running. Needs the latest Rojo bridge plugin (Settings > Agents).",
                )
                .on_click(on_click(&p.name, cx, |this, name, _, cx| this.toggle_flag(name, Flag::AutoConnect, cx))),
            )
        })
        .child(place)
        .when_some(p.place, |d, place| {
            d.child(
                tip_link(format!("studio-{}", p.name), "Open Studio", format!("Open place {} in Roblox Studio", place.place_id))
                    .on_click(on_click(&p.name, cx, |this, name, _, cx| {
                        let name = name.to_string();
                        this.run(cx, move |engine| engine.open_studio(&name), |this, place: Place, cx| {
                            this.notify(format!("Opening place {} in Roblox Studio", place.place_id), cx)
                        });
                    })),
            )
        })
        .children(logic::visible_badges(&p.badges, &snapshot.settings).into_iter().map(|badge| {
            let (tool, path) = (badge.to_string(), p.path.clone());
            tip_link(format!("badge-{}-{badge}", p.name), tool.clone(), logic::badge_tip(badge)).on_click(cx.listener(
                move |this, _, _, cx| this.tool_job(Job::Project { tool: tool.clone(), path: path.clone() }, cx),
            ))
        }))
        .child(
            // Logs and Remove show on hover; a focused one shows too (see `focusable`).
            div()
                .flex()
                .ml_auto()
                .child(
                    tip_link(format!("logs-{}", p.name), "Logs", format!("View logs for {}", p.name))
                        .opacity(0.)
                        .group_hover(group.clone(), |s| s.opacity(1.))
                        .on_click(on_click(&p.name, cx, |this, name, _, cx| this.open_logs(name, cx))),
                )
                .child(
                    link_hover(format!("remove-{}", p.name), "Remove", RED)
                        .opacity(0.)
                        .group_hover(group.clone(), |s| s.opacity(1.))
                        .tooltip(crate::widgets::tooltip("Remove from this list. Project files are kept."))
                        .on_click(on_click(&p.name, cx, |this, name, _, cx| this.remove(name, cx))),
                ),
        );

    div()
        .id(format!("row-{}", p.name))
        .group(group)
        .px(px(16.))
        .py(px(12.))
        .hover(|s| s.bg(rgb(CARD_HOVER)))
        .child(top)
        .child(path)
        .child(links)
        .into_any_element()
}

#[derive(Clone, Copy)]
enum Flag {
    Autostart,
    AutoConnect,
}

/// The main row button: start, stop, cancel, or kill what holds the port (after asking).
fn main_action(view: &mut MainView, name: &str, _: &mut Window, cx: &mut Context<MainView>) {
    let Some(project) = view.project(name) else { return };
    let name = name.to_string();
    if logic::row_status(project.state).action != MainAction::Kill {
        view.run(cx, move |engine| engine.toggle(&name), |_, (), _| {});
        return;
    }
    let port = project.port;
    let lookup = name.clone();
    view.run(cx, move |engine| engine.port_owner(&lookup), move |view, owner, cx| {
        let Some((pid, image)) = owner else {
            return view.notify(format!("Port {port} is free now."), cx);
        };
        if pid == 0 || pid == 4 {
            return view.notify(format!("Port {port} is held by Windows. Change this project's port instead."), cx);
        }
        view.ask(
            Confirm {
                title: "Kill process",
                message: format!(
                    "Port {port} is used by {image} (PID {pid}). Force-close it? Unsaved work in that application will be lost."
                ),
                label: "Force-close",
                danger: true,
                on_confirm: Box::new(move |view, cx| {
                    view.run(cx, move |engine| engine.kill_port_owner(&name, pid), |_, (), _| {});
                }),
            },
            cx,
        );
    });
}

/// Apply an inline edit; the window confirms changes it asked for itself.
pub(super) fn commit(view: &mut MainView, name: &str, field: Field, text: String, cx: &mut Context<MainView>) {
    let Some(project) = view.project(name) else { return };
    let name = name.to_string();
    match field {
        Field::Name => {
            if text == name || text.is_empty() {
                return;
            }
            let update = Update { name: Some(text.clone()), ..Update::default() };
            view.run(
                cx,
                move |engine| engine.update(&name, update).map(|_| name),
                move |view, old, cx| view.notify(format!("Renamed {old} to {text}"), cx),
            );
        }
        Field::Port => match logic::parse_port(&text) {
            Err(error) => view.notify(error, cx),
            Ok(port) if port == project.port => {}
            Ok(port) => {
                let update = Update { port: Some(port), ..Update::default() };
                let label = name.clone();
                view.run(
                    cx,
                    move |engine| engine.update(&name, update),
                    move |view, _, cx| view.notify(format!("{label}: port set to {port}"), cx),
                );
            }
        },
        Field::Place => {
            if text == project.place.map(|p| p.place_id.to_string()).unwrap_or_default() {
                return;
            }
            if !text.is_empty() {
                view.notify("Looking up place...", cx);
            }
            let label = name.clone();
            view.run(
                cx,
                move |engine| set_place(&engine, &name, &text),
                move |view, project, cx| {
                    view.notify(
                        match project.place {
                            Some(place) => format!("{label}: opens place {} in Studio", place.place_id),
                            None => format!("{label}: place cleared"),
                        },
                        cx,
                    )
                },
            );
        }
    }
}

/// Link a place ID or Roblox link to a project; empty text unlinks.
fn set_place(engine: &Client, name: &str, text: &str) -> Result<ProjectView, String> {
    let place = Engine::resolve_place(text).map_err(|error| match error {
        ResolveError::Invalid(message) => message,
        ResolveError::Unreachable(message) => format!("Could not reach Roblox: {message}"),
    })?;
    engine.update(name, Update { place: Some(place), ..Update::default() })
}

impl MainView {
    fn project(&self, name: &str) -> Option<&ProjectView> {
        self.snapshot.as_ref()?.projects.iter().find(|p| p.name == name)
    }

    fn toggle_flag(&mut self, name: &str, flag: Flag, cx: &mut Context<Self>) {
        let Some(project) = self.project(name) else { return };
        let (value, label, update) = match flag {
            Flag::Autostart => {
                let value = !project.autostart;
                (value, "autostart", Update { autostart: Some(value), ..Update::default() })
            }
            Flag::AutoConnect => {
                let value = !project.auto_connect;
                (value, "auto-connect", Update { auto_connect: Some(value), ..Update::default() })
            }
        };
        let name = name.to_string();
        let label = format!("{name}: {label} {}", if value { "on" } else { "off" });
        self.run(cx, move |engine| engine.update(&name, update), move |view, _, cx| view.notify(label, cx));
    }

    fn open_logs(&mut self, name: &str, cx: &mut Context<Self>) {
        self.logs = Some(Logs::new(name));
        let name = name.to_string();
        let backlog_name = name.clone();
        self.run(
            cx,
            move |engine| engine.logs(&backlog_name),
            move |view, backlog, cx| {
                if let Some(logs) = view.logs.as_mut().filter(|logs| logs.name == name) {
                    logs.load(backlog);
                    cx.notify();
                }
            },
        );
        cx.notify();
    }

    fn remove(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(project) = self.project(name) else { return };
        let name = name.to_string();
        let remove = move |view: &mut MainView, cx: &mut Context<MainView>| {
            let label = name.clone();
            view.run(
                cx,
                move |engine| engine.remove(&name),
                move |view, (), cx| view.notify(format!("Removed {label}"), cx),
            );
        };
        if !logic::is_active(project.state) {
            return remove(self, cx);
        }
        let message =
            format!("{} is currently running on port {}. Stop the server and remove?", project.name, project.port);
        self.ask(
            Confirm { title: "Server running", message, label: "Remove", danger: true, on_confirm: Box::new(remove) },
            cx,
        );
    }
}
