use alacritty_terminal::event::Notify;
use alacritty_terminal::event_loop::Msg;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor};
use egui::{
    Align2, Color32, CornerRadius, Event, EventFilter, FontId, Key, Modifiers, PointerButton,
    Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use crate::config::Settings;
use crate::palette;
use crate::session::Session;

pub struct GridInfo {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: f32,
    pub cell_h: f32,
    pub had_input: bool,
    /// User touched the view (scroll, click, selection) without sending bytes.
    pub interacted: bool,
    /// Content has outgrown the viewport (scrollback exists).
    pub grown: bool,
}

pub fn show(ui: &mut Ui, session: &mut Session, settings: &Settings, accept_input: bool) -> GridInfo {
    let rect = ui.available_rect_before_wrap();
    let response = ui.interact(rect, ui.id().with(("term", session.id)), Sense::click_and_drag());

    let font_id = FontId::monospace(settings.font_size);
    let (cell_w, cell_h) =
        ui.ctx().fonts_mut(|f| (f.glyph_width(&font_id, '0'), f.row_height(&font_id)));
    let cols = ((rect.width() - 8.0) / cell_w).floor().max(4.0) as u16;
    let rows = ((rect.height() - 4.0) / cell_h).floor().max(2.0) as u16;
    let origin = rect.min + Vec2::new(4.0, 2.0);

    let mut info =
        GridInfo { cols, rows, cell_w, cell_h, had_input: false, interacted: false, grown: false };

    let crate::session::Phase::Live(live) = &mut session.phase else {
        return info;
    };

    if accept_input {
        response.request_focus();
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                response.id,
                EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
    } else if response.has_focus() {
        response.surrender_focus();
    }
    let focused = response.has_focus();

    let term_arc = live.term.clone();
    let mut term = term_arc.lock();

    // Resize PTY and terminal to fit the widget.
    if cols != live.cols || rows != live.rows {
        term.resize(TermSize::new(cols as usize, rows as usize));
        let _ = live.notifier.0.send(Msg::Resize(alacritty_terminal::event::WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: cell_w.round() as u16,
            cell_height: cell_h.round() as u16,
        }));
        live.cols = cols;
        live.rows = rows;
    }

    // Bottom-anchor content that has not yet outgrown the viewport (Warp-style):
    // empty space stays above, the prompt and fresh output sit near the input.
    let history = term.grid().total_lines() - term.grid().screen_lines();
    info.grown = history > 0;
    let origin = if history == 0 && term.grid().display_offset() == 0 {
        let content = term.renderable_content();
        let mut bottom = content.cursor.point.line.0;
        for c in content.display_iter {
            let occupied =
                c.cell.c != ' ' || c.cell.bg != AnsiColor::Named(NamedColor::Background);
            if occupied && c.point.line.0 > bottom {
                bottom = c.point.line.0;
            }
        }
        let shift = (rows as i32 - 1 - bottom).max(0) as f32 * cell_h;
        Pos2::new(origin.x, origin.y + shift)
    } else {
        origin
    };

    let mode = *term.mode();
    let mut out: Vec<u8> = Vec::new();

    // Keyboard input.
    if focused {
        let events = ui.input(|i| i.events.clone());
        for event in events {
            match event {
                Event::Text(t) => {
                    out.extend_from_slice(t.as_bytes());
                },
                Event::Key { key, physical_key, pressed: true, modifiers, .. } => {
                    // Fall back to the physical key so Ctrl+C etc. work in non-latin layouts.
                    let key = if key_letter(key).is_none() {
                        physical_key.unwrap_or(key)
                    } else {
                        key
                    };
                    if let Some(bytes) = encode_key(key, modifiers, mode) {
                        out.extend_from_slice(&bytes);
                    }
                },
                Event::Paste(s) => {
                    if mode.contains(TermMode::BRACKETED_PASTE) {
                        // Strip a nested paste terminator: classic paste-injection guard.
                        let s = s.replace("\x1b[201~", "");
                        out.extend_from_slice(b"\x1b[200~");
                        out.extend_from_slice(s.as_bytes());
                        out.extend_from_slice(b"\x1b[201~");
                    } else {
                        out.extend_from_slice(s.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                    }
                },
                Event::Copy => {
                    if let Some(text) = term.selection_to_string() {
                        if !text.is_empty() {
                            ui.ctx().copy_text(text);
                        }
                    }
                },
                _ => {},
            }
        }
    }

    // Mouse wheel scrolling.
    if response.hovered() {
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            info.interacted = true;
            session.scroll_accum += dy;
            let lines = (session.scroll_accum / cell_h).trunc() as i32;
            if lines != 0 {
                session.scroll_accum -= lines as f32 * cell_h;
                if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                    let seq: &[u8] = if lines > 0 {
                        if mode.contains(TermMode::APP_CURSOR) { b"\x1bOA" } else { b"\x1b[A" }
                    } else if mode.contains(TermMode::APP_CURSOR) {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    };
                    for _ in 0..lines.abs() {
                        out.extend_from_slice(seq);
                    }
                } else {
                    term.scroll_display(Scroll::Delta(lines));
                }
            }
        }
    }

    // Mouse selection.
    let display_offset = term.grid().display_offset();
    let pos_to_point = |pos: Pos2| -> (Point, Side) {
        let rel = pos - origin;
        let col = ((rel.x / cell_w) as usize).min(cols as usize - 1);
        let row = ((rel.y / cell_h) as i32).clamp(0, rows as i32 - 1);
        let side = if rel.x / cell_w % 1.0 > 0.5 { Side::Right } else { Side::Left };
        (Point::new(Line(row - display_offset as i32), Column(col)), side)
    };

    if let Some(pos) = response.interact_pointer_pos() {
        info.interacted = true;
        let (point, side) = pos_to_point(pos);
        if response.triple_clicked() {
            term.selection = Some(Selection::new(SelectionType::Lines, point, side));
        } else if response.double_clicked() {
            term.selection = Some(Selection::new(SelectionType::Semantic, point, side));
        } else if response.drag_started_by(PointerButton::Primary) {
            // egui only declares a drag after ~6px of travel; by then the pointer
            // has left the pressed cell and the first character would be lost.
            let anchor = ui.input(|i| i.pointer.press_origin()).unwrap_or(pos);
            let (apoint, aside) = pos_to_point(anchor);
            let mut sel = Selection::new(SelectionType::Simple, apoint, aside);
            sel.update(point, side);
            term.selection = Some(sel);
        } else if response.dragged_by(PointerButton::Primary) {
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
        } else if response.clicked() {
            term.selection = None;
        }
    }

    // Copy-on-select: as soon as a selection gesture completes.
    if settings.copy_on_select
        && (response.drag_stopped_by(PointerButton::Primary)
            || response.double_clicked()
            || response.triple_clicked())
    {
        if let Some(text) = term.selection_to_string() {
            if !text.is_empty() {
                ui.ctx().copy_text(text);
            }
        }
    }

    if !out.is_empty() {
        term.scroll_display(Scroll::Bottom);
        session.scroll_accum = 0.0;
        info.had_input = true;
    }

    // ---- Snapshot cells under the lock, paint after releasing it ----
    // Painting shapes ~1-3ms per frame; doing it under the FairMutex would
    // stall the PTY reader thread on every frame during heavy output.
    struct DrawCell {
        x: f32,
        y: f32,
        c: char,
        zw: Option<Vec<char>>,
        fg: Color32,
        bg: Option<Color32>,
        wide: bool,
        underline: bool,
        strike: bool,
    }

    let content = term.renderable_content();
    let display_offset = content.display_offset;
    let sel = content.selection;
    let colors = content.colors;
    let cursor = content.cursor;
    let show_cursor = content.mode.contains(TermMode::SHOW_CURSOR);

    let mut cursor_cell: Option<(char, Color32)> = None;
    let mut cells: Vec<DrawCell> = Vec::with_capacity(512);

    for indexed in content.display_iter {
        let point = indexed.point;
        let vp_line = point.line.0 + display_offset as i32;
        if vp_line < 0 || vp_line >= rows as i32 {
            continue;
        }
        let cell = &indexed.cell;
        let flags = cell.flags;
        let x = origin.x + point.column.0 as f32 * cell_w;
        let y = origin.y + vp_line as f32 * cell_h;

        let selected = sel.is_some_and(|r| selection_contains(&r, point));
        let (fg, bg) = palette::cell_colors(cell.fg, cell.bg, flags, colors, selected);

        if point == cursor.point {
            cursor_cell = Some((cell.c, bg.unwrap_or(palette::TERM_BG)));
        }

        let spacer = flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            || flags.contains(Flags::HIDDEN);
        let has_text = !spacer && cell.c != ' ';
        let underline = !spacer && flags.intersects(Flags::ALL_UNDERLINES);
        let strike = !spacer && flags.contains(Flags::STRIKEOUT);
        if bg.is_none() && !has_text && !underline && !strike {
            continue;
        }
        cells.push(DrawCell {
            x,
            y,
            c: if has_text { cell.c } else { ' ' },
            zw: if has_text { cell.zerowidth().map(|z| z.to_vec()) } else { None },
            fg,
            bg,
            wide: flags.contains(Flags::WIDE_CHAR),
            underline,
            strike,
        });
    }

    drop(term);

    // ---- Render (lock released) ----
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, palette::TERM_BG);

    let mut buf = String::with_capacity(4);
    for dc in &cells {
        if let Some(bg) = dc.bg {
            let w = if dc.wide { cell_w * 2.0 } else { cell_w + 0.5 };
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(dc.x, dc.y), Vec2::new(w, cell_h + 0.5)),
                0.0,
                bg,
            );
        }
        if dc.c != ' ' {
            buf.clear();
            buf.push(dc.c);
            if let Some(zw) = &dc.zw {
                buf.extend(zw.iter());
            }
            painter.text(Pos2::new(dc.x, dc.y), Align2::LEFT_TOP, &buf, font_id.clone(), dc.fg);
        }
        if dc.underline {
            let uy = dc.y + cell_h - 1.5;
            painter.line_segment(
                [Pos2::new(dc.x, uy), Pos2::new(dc.x + cell_w, uy)],
                Stroke::new(1.0, dc.fg),
            );
        }
        if dc.strike {
            let sy = dc.y + cell_h * 0.55;
            painter.line_segment(
                [Pos2::new(dc.x, sy), Pos2::new(dc.x + cell_w, sy)],
                Stroke::new(1.0, dc.fg),
            );
        }
    }

    // Cursor.
    if show_cursor {
        let vp_line = cursor.point.line.0 + display_offset as i32;
        if (0..rows as i32).contains(&vp_line) {
            let x = origin.x + cursor.point.column.0 as f32 * cell_w;
            let y = origin.y + vp_line as f32 * cell_h;
            let cell_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));
            let shape = if focused { cursor.shape } else { CursorShape::HollowBlock };
            match shape {
                CursorShape::Block => {
                    painter.rect_filled(cell_rect, 0.0, palette::CURSOR);
                    if let Some((c, _)) = cursor_cell {
                        if c != ' ' {
                            painter.text(
                                Pos2::new(x, y),
                                Align2::LEFT_TOP,
                                c,
                                font_id.clone(),
                                palette::TERM_BG,
                            );
                        }
                    }
                },
                CursorShape::Beam => {
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x, y), Vec2::new(2.0, cell_h)),
                        0.0,
                        palette::CURSOR,
                    );
                },
                CursorShape::Underline => {
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x, y + cell_h - 2.0), Vec2::new(cell_w, 2.0)),
                        0.0,
                        palette::CURSOR,
                    );
                },
                CursorShape::HollowBlock => {
                    painter.rect_stroke(
                        cell_rect,
                        0.0,
                        Stroke::new(1.0, palette::CURSOR),
                        StrokeKind::Inside,
                    );
                },
                CursorShape::Hidden => {},
            }
        }
    }

    // Scrollback position badge.
    if display_offset > 0 {
        let text = format!("{display_offset}");
        let badge_pos = Pos2::new(rect.right() - 12.0, rect.top() + 46.0);
        let galley_rect = painter.text(
            badge_pos,
            Align2::RIGHT_TOP,
            format!("↑ {text}"),
            FontId::proportional(11.0),
            Color32::from_gray(160),
        );
        painter.rect_stroke(
            galley_rect.expand(4.0),
            CornerRadius::same(4),
            Stroke::new(1.0, Color32::from_gray(70)),
            StrokeKind::Outside,
        );
    }

    if !out.is_empty() {
        live.notifier.notify(out);
    }

    info
}

fn selection_contains(range: &SelectionRange, point: Point) -> bool {
    if point.line < range.start.line || point.line > range.end.line {
        return false;
    }
    if range.is_block {
        return point.column >= range.start.column && point.column <= range.end.column;
    }
    if point.line == range.start.line && point.column < range.start.column {
        return false;
    }
    if point.line == range.end.line && point.column > range.end.column {
        return false;
    }
    true
}

fn key_letter(key: Key) -> Option<u8> {
    Some(match key {
        Key::A => b'a',
        Key::B => b'b',
        Key::C => b'c',
        Key::D => b'd',
        Key::E => b'e',
        Key::F => b'f',
        Key::G => b'g',
        Key::H => b'h',
        Key::I => b'i',
        Key::J => b'j',
        Key::K => b'k',
        Key::L => b'l',
        Key::M => b'm',
        Key::N => b'n',
        Key::O => b'o',
        Key::P => b'p',
        Key::Q => b'q',
        Key::R => b'r',
        Key::S => b's',
        Key::T => b't',
        Key::U => b'u',
        Key::V => b'v',
        Key::W => b'w',
        Key::X => b'x',
        Key::Y => b'y',
        Key::Z => b'z',
        _ => return None,
    })
}

fn encode_key(key: Key, mods: Modifiers, mode: TermMode) -> Option<Vec<u8>> {
    if mods.command {
        return None; // App-level shortcuts.
    }
    let kitty = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let mut m = 1u8;
    if mods.shift {
        m += 1;
    }
    if mods.alt {
        m += 2;
    }
    if mods.ctrl {
        m += 4;
    }
    let has_mods = m > 1;

    let arrow = |c: char| -> Vec<u8> {
        if has_mods {
            format!("\x1b[1;{m}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if has_mods {
            format!("\x1b[{n};{m}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };

    let seq = match key {
        Key::Enter => {
            if kitty && has_mods {
                format!("\x1b[13;{m}u").into_bytes()
            } else if mods.alt {
                b"\x1b\r".to_vec()
            } else {
                b"\r".to_vec()
            }
        },
        Key::Escape => {
            if kitty {
                if has_mods { format!("\x1b[27;{m}u").into_bytes() } else { b"\x1b[27u".to_vec() }
            } else {
                b"\x1b".to_vec()
            }
        },
        Key::Backspace => {
            if kitty && has_mods {
                format!("\x1b[127;{m}u").into_bytes()
            } else if mods.alt {
                b"\x1b\x7f".to_vec()
            } else if mods.ctrl {
                b"\x08".to_vec()
            } else {
                b"\x7f".to_vec()
            }
        },
        Key::Tab => {
            if mods.shift {
                if kitty { b"\x1b[9;2u".to_vec() } else { b"\x1b[Z".to_vec() }
            } else {
                b"\t".to_vec()
            }
        },
        Key::ArrowUp => arrow('A'),
        Key::ArrowDown => arrow('B'),
        Key::ArrowRight => arrow('C'),
        Key::ArrowLeft => arrow('D'),
        Key::Home => arrow('H'),
        Key::End => arrow('F'),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::Delete => tilde(3),
        Key::Insert => tilde(2),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        Key::Space if mods.ctrl => {
            if mods.alt { b"\x1b\x00".to_vec() } else { b"\x00".to_vec() }
        },
        k if mods.ctrl => {
            let byte = match k {
                Key::OpenBracket => 0x1b,
                Key::Backslash => 0x1c,
                Key::CloseBracket => 0x1d,
                Key::Minus | Key::Slash => 0x1f,
                _ => key_letter(k)? & 0x1f,
            };
            if mods.alt { vec![0x1b, byte] } else { vec![byte] }
        },
        _ => return None,
    };
    Some(seq)
}
