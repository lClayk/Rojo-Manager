//! T3 Code's dark tokens on a true black background, and the component theme built from them.
//! Colors are 0xRRGGBB; use them as `rgb(theme::CARD)`.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, Pixels, px, rgb, rgba};

pub const BG: u32 = 0x000000;
pub const CARD: u32 = 0x111111;
pub const CARD_HOVER: u32 = 0x161616;
pub const CONTROL: u32 = 0x1c1c1c;
pub const CONTROL_HOVER: u32 = 0x262626;
pub const BORDER: u32 = 0x242424;
pub const INPUT: u32 = 0x2e2e2e;
pub const TEXT: u32 = 0xf5f5f5;
pub const MUTED: u32 = 0xa3a3a3;
pub const SECONDARY: u32 = 0x8f8f8f;
pub const ACCENT: u32 = 0x346bf1;
pub const ACCENT_HOVER: u32 = 0x3162db;
pub const DANGER: u32 = 0xdc2626;
pub const DANGER_HOVER: u32 = 0xb91c1c;
pub const GREEN: u32 = 0x34d399;
pub const YELLOW: u32 = 0xfbbf24;
pub const RED: u32 = 0xf87171;
pub const BLUE: u32 = 0x60a5fa;
pub const IDLE: u32 = 0x4b4b4b;
pub const TOOLTIP: u32 = 0x161616;
/// Behind dialogs: black at 70%.
pub const SCRIM: u32 = 0x000000b3;

pub const FONT: &str = "Segoe UI Variable Text";
pub const MONO: &str = "Cascadia Mono";
pub const TEXT_SM: Pixels = px(12.);
pub const TEXT_BASE: Pixels = px(13.);
pub const TEXT_LG: Pixels = px(14.);

/// Dark mode, then the component colors (inputs, tooltips, title bar, scrollbars) from the tokens.
pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    theme.font_family = FONT.into();
    theme.font_size = TEXT_BASE;
    theme.mono_font_family = MONO.into();
    theme.mono_font_size = TEXT_SM;
    theme.radius = px(6.);
    theme.radius_lg = px(12.);
    theme.shadow = false;
    let hsla = |color: u32| -> Hsla { rgb(color).into() };
    let c = &mut theme.colors;
    c.background = hsla(BG);
    c.foreground = hsla(TEXT);
    c.border = hsla(BORDER);
    c.input = hsla(INPUT);
    c.ring = hsla(ACCENT);
    c.caret = hsla(TEXT);
    c.selection = rgba(0x346bf166).into();
    c.primary = hsla(ACCENT);
    c.primary_hover = hsla(ACCENT_HOVER);
    c.primary_foreground = hsla(TEXT);
    c.muted = hsla(CONTROL);
    c.muted_foreground = hsla(MUTED);
    c.accent = hsla(CONTROL_HOVER);
    c.accent_foreground = hsla(TEXT);
    c.popover = hsla(TOOLTIP);
    c.popover_foreground = hsla(TEXT);
    c.title_bar = hsla(BG);
    c.title_bar_border = hsla(BG);
    c.window_border = hsla(BORDER);
    c.secondary = hsla(CONTROL);
    c.secondary_hover = hsla(CONTROL_HOVER);
    c.secondary_active = hsla(INPUT);
    c.secondary_foreground = hsla(TEXT);
    c.danger = hsla(DANGER);
    c.danger_hover = hsla(DANGER_HOVER);
    c.danger_active = hsla(DANGER_HOVER);
    c.danger_foreground = hsla(TEXT);
    c.scrollbar = Hsla::transparent_black();
    c.scrollbar_thumb = hsla(INPUT);
    c.scrollbar_thumb_hover = hsla(0x3a3a3a);
    c.overlay = rgba(SCRIM).into();
}
