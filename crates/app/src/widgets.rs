//! Small shared controls in T3 Code's dark style: buttons, row links, switches, panels, settings
//! rows, and the dialog frame.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::theme::*;

pub type Handler = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// A callback that needs no event: dialog close buttons and scrims.
pub type Callback = Rc<dyn Fn(&mut Window, &mut App)>;

/// A Lucide icon from assets/icons, drawn in `color`.
pub fn icon(name: &'static str, size: f32, color: u32) -> Svg {
    svg().path(format!("icons/{name}.svg")).size(px(size)).flex_shrink_0().text_color(rgb(color))
}

/// A dark tooltip builder for `.tooltip(...)`.
pub fn tooltip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

thread_local! {
    static TABBABLE: Cell<bool> = const { Cell::new(true) };
}

/// Build controls that Tab reaches when `on`, or skips, such as those under an open dialog.
pub fn tabbable<R>(on: bool, build: impl FnOnce() -> R) -> R {
    let outer = TABBABLE.replace(on);
    let built = build();
    TABBABLE.set(outer);
    built
}

/// Give `element` a border (`border`, or transparent) that turns into a focus ring, and make it
/// a Tab stop when `tab`. Enter and Space then click it. The ring is white, so it shows on
/// blue buttons too, and a focused control shows even where it is hidden until hover.
fn focusable<E: InteractiveElement + Styled>(element: E, border: Option<u32>, tab: bool) -> E {
    let element = element.border_1().border_color(border.map_or(transparent_black(), |c| rgb(c).into()));
    if tab { element.tab_index(0).focus_visible(|s| s.border_color(rgb(TEXT)).opacity(1.)) } else { element }
}

/// A clickable element, such as a whole settings row, as a Tab stop with a focus ring.
pub fn tab_stop<E: InteractiveElement + Styled>(element: E) -> E {
    focusable(element, None, TABBABLE.get())
}

fn with_tip<E: StatefulInteractiveElement>(element: E, tip: Option<SharedString>) -> E {
    match tip {
        Some(tip) => element.tooltip(tooltip(tip)),
        None => element,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Outline,
    Primary,
    Ghost,
    Danger,
}

/// A 28px button: text, icon, or both.
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<&'static str>,
    variant: Variant,
    disabled: bool,
    tip: Option<SharedString>,
    width: Option<Pixels>,
    on_click: Option<Handler>,
    tabbable: bool,
}

impl Button {
    pub fn new(id: impl Into<ElementId>) -> Self {
        Button {
            id: id.into(),
            label: None,
            icon: None,
            variant: Variant::Outline,
            disabled: false,
            tip: None,
            width: None,
            on_click: None,
            tabbable: TABBABLE.get(),
        }
    }

    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn icon(mut self, name: &'static str) -> Self {
        self.icon = Some(name);
        self
    }

    pub fn variant(mut self, variant: Variant) -> Self {
        self.variant = variant;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn tip(mut self, tip: impl Into<SharedString>) -> Self {
        self.tip = Some(tip.into());
        self
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(px(width));
        self
    }

    pub fn on_click(mut self, handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (bg, hover, fg, hover_fg) = match self.variant {
            Variant::Outline => (Some(CONTROL), CONTROL_HOVER, TEXT, TEXT),
            Variant::Primary => (Some(ACCENT), ACCENT_HOVER, 0xffffff, 0xffffff),
            Variant::Ghost => (None, CONTROL_HOVER, MUTED, TEXT),
            Variant::Danger => (Some(DANGER), DANGER_HOVER, 0xffffff, 0xffffff),
        };
        let group: SharedString = format!("button-{}", self.id).into();
        let enabled = !self.disabled;
        let icon_only = self.label.is_none();
        // After a click the tooltip stays hidden until the pointer leaves, as in Windows.
        let clicked = window.use_keyed_state(SharedString::from(format!("{group}-clicked")), cx, |_, _| false);
        let tip = self.tip.filter(|_| !*clicked.read(cx));
        let button = div()
            .id(self.id)
            .group(group.clone())
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .h(px(28.))
            .rounded(px(6.))
            .when(icon_only, |b| b.w(px(28.)))
            .when(!icon_only, |b| b.px(px(10.)))
            .when_some(self.width, |b, width| b.w(width))
            .text_size(TEXT_BASE)
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(fg))
            .when_some(bg, |b, bg| b.bg(rgb(bg)))
            .map(|b| focusable(b, (self.variant == Variant::Outline).then_some(INPUT), enabled && self.tabbable))
            .when_some(self.icon, |b, name| {
                b.child(
                    icon(name, 15., fg)
                        .when(enabled, |i| i.group_hover(group.clone(), |s| s.text_color(rgb(hover_fg)))),
                )
            })
            .when_some(self.label, |b, label| b.child(label))
            .when(self.disabled, |b| b.opacity(0.4))
            .when(enabled, |b| {
                let (pressed, left) = (clicked.clone(), clicked);
                b.hover(|s| s.bg(rgb(hover)).text_color(rgb(hover_fg)))
                    .on_hover(move |hovered, _, cx| {
                        if !hovered {
                            left.update(cx, |clicked, cx| {
                                *clicked = false;
                                cx.notify();
                            });
                        }
                    })
                    .when_some(self.on_click, |b, handler| {
                        b.on_click(move |e, w, cx| {
                            pressed.update(cx, |clicked, _| *clicked = true);
                            handler(e, w, cx)
                        })
                    })
            });
        with_tip(button, tip)
    }
}

/// A compact muted link for the secondary actions in rows (Port, Set place, Logs, ...).
pub fn link(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    link_hover(id, label, TEXT)
}

/// A row link whose text turns `hover_text` under the pointer (Remove turns red).
pub fn link_hover(id: impl Into<ElementId>, label: impl Into<SharedString>, hover_text: u32) -> Stateful<Div> {
    focusable(div().id(id), None, TABBABLE.get())
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(6.))
        .h(px(24.))
        .px(px(6.))
        .rounded(px(6.))
        .text_size(TEXT_SM)
        .text_color(rgb(MUTED))
        .whitespace_nowrap()
        .hover(move |s| s.bg(rgb(CONTROL_HOVER)).text_color(rgb(hover_text)))
        .child(label.into())
}

/// A link with a tooltip.
pub fn tip_link(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    tip: impl Into<SharedString>,
) -> Stateful<Div> {
    with_tip(link(id, label), Some(tip.into()))
}

/// The pill and knob of a switch. Rounded shapes are anti-aliased by the GPU, so no edge artifacts.
fn switch_track(on: bool, width: f32, knob: f32) -> Div {
    let inset = 2.;
    let height = knob + inset * 2.;
    div()
        .relative()
        .flex_shrink_0()
        .w(px(width))
        .h(px(height))
        .rounded_full()
        .bg(rgb(if on { ACCENT } else { INPUT }))
        .child(
            div()
                .absolute()
                .top(px(inset))
                .left(px(if on { width - knob - inset } else { inset }))
                .size(px(knob))
                .rounded_full()
                .bg(rgb(0xffffff)),
        )
}

/// A settings switch.
pub fn switch(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    div().id(id).flex_shrink_0().child(switch_track(on, 32., 14.))
}

/// A small switch and its label as one row link (Autostart, Auto-connect).
pub fn switch_link(id: impl Into<ElementId>, label: &'static str, on: bool, tip: &'static str) -> Stateful<Div> {
    let row = focusable(div().id(id), None, TABBABLE.get())
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap(px(6.))
        .h(px(24.))
        .px(px(6.))
        .rounded(px(6.))
        .text_size(TEXT_SM)
        .text_color(rgb(MUTED))
        .whitespace_nowrap()
        .hover(|s| s.bg(rgb(CONTROL_HOVER)).text_color(rgb(TEXT)))
        .child(switch_track(on, 24., 10.))
        .child(label);
    with_tip(row, Some(tip.into()))
}

/// A raised card whose rows are separated by hairlines.
pub fn panel(rows: impl IntoIterator<Item = AnyElement>) -> Div {
    let mut panel = div().flex().flex_col().rounded(px(12.)).border_1().border_color(rgb(BORDER)).bg(rgb(CARD));
    for (index, row) in rows.into_iter().enumerate() {
        panel = panel.child(div().when(index > 0, |d| d.border_t_1().border_color(rgb(BORDER))).child(row));
    }
    panel
}

/// A settings-style row: label and optional hint on the left, a control on the right.
pub fn row(label: impl Into<SharedString>, hint: Option<SharedString>, control: impl IntoElement) -> Div {
    tinted_row(label, hint, MUTED, control)
}

/// A row whose hint has its own color, such as a warning.
pub fn tinted_row(
    label: impl Into<SharedString>,
    hint: Option<SharedString>,
    hint_color: u32,
    control: impl IntoElement,
) -> Div {
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
                .child(div().text_size(TEXT_BASE).text_color(rgb(TEXT)).child(label.into()))
                .when_some(hint, |d, hint| {
                    d.child(div().mt(px(2.)).text_size(TEXT_SM).text_color(rgb(hint_color)).child(hint))
                }),
        )
        .child(control)
}

/// A section heading and its panel, as in Settings.
pub fn section(title: &'static str, rows: impl IntoIterator<Item = AnyElement>) -> Div {
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .mb(px(8.))
                .px(px(2.))
                .text_size(TEXT_BASE)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(rgb(TEXT))
                .child(title),
        )
        .child(panel(rows))
}

/// Height of the app's title bar, which holds the header actions.
pub const TITLE_BAR: f32 = 44.;
/// Width of the minimize, maximize, and close buttons together; macOS puts its traffic lights on
/// the left instead.
const WINDOW_CONTROLS: f32 = if cfg!(target_os = "macos") { 0. } else { 3. * 34. };
/// Space before the title bar's content, past the traffic lights on macOS.
pub const TITLE_BAR_INSET: f32 = if cfg!(target_os = "macos") { 84. } else { 16. };

/// A modal panel over the window: a scrim that closes it on click, a header with the title,
/// optional actions and a close button, then the body. The scrim also dims the title bar but
/// keeps it a drag handle, and leaves the window controls working.
pub struct Dialog {
    pub title: AnyElement,
    pub actions: Vec<AnyElement>,
    pub body: AnyElement,
    pub max_width: f32,
    /// Fill the available height (logs) instead of fitting the content.
    pub tall: bool,
}

impl Dialog {
    pub fn render(self, id: &'static str, on_close: Callback) -> Div {
        let close = on_close.clone();
        let title_scrim = || div().absolute().top_0().h(px(TITLE_BAR)).bg(rgba(SCRIM));
        let body = div()
            .id(id)
            .absolute()
            .top(px(TITLE_BAR))
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .flex()
            .justify_center()
            .items_start()
            .p(px(12.))
            .pt(px(16.))
            .bg(rgba(SCRIM))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| close(window, cx))
            .child(
                div()
                    .id("dialog-panel")
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(self.max_width))
                    .max_h_full()
                    .when(self.tall, |d| d.h_full().max_h(px(640.)))
                    .rounded(px(12.))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(BG))
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap(px(8.))
                            .h(px(48.))
                            .pl(px(16.))
                            .pr(px(10.))
                            .border_b_1()
                            .border_color(rgb(BORDER))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(TEXT_LG)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(self.title),
                            )
                            .children(self.actions)
                            .child(
                                Button::new("dialog-close")
                                    .icon("x")
                                    .variant(Variant::Ghost)
                                    .tip("Close")
                                    .on_click(move |_, window, cx| on_close(window, cx)),
                            ),
                    )
                    .child(self.body),
            );
        div()
            .absolute()
            .inset_0()
            .child(
                title_scrim()
                    .id("dialog-title-scrim")
                    .left_0()
                    .right(px(WINDOW_CONTROLS))
                    .occlude()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(title_scrim().right_0().w(px(WINDOW_CONTROLS)))
            .child(body)
    }
}
