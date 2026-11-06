//! A project's captured Rojo output: the backlog from the engine, then live lines as they arrive.
//! Only the visible lines are laid out, so a full 2000-line log scrolls smoothly.

use gpui_kit::*;

use super::MainView;
use crate::logic::{self, LOG_CAP};
use crate::theme::*;
use crate::widgets::{Button, Dialog};

pub struct Logs {
    pub name: String,
    lines: Vec<(u64, String)>,
    /// Live lines that arrived before the backlog; merged by sequence number once it lands.
    pending: Option<Vec<(u64, String)>>,
    follow: bool,
    scroll: UniformListScrollHandle,
}

impl Logs {
    pub fn new(name: &str) -> Self {
        Logs {
            name: name.to_string(),
            lines: Vec::new(),
            pending: Some(Vec::new()),
            follow: true,
            scroll: UniformListScrollHandle::new(),
        }
    }

    pub fn load(&mut self, backlog: Vec<(u64, String)>) {
        logic::merge_log(&mut self.lines, backlog, LOG_CAP);
        let pending = self.pending.take().unwrap_or_default();
        logic::merge_log(&mut self.lines, pending, LOG_CAP);
        self.stick();
    }

    pub fn push(&mut self, seq: u64, line: String) {
        match &mut self.pending {
            Some(pending) => pending.push((seq, line)),
            None => {
                logic::merge_log(&mut self.lines, [(seq, line)], LOG_CAP);
                self.stick();
            }
        }
    }

    fn stick(&self) {
        if self.follow && !self.lines.is_empty() {
            self.scroll.scroll_to_item(self.lines.len() - 1, ScrollStrategy::Bottom);
        }
    }

    fn text(&self) -> String {
        self.lines.iter().map(|(_, line)| line.as_str()).collect::<Vec<_>>().join("\n")
    }
}

pub fn render(view: &MainView, logs: &Logs, cx: &mut Context<MainView>) -> Div {
    let count = logs.lines.len();
    let box_style = |d: Div| d.size_full().rounded(px(8.)).border_1().border_color(rgb(BORDER)).bg(rgb(CARD));
    let body = div().flex_1().min_h_0().p(px(12.)).child(if count == 0 {
        box_style(div())
            .px(px(12.))
            .py(px(10.))
            .text_size(TEXT_SM)
            .text_color(rgb(MUTED))
            .child("No output yet. Start this project's server to capture logs.")
            .into_any_element()
    } else {
        let list = uniform_list(
            "log-lines",
            count,
            cx.processor(|this, range: std::ops::Range<usize>, _, _| {
                let Some(logs) = this.logs.as_ref() else { return Vec::new() };
                logs.lines[range.start.min(logs.lines.len())..range.end.min(logs.lines.len())]
                    .iter()
                    .map(|(_, line)| div().px(px(12.)).whitespace_nowrap().child(line.clone()))
                    .collect()
            }),
        )
        .size_full()
        .py(px(8.))
        .font_family(MONO)
        .text_size(TEXT_SM)
        .line_height(px(19.))
        .text_color(rgb(TEXT))
        .track_scroll(&logs.scroll);
        box_style(div()).overflow_hidden().child(list).into_any_element()
    });

    let follow = logs.follow;
    let actions = vec![
        div()
            .id("follow")
            .flex()
            .items_center()
            .gap(px(8.))
            .mr(px(4.))
            .text_size(TEXT_SM)
            .text_color(rgb(MUTED))
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(logs) = this.logs.as_mut() {
                    logs.follow = !logs.follow;
                    logs.stick();
                }
                cx.notify();
            }))
            .child(crate::widgets::switch("follow-switch", follow))
            .child("Follow output")
            .into_any_element(),
        Button::new("copy-logs")
            .label(if view.copied("logs") { "Copied" } else { "Copy logs" })
            .disabled(count == 0)
            .on_click(cx.listener(|this, _, _, cx| {
                let text = this.logs.as_ref().map(Logs::text).unwrap_or_default();
                this.copy("logs", text, cx);
            }))
            .into_any_element(),
    ];

    let title = div().flex().gap(px(6.)).child(div().text_color(rgb(MUTED)).child("Logs")).child(logs.name.clone());

    Dialog { title: title.into_any_element(), actions, body: body.into_any_element(), max_width: 760., tall: true }
        .render(
            "logs",
            MainView::action(cx, |this, _, cx| {
                this.logs = None;
                cx.notify();
            }),
        )
}
