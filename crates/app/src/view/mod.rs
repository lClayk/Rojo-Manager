//! The window: header actions in the title bar, status banners, the project list, the notice
//! footer, and the dialogs over it (settings, tools, logs, confirmations).

mod logs;
mod project_row;
mod settings;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::TitleBar;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rojo_manager_core::manager::{Event, Snapshot};
use rojo_manager_core::tooling::Job;
use rojo_manager_core::updates::UpdateState;

use crate::ipc::Client;
use crate::logic;
use crate::theme::*;
use crate::widgets::{Button, Callback, Dialog, TITLE_BAR, Variant, tabbable};

pub use logs::Logs;
use settings::AgentHelp;

const NOTICE: Duration = Duration::from_secs(6);
const FLASH: Duration = Duration::from_secs(2);

/// Which project field an inline editor is changing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Port,
    Place,
}

struct Editing {
    project: String,
    field: Field,
    input: Entity<InputState>,
    _events: Subscription,
}

type OnConfirm = Box<dyn FnOnce(&mut MainView, &mut Context<MainView>)>;

struct Confirm {
    title: &'static str,
    message: String,
    label: &'static str,
    danger: bool,
    on_confirm: OnConfirm,
}

pub struct MainView {
    engine: Arc<Client>,
    snapshot: Option<Snapshot>,
    focus: FocusHandle,
    /// The layer on top: 0 for the list, then Settings, Tools, Logs, and a confirmation.
    layer: u8,
    notice: Option<SharedString>,
    notice_timer: Option<Task<()>>,
    /// A snapshot fetch is running; `stale` asks for one more when it lands.
    fetching: bool,
    stale: bool,
    settings_open: bool,
    tools_open: bool,
    logs: Option<Logs>,
    confirm: Option<Confirm>,
    editing: Option<Editing>,
    /// The button that briefly shows an outcome ("Copied", "Added") instead of its label.
    flash: Option<(&'static str, &'static str, Task<()>)>,
    /// The agent button whose `mcp add` is running.
    adding: Option<&'static str>,
    /// Manual setup steps for an agent, over Settings.
    agent_help: Option<AgentHelp>,
    list_scroll: ScrollHandle,
    settings_scroll: ScrollHandle,
}

impl MainView {
    pub fn new(engine: Arc<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let mut view = MainView {
            engine,
            snapshot: None,
            focus,
            layer: 0,
            notice: None,
            notice_timer: None,
            fetching: false,
            stale: false,
            settings_open: false,
            tools_open: false,
            logs: None,
            confirm: None,
            editing: None,
            flash: None,
            adding: None,
            agent_help: None,
            list_scroll: ScrollHandle::new(),
            settings_scroll: ScrollHandle::new(),
        };
        view.refresh(cx);
        view
    }

    /// An engine event, forwarded while the window exists.
    pub fn on_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Changed => self.refresh(cx),
            Event::Notice(message) | Event::Alert(message) => self.notify(message, cx),
            Event::Log { project, seq, line } => {
                if let Some(logs) = self.logs.as_mut().filter(|logs| logs.name == project) {
                    logs.push(seq, line);
                    cx.notify();
                }
            }
        }
    }

    /// Fetch a fresh snapshot off the UI thread; bursts of changes coalesce into one more fetch.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.fetching {
            self.stale = true;
            return;
        }
        self.fetching = true;
        let engine = Arc::clone(&self.engine);
        let fetch = cx.background_executor().spawn(async move { engine.snapshot() });
        cx.spawn(async move |this, cx| {
            let snapshot = fetch.await;
            this.update(cx, |this, cx| {
                // Fails only when the tray app is gone, and this process exits with it.
                if let Ok(snapshot) = snapshot {
                    this.snapshot = Some(snapshot);
                }
                this.fetching = false;
                if std::mem::take(&mut this.stale) {
                    this.refresh(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A short message in the footer for six seconds.
    pub fn notify(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.notice = Some(message.into());
        self.notice_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE).await;
            this.update(cx, |this, cx| {
                this.notice = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Run an engine call on a background thread; an error is shown as a notice, success goes to `then`.
    fn run<T: Send + 'static>(
        &mut self,
        cx: &mut Context<Self>,
        work: impl FnOnce(Arc<Client>) -> Result<T, String> + Send + 'static,
        then: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        let engine = Arc::clone(&self.engine);
        let task = cx.background_executor().spawn(async move { work(engine) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(value) => then(this, value, cx),
                Err(message) => this.notify(message, cx),
            })
            .ok();
        })
        .detach();
    }

    fn tool_job(&mut self, job: Job, cx: &mut Context<Self>) {
        self.run(cx, move |engine| engine.run_tool_job(job), |_, (), _| {});
    }

    fn copy(&mut self, key: &'static str, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.flash(key, "Copied", cx);
    }

    /// Show `label` on the button `key` for two seconds.
    fn flash(&mut self, key: &'static str, label: &'static str, cx: &mut Context<Self>) {
        let reset = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLASH).await;
            this.update(cx, |this, cx| {
                this.flash = None;
                cx.notify();
            })
            .ok();
        });
        self.flash = Some((key, label, reset));
        cx.notify();
    }

    fn flashed(&self, key: &str) -> Option<&'static str> {
        self.flash.as_ref().filter(|(flashed, ..)| *flashed == key).map(|(_, label, _)| *label)
    }

    fn add_project(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select project or parent folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            this.update(cx, |this, cx| {
                this.run(cx, move |engine| engine.add_projects(&path), |_, _, _| {});
            })
            .ok();
        })
        .detach();
    }

    /// A view callback that needs no event, for dialog close buttons and scrims.
    fn action(cx: &mut Context<Self>, f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static) -> Callback {
        let view = cx.entity().downgrade();
        Rc::new(move |window, cx| {
            view.update(cx, |this, cx| f(this, window, cx)).ok();
        })
    }

    fn ask(&mut self, confirm: Confirm, cx: &mut Context<Self>) {
        self.confirm = Some(confirm);
        cx.notify();
    }

    /// Start editing a project field in place: the input takes focus with its text selected.
    fn edit(&mut self, project: &str, field: Field, value: String, window: &mut Window, cx: &mut Context<Self>) {
        let placeholder = if field == Field::Place { "Place ID or Roblox link" } else { "" };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value).placeholder(placeholder));
        let events = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| match event {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_edit(window, cx),
            _ => {}
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.editing = Some(Editing { project: project.to_string(), field, input, _events: events });
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editing) = self.editing.take() else { return };
        window.focus(&self.focus, cx);
        let text = editing.input.read(cx).value().trim().to_string();
        project_row::commit(self, &editing.project, editing.field, text, cx);
        cx.notify();
    }

    /// Escape closes the topmost layer: an inline editor, a confirmation, then dialogs.
    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return self.cancel_edit(window, cx);
        }
        if self.confirm.take().is_none()
            && self.agent_help.take().is_none()
            && !std::mem::take(&mut self.tools_open)
            && self.logs.take().is_none()
        {
            self.settings_open = false;
        }
        cx.notify();
    }

    fn render_header(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> impl IntoElement {
        let projects = &snapshot.projects;
        TitleBar::new().h(px(TITLE_BAR)).pl(px(16.)).child(
            div()
                .flex()
                .items_center()
                .w_full()
                .pr(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_end()
                        .gap(px(6.))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(TEXT))
                                .child("Rojo Manager"),
                        )
                        .child(
                            // Clicks here open the link instead of dragging the window.
                            div()
                                .id("author")
                                .flex_shrink_0()
                                .pb(px(1.))
                                .text_size(px(11.))
                                .text_color(rgb(MUTED))
                                .cursor_pointer()
                                .hover(|s| s.text_color(rgb(TEXT)))
                                .occlude()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_click(cx.listener(|this, _, _, cx| this.open_author(cx)))
                                .child("by Clayk"),
                        ),
                )
                .child(
                    // Clicks here are the app's, not the title bar's window drag.
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Button::new("add-project")
                                .icon("plus")
                                .label("Add project")
                                .variant(Variant::Primary)
                                .on_click(cx.listener(|this, _, _, cx| this.add_project(cx))),
                        )
                        .child(div().w(px(4.)))
                        .child(
                            Button::new("start-all")
                                .icon("play")
                                .variant(Variant::Ghost)
                                .tip("Start all stopped servers")
                                .disabled(!logic::can_start_all(projects))
                                .on_click(cx.listener(|this, _, _, _| this.engine.start_all())),
                        )
                        .child(
                            Button::new("stop-all")
                                .icon("square")
                                .variant(Variant::Ghost)
                                .tip("Stop all servers")
                                .disabled(!logic::can_stop_all(projects))
                                .on_click(cx.listener(|this, _, _, _| this.engine.stop_all())),
                        )
                        .child(
                            Button::new("settings").icon("settings").variant(Variant::Ghost).tip("Settings").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.settings_open = true;
                                    // The plugin row compares the bridge with the installed Rojo.
                                    this.engine.refresh_tool_versions();
                                    cx.notify();
                                }),
                            ),
                        ),
                ),
        )
    }

    /// A status line under the header with its one fix (update Rojo, view the app update).
    fn banner(icon: &'static str, tone: u32, message: String, action: impl IntoElement) -> Div {
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(10.))
            .mx(px(16.))
            .mb(px(8.))
            .py(px(6.))
            .pl(px(12.))
            .pr(px(6.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(BORDER))
            .bg(rgb(CARD))
            .child(crate::widgets::icon(icon, 14., tone))
            .child(div().flex_1().min_w_0().text_size(TEXT_SM).text_color(rgb(TEXT)).child(message))
            .child(action)
    }

    fn render_banners(&self, snapshot: &Snapshot, cx: &mut Context<Self>) -> Vec<Div> {
        let mut banners = Vec::new();
        if let Some(action) = snapshot.rojo.action.clone() {
            let retry = action == "Retry";
            banners.push(Self::banner(
                "triangle-alert",
                YELLOW,
                snapshot.rojo.message.clone(),
                Button::new("rojo-fix").label(action).disabled(snapshot.tool_job.is_some()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        if retry {
                            this.engine.check_rojo();
                        } else {
                            this.tool_job(Job::Global { tool: "Rojo".into() }, cx);
                        }
                    },
                )),
            ));
        }
        let rojo = snapshot.tool_versions.get("Rojo").cloned().flatten();
        if let Some(message) = logic::bridge_outdated(&snapshot.studio_plugin, rojo.as_deref()) {
            let installing = settings::installing_bridge(snapshot);
            banners.push(Self::banner(
                "circle-arrow-up",
                BLUE,
                message,
                Button::new("update-bridge")
                    .label(if installing { "Updating" } else { "Update bridge" })
                    .variant(Variant::Primary)
                    .disabled(snapshot.tool_job.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.tool_job(Job::Bridge { tool: "Rojo".into() }, cx))),
            ));
        }
        let update = &snapshot.app_update;
        let installing = update.state == UpdateState::Installing;
        if update.state == UpdateState::Outdated || installing {
            banners.push(Self::banner(
                "circle-arrow-up",
                BLUE,
                update.message.clone(),
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap(px(8.))
                    .child(
                        Button::new("view-update")
                            .label("What's new")
                            .variant(Variant::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| this.open_release(cx))),
                    )
                    .child(
                        Button::new("install-update")
                            .label(if installing { "Updating" } else { "Update now" })
                            .variant(Variant::Primary)
                            .disabled(installing)
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_update(cx))),
                    ),
            ));
        }
        banners
    }

    /// Ask before installing: the app restarts, and with it the servers it runs.
    pub(crate) fn confirm_update(&mut self, cx: &mut Context<Self>) {
        use rojo_manager_core::manager::ServerState;
        let running = self.snapshot.as_ref().map(|snapshot| {
            let restarts =
                |state| matches!(state, ServerState::Starting | ServerState::Serving | ServerState::Connected);
            snapshot.projects.iter().filter(|project| restarts(project.state)).count()
        });
        let reconnect = "stop for a few seconds and start again, so reconnect Rojo in Studio afterwards.";
        let servers = match running {
            Some(0) => String::new(),
            Some(1) => format!(" Its running server will {reconnect}"),
            Some(count) => format!(" Its {count} running servers will {reconnect}"),
            None => format!(" Servers it runs will {reconnect}"),
        };
        self.ask(
            Confirm {
                title: "Install update",
                message: format!("Rojo Manager restarts to install the update.{servers}"),
                label: "Update and restart",
                danger: false,
                on_confirm: Box::new(|view, cx| view.install_update(cx)),
            },
            cx,
        );
    }

    /// Download the update; the tray app then restarts into it, reopening this window.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        self.run(cx, |engine| engine.install_app_update(), |_, _, _| {});
    }

    fn open_author(&mut self, cx: &mut Context<Self>) {
        const AUTHOR: &str = "https://github.com/lClayk";
        if rojo_manager_core::shell::open(AUTHOR).is_err() {
            self.notify(format!("Open {AUTHOR} in your browser."), cx);
        }
    }

    /// Open the release page from the latest update check; the URL is always the app's own releases.
    fn open_release(&mut self, cx: &mut Context<Self>) {
        use rojo_manager_core::{shell, updates};
        let url = self.snapshot.as_ref().map(|s| s.app_update.url.clone()).unwrap_or_default();
        let url = if url.starts_with(updates::RELEASES_URL) { url } else { updates::RELEASES_URL.to_string() };
        if shell::open(&url).is_err() {
            self.notify(format!("Open {} in your browser to download the update.", updates::RELEASES_URL), cx);
        }
    }

    fn render_list(&self, snapshot: &Snapshot, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if snapshot.projects.is_empty() {
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(6.))
                .px(px(24.))
                .child(
                    div()
                        .id("add-first")
                        .px(px(4.))
                        .rounded(px(4.))
                        .text_size(TEXT_LG)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(BLUE))
                        .hover(|s| s.text_color(rgb(TEXT)))
                        .on_click(cx.listener(|this, _, _, cx| this.add_project(cx)))
                        .child("Add your first project"),
                )
                .child(
                    div()
                        .max_w(px(340.))
                        .text_center()
                        .text_size(TEXT_SM)
                        .text_color(rgb(MUTED))
                        .child("Choose a Rojo project folder or a parent folder to import multiple projects."),
                )
                .into_any_element();
        }
        let mut rows = Vec::new();
        for item in logic::list_items(&snapshot.projects, snapshot.settings.group_experiences) {
            match item {
                logic::ListItem::Project(project) => {
                    rows.push(project_row::render(self, project, snapshot, false, window, cx))
                }
                logic::ListItem::Experience { universe_id, name, projects } => {
                    let open = !snapshot.collapsed.contains(&universe_id);
                    rows.push(project_row::render_experience(universe_id, &name, &projects, open, cx));
                    if open {
                        rows.extend(projects.iter().map(|p| project_row::render(self, p, snapshot, true, window, cx)));
                    }
                }
            }
        }
        div()
            .id("projects")
            .flex_1()
            .min_h_0()
            .px(px(16.))
            .pb(px(8.))
            .track_scroll(&self.list_scroll)
            .overflow_y_scrollbar()
            .child(crate::widgets::panel(rows).overflow_hidden())
            .into_any_element()
    }

    fn render_footer(&self, snapshot: &Snapshot) -> Div {
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px(4.))
            .px(px(20.))
            .pt(px(6.))
            .pb(px(10.))
            .text_size(TEXT_SM)
            .when_some(self.notice.clone(), |d, notice| d.child(div().text_color(rgb(TEXT)).child(notice)))
            .child(div().text_color(rgb(MUTED)).child(logic::summary(&snapshot.projects)))
    }

    fn render_confirm(&self, cx: &mut Context<Self>) -> Option<Div> {
        let confirm = self.confirm.as_ref()?;
        let close = Self::action(cx, |this, _, cx| {
            this.confirm = None;
            cx.notify();
        });
        let close_click = close.clone();
        let body = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(16.))
                    .pt(px(14.))
                    .pb(px(4.))
                    .text_size(TEXT_BASE)
                    .line_height(px(20.))
                    .text_color(rgb(TEXT))
                    .child(confirm.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .px(px(16.))
                    .pt(px(12.))
                    .pb(px(16.))
                    .child(Button::new("confirm-cancel").label("Cancel").on_click(move |_, w, cx| close_click(w, cx)))
                    .child(
                        Button::new("confirm-ok")
                            .label(confirm.label)
                            .variant(if confirm.danger { Variant::Danger } else { Variant::Primary })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(confirm) = this.confirm.take() {
                                    (confirm.on_confirm)(this, cx);
                                }
                                cx.notify();
                            })),
                    ),
            );
        Some(
            Dialog {
                title: confirm.title.into_any_element(),
                actions: vec![],
                body: body.into_any_element(),
                max_width: 400.,
                tall: false,
            }
            .render("confirm", close),
        )
    }
}

impl Focusable for MainView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .id("root")
            .track_focus(&self.focus)
            .key_context("RojoManager")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => this.escape(window, cx),
                    "tab" if event.keystroke.modifiers.shift => window.focus_prev(cx),
                    "tab" => window.focus_next(cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .font_family(FONT)
            .text_size(TEXT_BASE)
            .text_color(rgb(TEXT));
        let Some(snapshot) = self.snapshot.clone() else {
            return root.child(TitleBar::new().h(px(TITLE_BAR)));
        };
        // Only the topmost layer's controls are Tab stops: the list, or the dialog on top of it.
        let top: u8 = if self.confirm.is_some() || self.agent_help.is_some() {
            4
        } else if self.logs.is_some() {
            3
        } else if self.tools_open {
            2
        } else if self.settings_open {
            1
        } else {
            0
        };
        if top != self.layer {
            // The button that opened or closed a layer may no longer be a Tab stop; keys still
            // reach this view (Escape, Tab) when focus starts over from its root.
            self.layer = top;
            window.focus(&self.focus, cx);
        }
        let header = tabbable(top == 0, || self.render_header(&snapshot, cx).into_any_element());
        let banners = tabbable(top == 0, || self.render_banners(&snapshot, cx));
        let list = tabbable(top == 0, || self.render_list(&snapshot, window, cx));
        root.child(header)
            .children(banners)
            .child(list)
            .child(self.render_footer(&snapshot))
            .when(self.settings_open, |root| root.child(tabbable(top == 1, || settings::render(self, &snapshot, cx))))
            .when(self.tools_open, |root| {
                root.child(tabbable(top == 2, || settings::render_tools(self, &snapshot, cx)))
            })
            .when_some(
                self.logs.as_ref().map(|logs| tabbable(top == 3, || logs::render(self, logs, cx))),
                |root, logs| root.child(logs),
            )
            .when_some(
                self.agent_help.as_ref().map(|help| settings::render_agent_help(self, help, cx)),
                |root, help| root.child(help),
            )
            .children(self.render_confirm(cx))
    }
}
