//! The setup, shown once per profile (see Snapshot::setup): the Studio plugin, adding Rojo Manager
//! to agents, which agent tools to keep, and a few preferences. Each page does its work right away,
//! so skipping keeps what was done; Settings has the same controls.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use rojo_manager_core::manager::Snapshot;

use super::MainView;
use super::settings::{self, AgentHelp};
use crate::agents::Agent;
use crate::logic;
use crate::theme::*;
use crate::widgets::{Button, Dialog, Variant, icon, panel, row};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Plugin,
    Agents,
    Tools,
    Preferences,
}

impl Step {
    const ALL: [Step; 4] = [Step::Plugin, Step::Agents, Step::Tools, Step::Preferences];

    fn index(self) -> usize {
        Step::ALL.iter().position(|step| *step == self).unwrap_or(0)
    }

    fn title(self) -> &'static str {
        match self {
            Step::Plugin => "Install the Studio plugin",
            Step::Agents => "Add Rojo Manager to your agents",
            Step::Tools => "Choose agent tools",
            Step::Preferences => "A few preferences",
        }
    }
}

pub fn render(view: &MainView, snapshot: &Snapshot, cx: &mut Context<MainView>) -> Div {
    let step = view.setup_step;
    let index = step.index();
    let last = index + 1 == Step::ALL.len();
    let (intro, rows): (String, Vec<AnyElement>) = match step {
        Step::Plugin => (
            "Rojo's plugin with a small bridge, so Rojo connects on its own and agents can reach Studio.".into(),
            settings::plugin_rows(snapshot, cx),
        ),
        Step::Agents => ("Lets agents run your servers, open places, and work in Studio.".into(), agent_rows(view, cx)),
        Step::Tools => (
            match logic::found_mcps(&snapshot.studio_mcps) {
                Some(found) => format!("Your agents also have {found}. Turn off what those already cover."),
                None => "Turn off any your agents don't need.".into(),
            },
            settings::tool_toggles(snapshot, cx),
        ),
        Step::Preferences => ("You can change these any time in Settings.".into(), preference_rows(snapshot, cx)),
    };
    let go = |to: Option<Step>| {
        cx.listener(move |this: &mut MainView, _, _, cx| match to {
            Some(to) => {
                this.setup_step = to;
                cx.notify();
            }
            None => this.finish_setup(cx),
        })
    };
    let footer = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .when(index > 0, |d| {
            d.child(
                Button::new("setup-back")
                    .label("Back")
                    .variant(Variant::Ghost)
                    .on_click(go(Some(Step::ALL[index - 1]))),
            )
        })
        .child(div().flex_1())
        .when(!last, |d| {
            d.child(Button::new("setup-skip").label("Skip setup").variant(Variant::Ghost).on_click(go(None)))
        })
        .child(
            Button::new("setup-next")
                .label(if last { "Done" } else { "Next" })
                .variant(Variant::Primary)
                .width(72.)
                .on_click(go(Step::ALL.get(index + 1).copied())),
        );
    // The rows scroll, so Next stays reachable in a short window.
    let body = div()
        .flex()
        .flex_col()
        .min_h_0()
        .gap(px(12.))
        .p(px(16.))
        .child(div().px(px(2.)).text_size(TEXT_SM).text_color(rgb(MUTED)).child(intro))
        .child(div().id("setup-rows").min_h_0().overflow_y_scroll().child(panel(rows)))
        .child(footer.flex_shrink_0());
    Dialog {
        title: step.title().into_any_element(),
        actions: vec![progress(index).into_any_element()],
        body: body.into_any_element(),
        max_width: 460.,
        tall: false,
    }
    .render("setup", MainView::action(cx, |this, _, cx| this.finish_setup(cx)))
}

/// Starting with Windows, the tray, updates, and keeping the cursor out of hidden Studio.
fn preference_rows(snapshot: &Snapshot, cx: &mut Context<MainView>) -> Vec<AnyElement> {
    vec![
        settings::toggle(snapshot, "launch_at_startup", settings::STARTUP_LABEL, None, cx),
        settings::toggle(
            snapshot,
            "tray",
            "Keep running in the tray",
            Some("Closing the window leaves servers running"),
            cx,
        ),
        settings::toggle(
            snapshot,
            "auto_update",
            "Install updates on launch",
            Some("The app and the Studio bridge"),
            cx,
        ),
        settings::keep_cursor_row(snapshot, cx),
    ]
}

/// "2 of 4" with a bar per page, the ones reached in blue.
fn progress(index: usize) -> Div {
    let bars = Step::ALL
        .iter()
        .enumerate()
        .map(|(at, _)| div().w(px(14.)).h(px(3.)).rounded_full().bg(rgb(if at <= index { ACCENT } else { INPUT })));
    div().flex().items_center().gap(px(4.)).mr(px(6.)).children(bars).child(
        div().ml(px(6.)).text_size(TEXT_SM).text_color(rgb(MUTED)).child(format!(
            "{} of {}",
            index + 1,
            Step::ALL.len()
        )),
    )
}

/// Claude Code and Codex, added with one click or shown as added, and the config for any other client.
fn agent_rows(view: &MainView, cx: &mut Context<MainView>) -> Vec<AnyElement> {
    let mut rows: Vec<AnyElement> = [("setup-claude", Agent::Claude), ("setup-codex", Agent::Codex)]
        .into_iter()
        .map(|(key, agent)| {
            let control = if view.mcp_agents.contains(&agent.name()) {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .h(px(28.))
                    .text_size(TEXT_SM)
                    .text_color(rgb(GREEN))
                    .child(icon("check", 14., GREEN))
                    .child("Added")
                    .into_any_element()
            } else {
                let label = if view.adding == Some(key) { "Adding…" } else { view.flashed(key).unwrap_or("Add") };
                Button::new(key)
                    .label(label)
                    .width(72.)
                    .disabled(view.adding.is_some())
                    .on_click(cx.listener(move |this, _, _, cx| this.add_to_agent(key, agent, cx)))
                    .into_any_element()
            };
            row(agent.name(), None, control).into_any_element()
        })
        .collect();
    rows.push(
        row(
            "Other MCP client",
            Some("Cursor, Claude Desktop, Windsurf, and others".into()),
            Button::new("setup-other").label("Show config").on_click(cx.listener(|this, _, _, cx| {
                let path = this.snapshot.as_ref().map(|s| s.mcp_path.clone()).unwrap_or_default();
                this.agent_help = Some(AgentHelp::other(&path));
                cx.notify();
            })),
        )
        .into_any_element(),
    );
    rows
}
