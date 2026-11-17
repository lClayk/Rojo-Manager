//! Add project: an existing project folder, a new project from `rojo init`, or a project converted
//! from a place file. Opened from the Add project button.

use std::path::{Path, PathBuf};

use gpui_kit::*;

use super::MainView;
use crate::theme::*;
use crate::widgets::Dialog;

#[derive(Clone, Copy)]
enum Choice {
    Existing,
    New,
    Convert,
}

const CHOICES: [(Choice, &str, &str); 3] = [
    (Choice::Existing, "Existing folder", "A Rojo project, or a folder of them"),
    (Choice::New, "New project", "rojo init in a new folder, with a local place"),
    (Choice::Convert, "Convert place", "A .rbxl file's scripts become files; the place keeps its map"),
];

pub fn render(cx: &mut Context<MainView>) -> Div {
    let rows = CHOICES.into_iter().map(|(choice, label, hint)| {
        div()
            .id(label)
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(12.))
            .py(px(10.))
            .border_b_1()
            .border_color(rgb(BORDER))
            .hover(|s| s.bg(rgb(CARD_HOVER)))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.add_open = false;
                cx.notify();
                match choice {
                    Choice::Existing => this.add_existing(cx),
                    Choice::New => new_project(this, cx),
                    Choice::Convert => convert_place(cx),
                }
            }))
            .child(div().text_color(rgb(TEXT)).child(label))
            .child(div().text_size(TEXT_SM).text_color(rgb(MUTED)).child(hint))
    });
    let body = div().p(px(12.)).child(
        div().rounded(px(8.)).border_1().border_color(rgb(BORDER)).bg(rgb(CARD)).overflow_hidden().children(rows),
    );
    Dialog {
        title: "Add project".into_any_element(),
        actions: Vec::new(),
        body: body.into_any_element(),
        max_width: 400.,
        tall: false,
    }
    .render(
        "add",
        MainView::action(cx, |this, _, cx| {
            this.add_open = false;
            cx.notify();
        }),
    )
}

/// Where the save dialog starts: beside the first project, else the home folder.
fn start_folder(view: &MainView) -> PathBuf {
    let first = view.snapshot.as_ref().and_then(|s| s.projects.first()).map(|p| PathBuf::from(&p.path));
    first.and_then(|path| path.parent().map(Path::to_path_buf)).or_else(std::env::home_dir).unwrap_or_default()
}

/// Ask for the new project's folder, then make it.
fn new_project(view: &mut MainView, cx: &mut Context<MainView>) {
    let picked = cx.prompt_for_new_path(&start_folder(view), Some("NewGame"));
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(path))) = picked.await else { return };
        this.update(cx, |this, cx| this.run(cx, move |engine| engine.create_project(&path), |_, _, _| {})).ok();
    })
    .detach();
}

/// Ask for the place file, then for the project's folder beside it, named after it.
fn convert_place(cx: &mut Context<MainView>) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Select a .rbxl place file".into()),
    });
    cx.spawn(async move |this, cx| {
        let Ok(Ok(Some(paths))) = picked.await else { return };
        let Some(file) = paths.into_iter().next() else { return };
        let folder = file.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = file.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(destination) = this.update(cx, |_, cx| cx.prompt_for_new_path(&folder, Some(&name))) else { return };
        let Ok(Ok(Some(path))) = destination.await else { return };
        this.update(cx, |this, cx| this.run(cx, move |engine| engine.convert_place(&file, &path), |_, _, _| {})).ok();
    })
    .detach();
}
