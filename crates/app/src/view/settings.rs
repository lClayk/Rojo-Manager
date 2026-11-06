//! Settings, and the Tools dialog it opens for global tool setup.

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rojo_manager_core::manager::Snapshot;
use rojo_manager_core::tooling::{Job, RojoState, StudioPlugin};
use rojo_manager_core::updates::UpdateState;

use super::MainView;
use crate::logic;
use crate::theme::*;
use crate::widgets::{Button, Dialog, Variant, icon, panel, row, section, switch, tab_stop, tinted_row};

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
    let setup = |id: &'static str, label: &'static str, cli: &'static str| {
        Button::new(id).label(if view.copied(id) { "Copied" } else { label }).width(104.).on_click(cx.listener(
            move |this, _, _, cx| {
                let path = this.snapshot.as_ref().map(|s| s.mcp_path.clone()).unwrap_or_default();
                this.copy(id, format!("{cli} mcp add rojo-manager -- \"{path}\""), cx);
            },
        ))
    };
    let (claude, codex) = (setup("claude", "Claude Code", "claude"), setup("codex", "Codex", "codex"));
    agents.push(
        row(
            "Add to an agent",
            Some("Copies a command to run in a terminal.".into()),
            div().flex().flex_shrink_0().gap(px(8.)).child(claude).child(codex),
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
