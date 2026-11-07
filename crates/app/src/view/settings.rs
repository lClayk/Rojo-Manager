//! Settings, and the Tools dialog it opens for global tool setup.

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rojo_manager_core::manager::Snapshot;
use rojo_manager_core::tooling::{Job, RojoState, StudioPlugin};
use rojo_manager_core::updates::UpdateState;

use super::MainView;
use crate::agents::{self, Agent, Failure};
use crate::logic;
use crate::theme::*;
use crate::widgets::{Button, Dialog, Variant, icon, panel, row, section, switch, tab_stop, tinted_row};

/// Setup steps for an agent the app cannot add itself, shown over Settings.
pub struct AgentHelp {
    title: String,
    message: String,
    /// The config or command to copy.
    code: String,
    copy: &'static str,
    /// List where common MCP clients keep their config file.
    clients: bool,
}

impl AgentHelp {
    fn other(path: &str) -> Self {
        AgentHelp {
            title: "Other MCP clients".into(),
            message: "Add this to your MCP client's config file. If the file already has \"mcpServers\", \
                      put only the rojo-manager entry inside it."
                .into(),
            code: agents::config_snippet(path),
            copy: "Copy config",
            clients: true,
        }
    }

    fn manual(agent: Agent, path: &str, reason: &str) -> Self {
        AgentHelp {
            title: format!("Add to {}", agent.name()),
            message: format!("{}. Run this in a terminal instead:", reason.trim_end_matches('.')),
            code: agent.command_line(path),
            copy: "Copy command",
            clients: false,
        }
    }
}

const CLIENTS: [(&str, &str); 3] = [
    ("Claude Desktop", r"%APPDATA%\Claude\claude_desktop_config.json"),
    ("Cursor", r"%USERPROFILE%\.cursor\mcp.json"),
    ("Windsurf", r"%USERPROFILE%\.codeium\windsurf\mcp_config.json"),
];

/// Read a setting from the snapshot; launch_at_startup lives in the registry, not config.json.
fn value(snapshot: &Snapshot, key: &str) -> bool {
    match key {
        "launch_at_startup" => snapshot.launch_at_startup,
        key => snapshot.settings.get(key).unwrap_or(false),
    }
}

impl MainView {
    fn set_setting(&mut self, key: &'static str, value: bool, cx: &mut Context<Self>) {
        self.run(cx, move |engine| engine.set_setting(key, value), |_, (), _| {});
    }

    /// Run the agent's `mcp add` off the UI thread; if that cannot work, show the command to run instead.
    fn add_to_agent(&mut self, key: &'static str, agent: Agent, cx: &mut Context<Self>) {
        let Some(path) = self.snapshot.as_ref().map(|s| s.mcp_path.clone()) else { return };
        self.adding = Some(key);
        cx.notify();
        let task = cx.background_executor().spawn({
            let path = path.clone();
            async move { agents::add(agent, &path) }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.adding = None;
                match result {
                    Ok(()) => {
                        this.flash(key, "Added", cx);
                        this.notify(format!("Added to {}. Start a new session to use it.", agent.name()), cx);
                    }
                    Err(Failure::Missing) => {
                        let reason = format!("{}'s command line tool was not found", agent.name());
                        this.agent_help = Some(AgentHelp::manual(agent, &path, &reason));
                        cx.notify();
                    }
                    Err(Failure::Failed(message)) => {
                        this.agent_help = Some(AgentHelp::manual(agent, &path, &message));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}

fn toggle(
    snapshot: &Snapshot,
    key: &'static str,
    label: &'static str,
    hint: Option<&'static str>,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let on = value(snapshot, key);
    let flip = cx.listener(move |this, _: &ClickEvent, _, cx| this.set_setting(key, !on, cx));
    tab_stop(div().id(key).rounded(px(11.)))
        .on_click(flip)
        .child(row(label, hint.map(Into::into), switch(format!("{key}-switch"), on)))
        .into_any_element()
}

pub fn render(view: &MainView, snapshot: &Snapshot, cx: &mut Context<MainView>) -> Div {
    let busy = snapshot.tool_job.is_some();
    let update = &snapshot.app_update;
    let bridge = matches!(snapshot.studio_plugin, StudioPlugin::Bridge { .. });
    // Updating Rojo rebuilds the bridge too.
    let installing = snapshot.tool_job.as_ref().is_some_and(|job| {
        matches!(job.operation.as_str(), "bridge" | "plugin")
            || (bridge && job.operation == "global" && job.tool == "Rojo")
    });
    let rojo = snapshot.tool_versions.get("Rojo").cloned().flatten();
    let plugin = logic::plugin_row(&snapshot.studio_plugin, rojo.as_deref(), installing);
    let shortcuts = [
        ("show_wally", "Wally install button", "On projects with wally.toml"),
        ("show_rokit", "Rokit install button", "On projects with rokit.toml"),
        ("show_aftman", "Aftman install button", "On projects with aftman.toml"),
    ];

    let mut general = vec![toggle(snapshot, "launch_at_startup", "Launch at Windows startup", None, cx)];
    // Starting hidden only applies to launches at Windows startup.
    if snapshot.launch_at_startup {
        general.push(toggle(snapshot, "start_hidden", "Start hidden in the tray", None, cx));
    }
    general.extend([
        toggle(snapshot, "tray", "Keep running in the tray", Some("Closing the window leaves servers running"), cx),
        toggle(snapshot, "tray_status", "Show server status on the tray icon", None, cx),
        toggle(snapshot, "notifications", "Notify when a server fails", Some("While the window is closed"), cx),
        toggle(snapshot, "app_version_check", "Check for app updates on launch", None, cx),
    ]);

    let mut agents = vec![
        tinted_row(
            "Studio plugin with agent bridge",
            Some(plugin.hint.into()),
            if plugin.warn { YELLOW } else { MUTED },
            Button::new("bridge")
                .label(plugin.button)
                .variant(if plugin.primary { Variant::Primary } else { Variant::Outline })
                .disabled(busy)
                .on_click(cx.listener(|this, _, _, cx| this.tool_job(Job::Bridge { tool: "Rojo".into() }, cx))),
        )
        .into_any_element(),
    ];
    // The bridge reports how many Rojo plugins Studio runs; until it has, the Creator Store copy
    // being installed (it may be turned off) is only worth a reminder.
    match snapshot.studio_rojo_plugins {
        Some(count) if bridge && count > 1 => agents.push(
            tinted_row(
                "Two Rojo plugins are running",
                Some("Turn off the Creator Store Rojo in Studio under Plugins > Manage Plugins.".into()),
                YELLOW,
                icon("icons/triangle-alert.svg", 16., YELLOW),
            )
            .into_any_element(),
        ),
        None if bridge && snapshot.creator_store_rojo => agents.push(
            row(
                "Creator Store Rojo",
                Some("Also installed. Keep it off in Studio under Plugins > Manage Plugins.".into()),
                div(),
            )
            .into_any_element(),
        ),
        _ => {}
    }
    let setup = |key: &'static str, label: &'static str, agent: Agent| {
        let label = if view.adding == Some(key) { "Adding…" } else { view.flashed(key).unwrap_or(label) };
        Button::new(key)
            .label(label)
            .width(104.)
            .disabled(view.adding.is_some())
            .on_click(cx.listener(move |this, _, _, cx| this.add_to_agent(key, agent, cx)))
    };
    let other = Button::new("mcp-config").label("Other").width(104.).tip("Any other MCP client").on_click(cx.listener(
        |this, _, _, cx| {
            let path = this.snapshot.as_ref().map(|s| s.mcp_path.clone()).unwrap_or_default();
            this.agent_help = Some(AgentHelp::other(&path));
            cx.notify();
        },
    ));
    agents.push(
        row(
            "Add to an agent",
            None,
            div()
                .flex()
                .flex_shrink_0()
                .gap(px(8.))
                .child(setup("claude", "Claude Code", Agent::Claude))
                .child(setup("codex", "Codex", Agent::Codex))
                .child(other),
        )
        .into_any_element(),
    );

    let body = div()
        .id("settings-body")
        .flex()
        .flex_col()
        .gap(px(20.))
        .p(px(16.))
        .min_h_0()
        .track_scroll(&view.settings_scroll)
        .overflow_y_scrollbar()
        .child(section("General", general))
        .child(section(
            "Rojo and tools",
            [
                row(
                    "Tools",
                    Some("Install or update Rojo, Rokit, Wally, Lune, and the formatters.".into()),
                    Button::new("open-tools").label("Open").on_click(cx.listener(|this, _, _, cx| {
                        this.tools_open = true;
                        this.engine.refresh_tool_versions();
                        cx.notify();
                    })),
                )
                .into_any_element(),
                toggle(snapshot, "version_check", "Check for Rojo updates on launch", None, cx),
                toggle(snapshot, "restart_crashed", "Restart servers that crash", Some("Up to 3 times a minute"), cx),
                toggle(
                    snapshot,
                    "auto_connect_accept",
                    "Skip Rojo's sync prompt on auto-connect",
                    Some("Applies the project to Studio without asking"),
                    cx,
                ),
            ]
            .into_iter()
            .chain(shortcuts.map(|(key, label, hint)| toggle(snapshot, key, label, Some(hint), cx))),
        ))
        .child(section("Agents", agents))
        .child(section(
            "About",
            [row(
                format!("Rojo Manager {}", snapshot.version),
                (!update.message.is_empty()).then(|| update.message.clone().into()),
                div()
                    .flex()
                    .flex_shrink_0()
                    .gap(px(8.))
                    .when(matches!(update.state, UpdateState::Unavailable | UpdateState::Error), |d| {
                        d.child(
                            Button::new("open-release")
                                .label("Open releases")
                                .on_click(cx.listener(|this, _, _, cx| this.open_release(cx))),
                        )
                    })
                    .map(|d| match update.state {
                        UpdateState::Outdated | UpdateState::Installing => d.child(
                            Button::new("settings-install-update")
                                .label(if update.state == UpdateState::Installing { "Updating" } else { "Update now" })
                                .variant(Variant::Primary)
                                .disabled(update.state == UpdateState::Installing)
                                .on_click(cx.listener(|this, _, _, cx| this.confirm_update(cx))),
                        ),
                        _ => d.child(
                            Button::new("check-update")
                                .label("Check now")
                                .disabled(update.state == UpdateState::Checking)
                                .on_click(cx.listener(|this, _, _, _| this.engine.check_app_update())),
                        ),
                    }),
            )
            .into_any_element()],
        ));

    Dialog {
        title: "Settings".into_any_element(),
        actions: vec![],
        body: body.into_any_element(),
        max_width: 520.,
        tall: false,
    }
    .render(
        "settings",
        MainView::action(cx, |this, _, cx| {
            this.settings_open = false;
            cx.notify();
        }),
    )
}

/// Global tool setup through Rokit, separate from installing a project's own dependencies.
pub fn render_tools(_: &MainView, snapshot: &Snapshot, cx: &mut Context<MainView>) -> Div {
    let busy = snapshot.tool_job.is_some();
    let rojo = &snapshot.rojo;
    let tool_row =
        |id: String, name: SharedString, version: Option<SharedString>, description: &'static str, button: Button| {
            div()
                .flex()
                .items_center()
                .gap(px(16.))
                .px(px(16.))
                .py(px(12.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .id(SharedString::from(id))
                                .flex()
                                .items_baseline()
                                .gap(px(8.))
                                .child(
                                    div()
                                        .text_size(TEXT_BASE)
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(rgb(TEXT))
                                        .child(name),
                                )
                                .when_some(version, |d, version| {
                                    d.child(div().truncate().text_size(TEXT_SM).text_color(rgb(MUTED)).child(version))
                                }),
                        )
                        .child(div().mt(px(2.)).text_size(TEXT_SM).text_color(rgb(MUTED)).child(description)),
                )
                .child(button)
                .into_any_element()
        };

    let mut rows: Vec<AnyElement> = snapshot
        .tools
        .iter()
        .map(|tool| {
            let version = snapshot.tool_versions.get(tool.name);
            let installed = matches!(version, Some(Some(_)));
            let label = version.map(|v| match v {
                Some(output) => SharedString::from(logic::version_number(output).to_string()),
                None => "Not installed".into(),
            });
            let name = tool.name;
            tool_row(
                format!("tool-{name}"),
                name.into(),
                label,
                tool.description,
                Button::new(format!("install-{name}"))
                    .label(if installed { "Update" } else { "Install" })
                    .width(72.)
                    .disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.tool_job(Job::Global { tool: name.into() }, cx))),
            )
        })
        .collect();
    // Installing here keeps the bridge when it is there, so Studio only ever gets one Rojo plugin.
    let (plugin, plugin_action) = match &snapshot.studio_plugin {
        StudioPlugin::Missing => ("Not installed", "Install"),
        StudioPlugin::Official => ("Installed", "Update"),
        StudioPlugin::Bridge { .. } => ("With agent bridge", "Update"),
    };
    rows.push(tool_row(
        "tool-plugin".into(),
        "Studio plugin".into(),
        Some(plugin.into()),
        "The official Rojo plugin for Roblox Studio",
        Button::new("install-plugin")
            .label(plugin_action)
            .width(72.)
            .disabled(busy)
            .on_click(cx.listener(|this, _, _, cx| this.tool_job(Job::Plugin { tool: "Rojo".into() }, cx))),
    ));

    let body = div()
        .flex()
        .flex_col()
        .gap(px(12.))
        .p(px(16.))
        .when(!rojo.message.is_empty(), |d| {
            d.child(
                div()
                    .px(px(2.))
                    .text_size(TEXT_SM)
                    .text_color(rgb(if rojo.action.is_some() { YELLOW } else { MUTED }))
                    .child(rojo.message.clone()),
            )
        })
        .child(panel(rows))
        .child(div().px(px(2.)).text_size(TEXT_SM).text_color(rgb(MUTED)).child(
            "Tool setup uses Rokit globally. Project version pins stay unchanged. Aftman projects remain supported through Rokit.",
        ));

    Dialog {
        title: "Tools".into_any_element(),
        actions: vec![
            Button::new("check-rojo")
                .label("Check Rojo")
                .disabled(busy || rojo.state == RojoState::Checking)
                .on_click(cx.listener(|this, _, _, _| this.engine.check_rojo()))
                .into_any_element(),
        ],
        body: body.into_any_element(),
        max_width: 520.,
        tall: false,
    }
    .render(
        "tools",
        MainView::action(cx, |this, _, cx| {
            this.tools_open = false;
            cx.notify();
        }),
    )
}

/// The steps for adding rojo-manager by hand: what to paste or run, with a copy button.
pub fn render_agent_help(view: &MainView, help: &AgentHelp, cx: &mut Context<MainView>) -> Div {
    let close = MainView::action(cx, |this, _, cx| {
        this.agent_help = None;
        cx.notify();
    });
    let close_click = close.clone();
    let code = div()
        .p(px(10.))
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(CARD))
        .font_family(MONO)
        .text_size(TEXT_SM)
        .line_height(px(19.))
        .text_color(rgb(TEXT))
        .child(help.code.clone());
    let clients = help.clients.then(|| {
        let rows = CLIENTS.iter().map(|(name, file)| {
            div()
                .flex()
                .flex_col()
                .child(div().text_color(rgb(TEXT)).child(*name))
                .child(div().font_family(MONO).text_size(TEXT_SM).text_color(rgb(MUTED)).child(*file))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .children(rows)
            .child(div().text_color(rgb(MUTED)).child("VS Code uses \"servers\" in place of \"mcpServers\"."))
    });
    // Scrolls so the buttons stay reachable in a short window.
    let body = div()
        .id("agent-help-body")
        .min_h_0()
        .overflow_y_scroll()
        .flex()
        .flex_col()
        .gap(px(12.))
        .px(px(16.))
        .pt(px(14.))
        .pb(px(16.))
        .text_size(TEXT_BASE)
        .line_height(px(20.))
        .child(div().text_color(rgb(TEXT)).child(help.message.clone()))
        .child(code)
        .children(clients)
        .child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.))
                .pt(px(4.))
                .child(Button::new("agent-help-done").label("Done").on_click(move |_, w, cx| close_click(w, cx)))
                .child(
                    Button::new("agent-help-copy")
                        .label(view.flashed("agent-help-copy").unwrap_or(help.copy))
                        .variant(Variant::Primary)
                        .width(112.)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(code) = this.agent_help.as_ref().map(|help| help.code.clone()) {
                                this.copy("agent-help-copy", code, cx);
                            }
                        })),
                ),
        );
    Dialog {
        title: help.title.clone().into_any_element(),
        actions: vec![],
        body: body.into_any_element(),
        max_width: 460.,
        tall: false,
    }
    .render("agent-help", close)
}
