use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use egui::Color32;

pub const TERM_BG: Color32 = Color32::from_rgb(0x14, 0x14, 0x14);
pub const TERM_FG: Color32 = Color32::from_rgb(0xc9, 0xc9, 0xc9);
pub const SELECTION_BG: Color32 = Color32::from_rgb(0x3a, 0x45, 0x55);
pub const CURSOR: Color32 = Color32::from_rgb(0xd0, 0xd0, 0xd0);

// Tomorrow Night-ish muted ANSI palette.
const ANSI: [Color32; 16] = [
    Color32::from_rgb(0x2e, 0x2e, 0x2e), // black
    Color32::from_rgb(0xcc, 0x66, 0x66), // red
    Color32::from_rgb(0xb5, 0xbd, 0x68), // green
    Color32::from_rgb(0xf0, 0xc6, 0x74), // yellow
    Color32::from_rgb(0x81, 0xa2, 0xbe), // blue
    Color32::from_rgb(0xb2, 0x94, 0xbb), // magenta
    Color32::from_rgb(0x8a, 0xbe, 0xb7), // cyan
    Color32::from_rgb(0xc5, 0xc8, 0xc6), // white
    Color32::from_rgb(0x66, 0x66, 0x66), // bright black
    Color32::from_rgb(0xd6, 0x82, 0x7d),
    Color32::from_rgb(0xc5, 0xcc, 0x85),
    Color32::from_rgb(0xf4, 0xd2, 0x8c),
    Color32::from_rgb(0x9c, 0xb5, 0xcc),
    Color32::from_rgb(0xc2, 0xa8, 0xc9),
    Color32::from_rgb(0xa1, 0xcc, 0xc6),
    Color32::from_rgb(0xea, 0xea, 0xea), // bright white
];

fn indexed(idx: u8) -> Color32 {
    match idx {
        0..=15 => ANSI[idx as usize],
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

fn named(n: NamedColor, colors: &Colors) -> Color32 {
    if let Some(rgb) = colors[n] {
        return rgb32(rgb);
    }
    match n {
        NamedColor::Foreground | NamedColor::BrightForeground => TERM_FG,
        NamedColor::Background => TERM_BG,
        NamedColor::Cursor => CURSOR,
        NamedColor::DimForeground => dim(TERM_FG),
        n if (n as usize) < 16 => ANSI[n as usize],
        n => {
            // Dim variants: DimBlack=259 ..= DimWhite=266
            let i = n as usize;
            if (259..=266).contains(&i) { dim(ANSI[i - 259]) } else { TERM_FG }
        },
    }
}

fn dim(c: Color32) -> Color32 {
    Color32::from_rgb(
        (c.r() as u32 * 2 / 3) as u8,
        (c.g() as u32 * 2 / 3) as u8,
        (c.b() as u32 * 2 / 3) as u8,
    )
}

pub fn resolve(color: Color, colors: &Colors, bold: bool) -> Color32 {
    match color {
        Color::Spec(rgb) => rgb32(rgb),
        Color::Indexed(idx) => {
            let idx = if bold && idx < 8 { idx + 8 } else { idx };
            if let Some(rgb) = colors[idx as usize] { rgb32(rgb) } else { indexed(idx) }
        },
        Color::Named(n) => {
            let n = if bold { n.to_bright() } else { n };
            named(n, colors)
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
    let bold = flags.contains(Flags::BOLD);
    let mut fg32 = resolve(fg, colors, bold);
    if flags.contains(Flags::DIM) {
        fg32 = dim(fg32);
    }
    let bg_default = bg == Color::Named(NamedColor::Background);
    let mut bg32 = if bg_default { None } else { Some(resolve(bg, colors, false)) };

    if flags.contains(Flags::INVERSE) {
        let old_fg = fg32;
        fg32 = bg32.unwrap_or(TERM_BG);
        bg32 = Some(old_fg);
    }
    if selected {
        bg32 = Some(SELECTION_BG);
    }
    (fg32, bg32)
}

/// Default color for OSC color queries.
pub fn query_color(index: usize, colors: &Colors) -> Rgb {
    if let Some(rgb) = colors[index] {
        return rgb;
    }
    let c = match index {
        256 => TERM_FG,
        257 => TERM_BG,
        258 => CURSOR,
        i if i < 256 => indexed(i as u8),
        _ => TERM_FG,
    };
    Rgb { r: c.r(), g: c.g(), b: c.b() }
}
