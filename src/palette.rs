use std::sync::RwLock;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

const fn c(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

#[derive(Clone)]
pub struct Theme {
    pub bg: Color32,
    pub fg: Color32,
    pub cursor: Color32,
    pub selection: Color32,
    pub ansi: [Color32; 16],
}

pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub theme: Theme,
}

// Base theme = the original muted Tomorrow Night-ish palette (unchanged look).
const TOMORROW: Theme = Theme {
    bg: Color32::from_rgb(0x14, 0x14, 0x14),
    fg: Color32::from_rgb(0xc9, 0xc9, 0xc9),
    cursor: Color32::from_rgb(0xd0, 0xd0, 0xd0),
    selection: Color32::from_rgb(0x3a, 0x45, 0x55),
    ansi: [
        Color32::from_rgb(0x2e, 0x2e, 0x2e),
        Color32::from_rgb(0xcc, 0x66, 0x66),
        Color32::from_rgb(0xb5, 0xbd, 0x68),
        Color32::from_rgb(0xf0, 0xc6, 0x74),
        Color32::from_rgb(0x81, 0xa2, 0xbe),
        Color32::from_rgb(0xb2, 0x94, 0xbb),
        Color32::from_rgb(0x8a, 0xbe, 0xb7),
        Color32::from_rgb(0xc5, 0xc8, 0xc6),
        Color32::from_rgb(0x66, 0x66, 0x66),
        Color32::from_rgb(0xd6, 0x82, 0x7d),
        Color32::from_rgb(0xc5, 0xcc, 0x85),
        Color32::from_rgb(0xf4, 0xd2, 0x8c),
        Color32::from_rgb(0x9c, 0xb5, 0xcc),
        Color32::from_rgb(0xc2, 0xa8, 0xc9),
        Color32::from_rgb(0xa1, 0xcc, 0xc6),
        Color32::from_rgb(0xea, 0xea, 0xea),
    ],
};

pub const PRESETS: [Preset; 6] = [
    Preset { key: "tomorrow", label: "Tomorrow Night", theme: TOMORROW },
    Preset {
        key: "gruvbox",
        label: "Gruvbox Dark",
        theme: Theme {
            bg: c(0x28, 0x28, 0x28),
            fg: c(0xeb, 0xdb, 0xb2),
            cursor: c(0xeb, 0xdb, 0xb2),
            selection: c(0x50, 0x49, 0x45),
            ansi: [
                c(0x28, 0x28, 0x28), c(0xcc, 0x24, 0x1d), c(0x98, 0x97, 0x1a), c(0xd7, 0x99, 0x21),
                c(0x45, 0x85, 0x88), c(0xb1, 0x62, 0x86), c(0x68, 0x9d, 0x6a), c(0xa8, 0x99, 0x84),
                c(0x92, 0x83, 0x74), c(0xfb, 0x49, 0x34), c(0xb8, 0xbb, 0x26), c(0xfa, 0xbd, 0x2f),
                c(0x83, 0xa5, 0x98), c(0xd3, 0x86, 0x9b), c(0x8e, 0xc0, 0x7c), c(0xeb, 0xdb, 0xb2),
            ],
        },
    },
    Preset {
        key: "nord",
        label: "Nord",
        theme: Theme {
            bg: c(0x2e, 0x34, 0x40),
            fg: c(0xd8, 0xde, 0xe9),
            cursor: c(0xd8, 0xde, 0xe9),
            selection: c(0x43, 0x4c, 0x5e),
            ansi: [
                c(0x3b, 0x42, 0x52), c(0xbf, 0x61, 0x6a), c(0xa3, 0xbe, 0x8c), c(0xeb, 0xcb, 0x8b),
                c(0x81, 0xa1, 0xc1), c(0xb4, 0x8e, 0xad), c(0x88, 0xc0, 0xd0), c(0xe5, 0xe9, 0xf0),
                c(0x4c, 0x56, 0x6a), c(0xbf, 0x61, 0x6a), c(0xa3, 0xbe, 0x8c), c(0xeb, 0xcb, 0x8b),
                c(0x81, 0xa1, 0xc1), c(0xb4, 0x8e, 0xad), c(0x8f, 0xbc, 0xbb), c(0xec, 0xef, 0xf4),
            ],
        },
    },
    Preset {
        key: "dracula",
        label: "Dracula",
        theme: Theme {
            bg: c(0x28, 0x2a, 0x36),
            fg: c(0xf8, 0xf8, 0xf2),
            cursor: c(0xf8, 0xf8, 0xf2),
            selection: c(0x44, 0x47, 0x5a),
            ansi: [
                c(0x21, 0x22, 0x2c), c(0xff, 0x55, 0x55), c(0x50, 0xfa, 0x7b), c(0xf1, 0xfa, 0x8c),
                c(0xbd, 0x93, 0xf9), c(0xff, 0x79, 0xc6), c(0x8b, 0xe9, 0xfd), c(0xf8, 0xf8, 0xf2),
                c(0x62, 0x72, 0xa4), c(0xff, 0x6e, 0x6e), c(0x69, 0xff, 0x94), c(0xff, 0xff, 0xa5),
                c(0xd6, 0xac, 0xff), c(0xff, 0x92, 0xdf), c(0xa4, 0xff, 0xff), c(0xff, 0xff, 0xff),
            ],
        },
    },
    Preset {
        key: "solarized",
        label: "Solarized Dark",
        theme: Theme {
            bg: c(0x00, 0x2b, 0x36),
            fg: c(0x83, 0x94, 0x96),
            cursor: c(0x93, 0xa1, 0xa1),
            selection: c(0x0c, 0x4a, 0x55),
            ansi: [
                c(0x07, 0x36, 0x42), c(0xdc, 0x32, 0x2f), c(0x85, 0x99, 0x00), c(0xb5, 0x89, 0x00),
                c(0x26, 0x8b, 0xd2), c(0xd3, 0x36, 0x82), c(0x2a, 0xa1, 0x98), c(0xee, 0xe8, 0xd5),
                c(0x00, 0x2b, 0x36), c(0xcb, 0x4b, 0x16), c(0x58, 0x6e, 0x75), c(0x65, 0x7b, 0x83),
                c(0x83, 0x94, 0x96), c(0x6c, 0x71, 0xc4), c(0x93, 0xa1, 0xa1), c(0xfd, 0xf6, 0xe3),
            ],
        },
    },
    Preset {
        key: "onedark",
        label: "One Dark",
        theme: Theme {
            bg: c(0x28, 0x2c, 0x34),
            fg: c(0xab, 0xb2, 0xbf),
            cursor: c(0xab, 0xb2, 0xbf),
            selection: c(0x3e, 0x44, 0x51),
            ansi: [
                c(0x28, 0x2c, 0x34), c(0xe0, 0x6c, 0x75), c(0x98, 0xc3, 0x79), c(0xe5, 0xc0, 0x7b),
                c(0x61, 0xaf, 0xef), c(0xc6, 0x78, 0xdd), c(0x56, 0xb6, 0xc2), c(0xab, 0xb2, 0xbf),
                c(0x54, 0x58, 0x62), c(0xe0, 0x6c, 0x75), c(0x98, 0xc3, 0x79), c(0xe5, 0xc0, 0x7b),
                c(0x61, 0xaf, 0xef), c(0xc6, 0x78, 0xdd), c(0x56, 0xb6, 0xc2), c(0xc8, 0xcc, 0xd4),
            ],
        },
    },
];

static ACTIVE: RwLock<Theme> = RwLock::new(TOMORROW);

/// Set the active theme by preset key (unknown key falls back to the first
/// preset). `accent`, if given, overrides the selection-highlight color.
pub fn apply(key: &str, accent: Option<Color32>) {
    let mut theme = PRESETS
        .iter()
        .find(|p| p.key == key)
        .map(|p| p.theme.clone())
        .unwrap_or_else(|| PRESETS[0].theme.clone());
    if let Some(a) = accent {
        theme.selection = a;
    }
    *ACTIVE.write().unwrap() = theme;
}

pub fn theme() -> Theme {
    ACTIVE.read().unwrap().clone()
}

pub fn term_bg() -> Color32 {
    ACTIVE.read().unwrap().bg
}

pub fn cursor() -> Color32 {
    ACTIVE.read().unwrap().cursor
}

pub fn selection() -> Color32 {
    ACTIVE.read().unwrap().selection
}

fn lighten(col: Color32, d: u8) -> Color32 {
    Color32::from_rgb(
        col.r().saturating_add(d),
        col.g().saturating_add(d),
        col.b().saturating_add(d),
    )
}

/// App chrome derived from the terminal background so the whole window follows
/// the theme. The offsets reproduce the original 0x19 / 0x1c on the 0x14 base.
pub fn chrome_sidebar() -> Color32 {
    lighten(term_bg(), 5)
}

pub fn chrome_bar() -> Color32 {
    lighten(term_bg(), 8)
}

fn indexed(idx: u8, th: &Theme) -> Color32 {
    match idx {
        0..=15 => th.ansi[idx as usize],
        16..=231 => {
            let i = idx as u32 - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            Color32::from_rgb(
                steps[(i / 36) as usize],
                steps[(i / 6 % 6) as usize],
                steps[(i % 6) as usize],
            )
        },
        232..=255 => {
            let g = 8 + 10 * (idx - 232);
            Color32::from_rgb(g, g, g)
        },
    }
}

fn rgb32(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

fn named(n: NamedColor, colors: &Colors, th: &Theme) -> Color32 {
    if let Some(rgb) = colors[n] {
        return rgb32(rgb);
    }
    match n {
        NamedColor::Foreground | NamedColor::BrightForeground => th.fg,
        NamedColor::Background => th.bg,
        NamedColor::Cursor => th.cursor,
        NamedColor::DimForeground => dim(th.fg),
        n if (n as usize) < 16 => th.ansi[n as usize],
        n => {
            // Dim variants: DimBlack=259 ..= DimWhite=266
            let i = n as usize;
            if (259..=266).contains(&i) { dim(th.ansi[i - 259]) } else { th.fg }
        },
    }
}

fn dim(col: Color32) -> Color32 {
    Color32::from_rgb(
        (col.r() as u32 * 2 / 3) as u8,
        (col.g() as u32 * 2 / 3) as u8,
        (col.b() as u32 * 2 / 3) as u8,
    )
}

fn resolve(color: Color, colors: &Colors, bold: bool, th: &Theme) -> Color32 {
    match color {
        Color::Spec(rgb) => rgb32(rgb),
        Color::Indexed(idx) => {
            let idx = if bold && idx < 8 { idx + 8 } else { idx };
            if let Some(rgb) = colors[idx as usize] { rgb32(rgb) } else { indexed(idx, th) }
        },
        Color::Named(n) => {
            let n = if bold { n.to_bright() } else { n };
            named(n, colors, th)
        },
    }
}

/// Resolved (fg, bg) for a cell. `bg` is None when it matches the default background.
pub fn cell_colors(
    fg: Color,
    bg: Color,
    flags: Flags,
    colors: &Colors,
    selected: bool,
) -> (Color32, Option<Color32>) {
    let th = theme();
    let bold = flags.contains(Flags::BOLD);
    let mut fg32 = resolve(fg, colors, bold, &th);
    if flags.contains(Flags::DIM) {
        fg32 = dim(fg32);
    }
    let bg_default = bg == Color::Named(NamedColor::Background);
    let mut bg32 = if bg_default { None } else { Some(resolve(bg, colors, false, &th)) };

    if flags.contains(Flags::INVERSE) {
        let old_fg = fg32;
        fg32 = bg32.unwrap_or(th.bg);
        bg32 = Some(old_fg);
    }
    if selected {
        bg32 = Some(th.selection);
    }
    (fg32, bg32)
}

/// Default color for OSC color queries.
pub fn query_color(index: usize, colors: &Colors) -> Rgb {
    if let Some(rgb) = colors[index] {
        return rgb;
    }
    let th = theme();
    let col = match index {
        256 => th.fg,
        257 => th.bg,
        258 => th.cursor,
        i if i < 256 => indexed(i as u8, &th),
        _ => th.fg,
    };
    Rgb { r: col.r(), g: col.g(), b: col.b() }
}
