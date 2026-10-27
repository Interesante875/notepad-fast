//! Low-contrast, eye-friendly colour palettes.

use egui::Color32;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeKind {
    /// Soft blue-grey dark theme.
    #[default]
    Nocturne,
    /// Warm off-white light theme.
    Paper,
    /// Low-blue-light sepia theme for long reading sessions.
    Sepia,
}

impl ThemeKind {
    pub const ALL: [ThemeKind; 3] = [ThemeKind::Nocturne, ThemeKind::Paper, ThemeKind::Sepia];

    pub fn name(self) -> &'static str {
        match self {
            ThemeKind::Nocturne => "Nocturne (dark)",
            ThemeKind::Paper => "Paper (light)",
            ThemeKind::Sepia => "Sepia (warm)",
        }
    }

    pub fn palette(self) -> Palette {
        match self {
            ThemeKind::Nocturne => NOCTURNE,
            ThemeKind::Paper => PAPER,
            ThemeKind::Sepia => SEPIA,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub bg: Color32,
    pub panel: Color32,
    pub gutter_bg: Color32,
    pub gutter_fg: Color32,
    pub gutter_fg_active: Color32,
    pub text: Color32,
    pub dim: Color32,
    pub current_line: Color32,
    pub selection: Color32,
    pub cursor: Color32,
    pub find_match: Color32,
    pub accent: Color32,
    pub scrollbar_thumb: Color32,
    // syntax
    pub key: Color32,
    pub string: Color32,
    pub number: Color32,
    pub keyword: Color32,
    pub punct: Color32,
    pub heading: Color32,
    pub emphasis: Color32,
    pub code: Color32,
    pub link: Color32,
    pub quote: Color32,
    pub error: Color32,
    pub warn: Color32,
    pub info: Color32,
    pub debug: Color32,
    pub timestamp: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const NOCTURNE: Palette = Palette {
    dark: true,
    bg: hex(0x1F2329),
    panel: hex(0x191C21),
    gutter_bg: hex(0x1B1F24),
    gutter_fg: hex(0x4B5363),
    gutter_fg_active: hex(0xA7B0C0),
    text: hex(0xC8CED8),
    dim: hex(0x6B7385),
    current_line: hex(0x252A31),
    selection: hex(0x2F4560),
    cursor: hex(0x7FB4CA),
    find_match: hex(0x4A4632),
    accent: hex(0x7FB4CA),
    scrollbar_thumb: hex(0x3A414D),
    key: hex(0x8FBCBB),
    string: hex(0xA3BE8C),
    number: hex(0xD08770),
    keyword: hex(0xB48EAD),
    punct: hex(0x6B7385),
    heading: hex(0x88C0D0),
    emphasis: hex(0xEBCB8B),
    code: hex(0xA3BE8C),
    link: hex(0x81A1C1),
    quote: hex(0x7B8494),
    error: hex(0xE06C75),
    warn: hex(0xE5C07B),
    info: hex(0x7FB4CA),
    debug: hex(0x6B7385),
    timestamp: hex(0x6F8FA6),
};

pub const PAPER: Palette = Palette {
    dark: false,
    bg: hex(0xFAF8F3),
    panel: hex(0xF1EEE6),
    gutter_bg: hex(0xF4F1EA),
    gutter_fg: hex(0xBDB6A6),
    gutter_fg_active: hex(0x6B6457),
    text: hex(0x383A3F),
    dim: hex(0x9A9486),
    current_line: hex(0xF2EEE4),
    selection: hex(0xD3E2F2),
    cursor: hex(0x2B6CB0),
    find_match: hex(0xF8E5A6),
    accent: hex(0x2B6CB0),
    scrollbar_thumb: hex(0xD8D2C4),
    key: hex(0x2E7D7D),
    string: hex(0x4F7F3A),
    number: hex(0xB0571E),
    keyword: hex(0x8A4FA0),
    punct: hex(0x8C8677),
    heading: hex(0x2B6CB0),
    emphasis: hex(0xA0661A),
    code: hex(0x4F7F3A),
    link: hex(0x2B6CB0),
    quote: hex(0x7D7768),
    error: hex(0xC0392B),
    warn: hex(0xB7791F),
    info: hex(0x2B6CB0),
    debug: hex(0x9A9486),
    timestamp: hex(0x6E7F8D),
};

pub const SEPIA: Palette = Palette {
    dark: false,
    bg: hex(0xF4ECD8),
    panel: hex(0xEADFC4),
    gutter_bg: hex(0xEEE4CC),
    gutter_fg: hex(0xB9A985),
    gutter_fg_active: hex(0x6E5A40),
    text: hex(0x5B4636),
    dim: hex(0x9C8A6E),
    current_line: hex(0xEDE3C9),
    selection: hex(0xE2CFA0),
    cursor: hex(0x9A5B2E),
    find_match: hex(0xF0D58A),
    accent: hex(0x9A5B2E),
    scrollbar_thumb: hex(0xD6C7A3),
    key: hex(0x6A7F3C),
    string: hex(0x7A6A2E),
    number: hex(0xA4502A),
    keyword: hex(0x8C4A6E),
    punct: hex(0xA39272),
    heading: hex(0x9A5B2E),
    emphasis: hex(0xA4502A),
    code: hex(0x6A7F3C),
    link: hex(0x5A6E8C),
    quote: hex(0x8C7B60),
    error: hex(0xB3392F),
    warn: hex(0xA66A12),
    info: hex(0x5A6E8C),
    debug: hex(0x9C8A6E),
    timestamp: hex(0x7D8C8A),
};

/// Apply the palette to egui's widget visuals (menus, dialogs, panels).
pub fn apply(ctx: &egui::Context, p: &Palette) {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.panel;
    v.window_fill = p.panel;
    v.faint_bg_color = p.current_line;
    v.extreme_bg_color = p.bg;
    v.text_edit_bg_color = Some(p.bg);
    v.code_bg_color = p.current_line;
    v.hyperlink_color = p.link;
    v.override_text_color = Some(p.text);
    v.selection.bg_fill = p.selection;
    v.selection.stroke.color = p.text;
    v.window_stroke.color = p.scrollbar_thumb;
    v.widgets.noninteractive.bg_stroke.color = p.scrollbar_thumb;
    ctx.set_visuals(v);
}
