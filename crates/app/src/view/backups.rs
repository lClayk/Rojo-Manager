//! A project's backups: copies of instances that existed only in Studio, made just before Rojo
//! removed them. Opened from the history icon on a project row; each one can be restored into the
//! project's open place.

use gpui_kit::*;

use super::MainView;
use crate::logic;
use crate::theme::*;
use crate::widgets::{Button, Dialog};

pub fn render(view: &MainView, name: &str, cx: &mut Context<MainView>) -> Div {
    let backups = view.project(name).map(|p| p.backups.clone()).unwrap_or_default();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let rows = backups.into_iter().map(|backup| {
        let restorable = backup.restorable();
        let (project, id) = (name.to_string(), backup.id.clone());
        let detail = match &backup.error {
            Some(error) => div().text_color(rgb(RED)).child(format!("Backup failed: {error}")),
            None => div().text_color(rgb(SECONDARY)).child(logic::backup_paths(&backup.paths, &backup.skipped)),
        };
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(12.))
            .py(px(10.))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .text_size(TEXT_SM)
                    .child(div().text_color(rgb(TEXT)).child(logic::ago(now, backup.time)))
                    .child(detail.min_w_0().truncate()),
            )
            .child(
                Button::new(format!("restore-{}", backup.id))
                    .label("Restore")
                    .disabled(!restorable)
                    .tip("Put these back under their old parents in the open place")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let (project, id) = (project.clone(), id.clone());
                        let label = project.clone();
                        this.run(
                            cx,
                            move |engine| engine.restore_backup(&project, &id),
                            move |view, message: String, cx| view.notify(format!("{label}: {message}"), cx),
                        );
                    })),
            )
            .into_any_element()
    });
    let body = div()
        .flex_1()
        .min_h_0()
        .p(px(12.))
        .child(
            // Scrolls, since the kept backups (backups::KEEP) outgrow a short window.
            div()
                .id("backup-list")
                .size_full()
                .rounded(px(8.))
                .border_1()
                .border_color(rgb(BORDER))
                .bg(rgb(CARD))
                .overflow_y_scroll()
                .children(rows),
        )
        .child(
            div()
                .pt(px(10.))
                .text_size(TEXT_SM)
                .text_color(rgb(MUTED))
                .child("Rojo removes restored instances again on its next sync unless the project ignores them."),
        );
    let title = div().flex().gap(px(6.)).child(div().text_color(rgb(MUTED)).child("Backups")).child(name.to_string());
    Dialog {
        title: title.into_any_element(),
        actions: Vec::new(),
        body: body.into_any_element(),
        max_width: 640.,
        tall: true,
    }
    .render(
        "backups",
        MainView::action(cx, |this, _, cx| {
            this.backups = None;
            cx.notify();
        }),
    )
}
