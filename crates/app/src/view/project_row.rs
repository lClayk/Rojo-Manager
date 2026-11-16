//! One project in the list: name, status, main button, folder, and its inline settings and links.

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::Input;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rojo_manager_core::config::PlaceLink;
use rojo_manager_core::manager::{Engine, PendingSync, ProjectView, ServerState, Snapshot, Update};
use rojo_manager_core::studio::ResolveError;
use rojo_manager_core::tooling::Job;

use super::{Confirm, Field, Logs, MainView};
use crate::ipc::Client;
use crate::logic::{self, MainAction, StatusColor};
use crate::theme::*;
use crate::widgets::{Button, icon, link_hover, switch_link, tip_link};

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

/// An experience's header: click to show or hide its places; start, stop, or open them all.
pub fn render_experience(
    universe_id: u64,
    name: &str,
    projects: &[&ProjectView],
    open: bool,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let names: Vec<String> = projects.iter().map(|p| p.name.clone()).collect();
    let action = |id: &str, label: &'static str, tip: &'static str, action: GroupAction| {
        let names = names.clone();
        tip_link(format!("experience-{id}-{universe_id}"), label, tip)
            .on_click(cx.listener(move |this, _, _, cx| this.group_action(&names, action, cx)))
    };
    let toggle = div()
        .id(format!("experience-toggle-{universe_id}"))
        .flex()
        .flex_1()
        .min_w_0()
        .items_center()
        .gap(px(8.))
        .tooltip(crate::widgets::tooltip(if open { "Hide its places" } else { "Show its places" }))
        .on_click(cx.listener(move |this, _, _, cx| this.set_collapsed(universe_id, open, cx)))
        .child(icon(if open { "chevron-down" } else { "chevron-right" }, 14., MUTED))
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(TEXT_LG)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(name.to_string()),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(TEXT_SM)
                .text_color(rgb(MUTED))
                .child(logic::group_summary(projects, "place")),
        );
    div()
        .id(format!("experience-{universe_id}"))
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(16.))
        .py(px(10.))
        .hover(|s| s.bg(rgb(CARD_HOVER)))
        .child(toggle)
        .child(
            div()
                .flex()
                .flex_shrink_0()
                .mr(px(-6.))
                .when(projects.iter().any(|p| p.state == ServerState::Off), |d| {
                    d.child(action("start", "Start all", "Start every place's server", GroupAction::Start))
                })
                .when(projects.iter().any(|p| logic::is_active(p.state)), |d| {
                    d.child(action("stop", "Stop all", "Stop every place's server", GroupAction::Stop))
                })
                .child(action("open", "Open places", "Open every place in Roblox Studio", GroupAction::Open)),
        )
        .into_any_element()
}

#[derive(Clone, Copy)]
pub(super) enum GroupAction {
    Start,
    Stop,
    Open,
}

/// One project in the list; `depth` indents it under a header, or a lane under its project.
pub fn render(
    view: &MainView,
    p: &ProjectView,
    snapshot: &Snapshot,
    depth: usize,
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
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap(px(8.))
                .child(title)
                .when_some(logic::lane_label(p), |d, label| {
                    d.child(div().flex_shrink_0().text_size(TEXT_SM).text_color(rgb(MUTED)).child(label))
                })
                .when(!p.backups.is_empty(), |d| {
                    let tip = format!("Backups of {} removals Rojo made in Studio", p.backups.len());
                    d.child(
                        div()
                            .id(format!("backups-{}", p.name))
                            .flex_shrink_0()
                            .text_color(rgb(MUTED))
                            .hover(|s| s.text_color(rgb(TEXT)))
                            .tooltip(crate::widgets::tooltip(tip))
                            .on_click(on_click(&p.name, cx, |this, name, _, cx| {
                                this.backups = Some(name.to_string());
                                cx.notify();
                            }))
                            .child(icon("history", 14., MUTED)),
                    )
                }),
        )
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
                Some(PlaceLink::Published(place)) => format!("Place {}", place.place_id),
                Some(PlaceLink::Local(_)) => "Local place".to_string(),
                None => "Set place".to_string(),
            },
            "Roblox place this project opens in Studio. Paste a place ID or link; an experience link uses its start place. Type local for an unpublished place file built from the project. Clear it to unset.",
        )
        .on_click(on_click(&p.name, cx, |this, name, window, cx| {
            let place = this.project(name).map(|p| logic::place_text(p.place.as_ref())).unwrap_or_default();
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
        .when(p.lane.is_none(), |d| {
            d.child(
                switch_link(format!("autostart-{}", p.name), "Autostart", p.autostart, "Start this project when Rojo Manager opens")
                    .on_click(on_click(&p.name, cx, |this, name, _, cx| this.toggle_flag(name, Flag::Autostart, cx))),
            )
        })
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
        .child(
            switch_link(
                format!("sourcemap-{}", p.name),
                "Sourcemap",
                p.sourcemap,
                "Keep sourcemap.json up to date for luau-lsp while the server runs (rojo sourcemap --watch)",
            )
            .on_click(on_click(&p.name, cx, |this, name, _, cx| this.toggle_flag(name, Flag::Sourcemap, cx))),
        )
        .child(place)
        .when_some(p.place.as_ref(), |d, place| {
            d.child(
                tip_link(format!("studio-{}", p.name), "Open Studio", format!("Open {} in Roblox Studio", place.describe()))
                    .on_click(on_click(&p.name, cx, |this, name, _, cx| {
                        let name = name.to_string();
                        this.run(cx, move |engine| engine.open_studio(&name), |this, place: PlaceLink, cx| {
                            this.notify(format!("Opening {} in Roblox Studio", place.describe()), cx)
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
        .pl(px(16. + 22. * depth as f32))
        .pr(px(16.))
        .py(px(12.))
        .hover(|s| s.bg(rgb(CARD_HOVER)))
        .child(top)
        .child(path)
        .child(links)
        .when_some(p.pending_sync.as_ref(), |d, pending| d.child(render_pending(&p.name, pending, cx)))
        .into_any_element()
}

/// The line under a project whose Studio waits for Rojo's initial sync: what accepting would do, with
/// Accept and Abort.
fn render_pending(name: &str, pending: &PendingSync, cx: &mut Context<MainView>) -> Div {
    let answer = |accept: bool| {
        let instance = pending.instance.clone();
        cx.listener(move |this: &mut MainView, _: &ClickEvent, _, cx| {
            let instance = instance.clone();
            this.run(cx, move |engine| engine.answer_sync(&instance, accept), |_, (), _| {});
        })
    };
    let tip = pending.summary.as_ref().map(logic::sync_paths).unwrap_or_default();
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_x(px(6.))
        .mt(px(6.))
        .text_size(TEXT_SM)
        .child(
            div()
                .id(format!("pending-{name}"))
                .flex()
                .flex_wrap()
                .gap_x(px(6.))
                .when(!tip.is_empty(), |d| d.tooltip(crate::widgets::tooltip(tip)))
                .child(div().text_color(rgb(TEXT)).child("Studio waits to sync:"))
                .children(
                    logic::sync_parts(pending.summary.as_ref())
                        .into_iter()
                        .map(|(part, warn)| div().text_color(rgb(if warn { RED } else { MUTED })).child(part)),
                ),
        )
        .child(
            div()
                .flex()
                .ml_auto()
                .mr(px(-6.))
                .child(
                    tip_link(format!("accept-{name}"), "Accept", "Apply the project to Studio").on_click(answer(true)),
                )
                .child(
                    tip_link(format!("abort-{name}"), "Abort", "Cancel this sync and disconnect")
                        .on_click(answer(false)),
                ),
        )
}

#[derive(Clone, Copy)]
enum Flag {
    Autostart,
    AutoConnect,
    Sourcemap,
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
            if text.eq_ignore_ascii_case(&logic::place_text(project.place.as_ref())) {
                return;
            }
            if !text.is_empty() && !text.eq_ignore_ascii_case("local") {
                view.notify("Looking up place...", cx);
            }
            let label = name.clone();
            view.run(
                cx,
                move |engine| set_place(&engine, &name, &text),
                move |view, project, cx| {
                    view.notify(
                        match project.place {
                            Some(place) => format!("{label}: opens {} in Studio", place.describe()),
                            None => format!("{label}: place cleared"),
                        },
                        cx,
                    )
                },
            );
        }
    }
}

/// Link a place ID or Roblox link, or "local", to a project; empty text unlinks.
fn set_place(engine: &Client, name: &str, text: &str) -> Result<ProjectView, String> {
    let place = Engine::resolve_place(text).map_err(|error| match error {
        ResolveError::Invalid(message) => message,
        ResolveError::Unreachable(message) => format!("Could not reach Roblox: {message}"),
    })?;
    engine.update(name, Update { place: Some(place), ..Update::default() })
}

impl MainView {
    pub(super) fn project(&self, name: &str) -> Option<&ProjectView> {
        self.snapshot.as_ref()?.projects.iter().find(|p| p.name == name)
    }

    fn set_collapsed(&mut self, universe_id: u64, collapsed: bool, cx: &mut Context<Self>) {
        self.run(cx, move |engine| engine.set_collapsed(universe_id, collapsed), |_, (), _| {});
    }

    /// Start the stopped servers, stop the running ones, or open every place, of one experience.
    fn group_action(&mut self, names: &[String], action: GroupAction, cx: &mut Context<Self>) {
        for name in names {
            let Some(project) = self.project(name) else { continue };
            let applies = match action {
                GroupAction::Start => project.state == ServerState::Off,
                GroupAction::Stop => logic::is_active(project.state),
                GroupAction::Open => true,
            };
            if !applies {
                continue;
            }
            let name = name.clone();
            match action {
                GroupAction::Start => self.run(cx, move |engine| engine.start_server(&name), |_, (), _| {}),
                GroupAction::Stop => self.run(cx, move |engine| engine.stop_server(&name), |_, (), _| {}),
                GroupAction::Open => self.run(cx, move |engine| engine.open_studio(&name), |_, _: PlaceLink, _| {}),
            }
        }
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
            Flag::Sourcemap => {
                let value = !project.sourcemap;
                (value, "sourcemap", Update { sourcemap: Some(value), ..Update::default() })
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
        if project.lane.is_some() {
            let message = format!(
                "Remove lane {}? Its Studio window closes, its server stops, and its place file is deleted.",
                project.name
            );
            let on_confirm = Box::new(remove);
            return self.ask(Confirm { title: "Remove lane", message, label: "Remove", danger: true, on_confirm }, cx);
        }
        let local = matches!(project.place, Some(PlaceLink::Local(_)));
        if !local && !logic::is_active(project.state) {
            return remove(self, cx);
        }
        let (title, mut message) = match logic::is_active(project.state) {
            true => {
                let message = format!(
                    "{} is currently running on port {}. Stop the server and remove?",
                    project.name, project.port
                );
                ("Server running", message)
            }
            false => ("Remove project", format!("Remove {}?", project.name)),
        };
        if local {
            message.push_str(" Its local place file is deleted.");
        }
        self.ask(Confirm { title, message, label: "Remove", danger: true, on_confirm: Box::new(remove) }, cx);
    }
}
