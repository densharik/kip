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

/// Characters that only exist in a color-emoji font, which egui cannot
/// rasterize (no COLR/sbix support in epaint) - they would draw as tofu boxes.
/// Claude Code prints ⏺ in front of every message, so this is most of what the
/// terminal shows. Swap in the closest outline glyph the fonts do have.
fn substitute(c: char) -> char {
    match c {
        '⏺' => '●',
        '⏹' => '■',
        '⏵' => '▶',
        _ => c,
    }
}

pub fn show(ui: &mut Ui, session: &mut Session, settings: &Settings, accept_input: bool) -> GridInfo {
    let rect = ui.available_rect_before_wrap();
    let response = ui.interact(rect, ui.id().with(("term", session.id)), Sense::click_and_drag());

    let font_id = FontId::monospace(settings.font_size);
    // Snap the cell and the grid origin to whole physical pixels. A cell of, say,
    // 7.83px puts every column on a different subpixel offset, so egui renders
    // four differently-blurred variants of the same glyph and the column pitch
    // wobbles by half a pixel - which reads as "the font looks off" no matter
    // which font is picked. On a whole-pixel grid every column rasterizes the
    // same way. Costs at most half a pixel of cell width.
    let ppp = ui.ctx().pixels_per_point();
    let snap = |v: f32| (v * ppp).round() / ppp;
    let (cell_w, cell_h) = ui
        .ctx()
        .fonts_mut(|f| (snap(f.glyph_width(&font_id, '0')), snap(f.row_height(&font_id))));
    let cols = ((rect.width() - 8.0) / cell_w).floor().max(4.0) as u16;
    let rows = ((rect.height() - 4.0) / cell_h).floor().max(2.0) as u16;
    let origin = Pos2::new(snap(rect.min.x + 4.0), snap(rect.min.y + 2.0));

    let mut info =
        GridInfo { cols, rows, cell_w, cell_h, had_input: false, interacted: false, grown: false };

    let fg_is_claude = session.fg_is_claude;
    let session_id = session.id;
    let session_cwd = session.cwd.clone();
    let crate::session::Phase::Live(live) = &mut session.phase else {
        return info;
    };
    let shell_pid = live.shell_pid;

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

    // A link popup swallows Escape (it closes the menu, see link_menu) instead of
    // letting it reach claude and cancel whatever it is doing.
    let menu_open = ui
        .ctx()
        .data(|d| d.get_temp::<(std::path::PathBuf, Pos2)>(egui::Id::new(("kip_link_menu", session_id))))
        .is_some();

    // Keyboard input.
    if focused {
        let events = ui.input(|i| i.events.clone());
        let mut seen_keys: Vec<Key> = Vec::new();
        for event in events {
            match event {
                Event::Text(t) => {
                    out.extend_from_slice(t.as_bytes());
                },
                Event::Key { key, physical_key, pressed: true, repeat, modifiers, .. } => {
                    // Fall back to the physical key so Ctrl+C etc. work in non-latin layouts.
                    let key = if key_letter(key).is_none() {
                        physical_key.unwrap_or(key)
                    } else {
                        key
                    };
                    if menu_open && key == Key::Escape {
                        continue;
                    }
                    // A slow launch frame batches a held key's OS auto-repeats into one
                    // frame; forwarding the whole burst walks the app's history in a
                    // single shot. Cap auto-repeats to one press per key per frame (the
                    // command editor is already capped this way via consume_plain).
                    let dup = seen_keys.contains(&key);
                    if !dup {
                        seen_keys.push(key);
                    }
                    if repeat && dup {
                        continue;
                    }
                    if let Some(bytes) = encode_key(key, modifiers, mode, fg_is_claude) {
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

    // Mouse wheel. Order matters. If the app enabled mouse reporting, send wheel
    // events so it scrolls its own view - this is what Warp does and what Claude
    // Code expects. Sending arrow keys to a mouse-reporting app made the wheel
    // walk Claude's prompt history instead of scrolling. Only fall back to arrow
    // keys for alt-screen apps that use alternate-scroll without mouse reporting
    // (less, vim); otherwise scroll our own scrollback.
    if response.hovered() {
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        if dy != 0.0 {
            info.interacted = true;
            session.scroll_accum += dy;
            let lines = (session.scroll_accum / cell_h).trunc() as i32;
            if lines != 0 {
                session.scroll_accum -= lines as f32 * cell_h;
                if mode.intersects(TermMode::MOUSE_MODE) {
                    let (col, row) =
                        wheel_cell(response.hover_pos(), origin, cell_w, cell_h, cols, rows);
                    let sgr = mode.contains(TermMode::SGR_MOUSE);
                    for _ in 0..lines.abs() {
                        out.extend_from_slice(&wheel_report(lines > 0, col, row, sgr));
                    }
                } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
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

    // A filesystem path under the pointer becomes a clickable link (Warp-style):
    // hover underlines it, a plain click opens an Open / Reveal menu. Skip while
    // drag-selecting so a selection stroke over a path shows no stray underline.
    let mut hovered_link: Option<(Rect, std::path::PathBuf)> = None;
    if let Some(pos) = response.hover_pos().filter(|_| !response.dragged()) {
        let rel = pos - origin;
        let col = (rel.x / cell_w) as i32;
        let row = (rel.y / cell_h) as i32;
        if rel.x >= 0.0 && rel.y >= 0.0 && col < cols as i32 && row < rows as i32 {
            let line = Line(row - display_offset as i32);
            let grid = term.grid();
            let chars: Vec<char> = (0..cols as usize).map(|c| grid[line][Column(c)].c).collect();
            if let Some((span, token)) = link_span(&chars, col as usize) {
                // Hit the filesystem only when the token changed since the last
                // frame; a parked pointer (or a redraw storm) must not stat every
                // frame, which would also freeze the UI over a hung network mount.
                let probe_id = egui::Id::new(("kip_link_probe", session_id));
                let cached =
                    ui.ctx().data(|d| d.get_temp::<(String, Option<std::path::PathBuf>)>(probe_id));
                let resolved = match &cached {
                    Some((t, r)) if *t == token => r.clone(),
                    _ => {
                        let r = probe_path(&token, shell_pid, &session_cwd);
                        ui.ctx().data_mut(|d| d.insert_temp(probe_id, (token.clone(), r.clone())));
                        r
                    },
                };
                if let Some(p) = resolved {
                    let x0 = origin.x + span.start as f32 * cell_w;
                    let x1 = origin.x + span.end as f32 * cell_w;
                    let y = origin.y + row as f32 * cell_h;
                    hovered_link = Some((
                        Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, y + cell_h)),
                        p,
                    ));
                }
            }
        }
    }

    let mut clicked_link: Option<(std::path::PathBuf, Pos2)> = None;
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
            if let Some((_, path)) = &hovered_link {
                clicked_link = Some((path.clone(), pos));
            } else {
                term.selection = None;
            }
        }
    }
    if hovered_link.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
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
            cursor_cell = Some((substitute(cell.c), bg.unwrap_or(palette::term_bg())));
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
            c: if has_text { substitute(cell.c) } else { ' ' },
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
    painter.rect_filled(rect, 0.0, palette::term_bg());

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

    // Underline the path under the pointer so it reads as clickable.
    if let Some((lrect, _)) = &hovered_link {
        let uy = lrect.bottom() - 1.0;
        painter.line_segment(
            [Pos2::new(lrect.left(), uy), Pos2::new(lrect.right(), uy)],
            Stroke::new(1.0, palette::group_accent()),
        );
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
                    painter.rect_filled(cell_rect, 0.0, palette::cursor());
                    if let Some((c, _)) = cursor_cell {
                        if c != ' ' {
                            painter.text(
                                Pos2::new(x, y),
                                Align2::LEFT_TOP,
                                c,
                                font_id.clone(),
                                palette::term_bg(),
                            );
                        }
                    }
                },
                CursorShape::Beam => {
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x, y), Vec2::new(2.0, cell_h)),
                        0.0,
                        palette::cursor(),
                    );
                },
                CursorShape::Underline => {
                    painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x, y + cell_h - 2.0), Vec2::new(cell_w, 2.0)),
                        0.0,
                        palette::cursor(),
                    );
                },
                CursorShape::HollowBlock => {
                    painter.rect_stroke(
                        cell_rect,
                        0.0,
                        Stroke::new(1.0, palette::cursor()),
                        StrokeKind::Inside,
                    );
                },
                CursorShape::Hidden => {},
            }
        }
    }

    // Scrollback position badge: how many lines up we are. A drawn up-triangle
    // instead of an arrow glyph - not every font ships one and it showed as a
    // tofu box. Keep UI text ASCII-only for the same reason.
    if display_offset > 0 {
        // Contrast against the terminal bg, which is light for light themes.
        let (ink, edge) = if palette::light() {
            (Color32::from_gray(110), Color32::from_gray(150))
        } else {
            (Color32::from_gray(160), Color32::from_gray(70))
        };
        let label = format!("{display_offset}");
        let gr = painter.text(
            Pos2::new(rect.right() - 12.0, rect.top() + 46.0),
            Align2::RIGHT_TOP,
            &label,
            FontId::proportional(11.0),
            ink,
        );
        let cy = gr.center().y;
        let tx = gr.left() - 7.0;
        painter.add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(tx, cy - 3.5),
                Pos2::new(tx - 4.0, cy + 3.0),
                Pos2::new(tx + 4.0, cy + 3.0),
            ],
            ink,
            Stroke::NONE,
        ));
        let mut box_ = gr;
        box_.min.x -= 13.0;
        painter.rect_stroke(
            box_.expand(4.0),
            CornerRadius::same(4),
            Stroke::new(1.0, edge),
            StrokeKind::Outside,
        );
    }

    if !out.is_empty() {
        live.notifier.notify(out);
    }

    link_menu(ui, session_id, clicked_link);

    info
}

/// The Open / Reveal popup for a clicked path link. State lives in egui memory
/// (keyed by session) so it survives across frames without borrowing `session`.
fn link_menu(ui: &mut Ui, session_id: u64, clicked: Option<(std::path::PathBuf, Pos2)>) {
    use crate::i18n::tr;
    let menu_id = egui::Id::new(("kip_link_menu", session_id));
    let opened_now = clicked.is_some();
    if let Some((path, pos)) = clicked {
        ui.ctx().data_mut(|d| d.insert_temp(menu_id, (path, pos)));
    }
    let Some((path, pos)) = ui.ctx().data(|d| d.get_temp::<(std::path::PathBuf, Pos2)>(menu_id))
    else {
        return;
    };
    let reveal_label = if cfg!(target_os = "macos") {
        tr("Показать в Finder", "Reveal in Finder")
    } else if cfg!(windows) {
        tr("Показать в проводнике", "Show in Explorer")
    } else {
        tr("Показать в папке", "Show in folder")
    };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut close = false;
    let area = egui::Area::new(menu_id.with("popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .constrain(true)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(160.0);
                ui.label(egui::RichText::new(name).weak().small());
                if ui.button(tr("Открыть", "Open")).clicked() {
                    crate::plat::open_path(&path, false);
                    close = true;
                }
                if ui.button(reveal_label).clicked() {
                    crate::plat::open_path(&path, true);
                    close = true;
                }
            });
        });
    // Dismiss on a press outside the popup (but not the click that just opened
    // it) or on Escape (the keyboard handler withholds that Escape from claude).
    let outside = !opened_now
        && ui.input(|i| i.pointer.any_pressed())
        && ui
            .ctx()
            .input(|i| i.pointer.interact_pos())
            .is_none_or(|p| !area.response.rect.contains(p));
    let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if close || outside || escape {
        ui.ctx().data_mut(|d| d.remove::<(std::path::PathBuf, Pos2)>(menu_id));
    }
}

/// Characters allowed inside a path token. Stops at whitespace and the
/// shell/markup delimiters that never sit mid-path in agent output.
fn is_link_char(c: char) -> bool {
    !c.is_whitespace()
        && !matches!(
            c,
            '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | '|' | '*' | '?'
        )
}

/// The maximal path token covering column `col`, with a trailing `:line[:col]`
/// reference and sentence punctuation stripped. Returns the path's column span
/// and the token string, or `None` when `col` does not sit on a path.
fn link_span(chars: &[char], col: usize) -> Option<(std::ops::Range<usize>, String)> {
    if col >= chars.len() || !is_link_char(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_link_char(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && is_link_char(chars[end]) {
        end += 1;
    }
    // Drop trailing sentence punctuation ("edited foo.rs." -> "foo.rs").
    while end > start && matches!(chars[end - 1], '.' | ',' | ';') {
        end -= 1;
    }
    // Drop a trailing :line[:col] reference ("main.rs:42:10" -> "main.rs").
    for _ in 0..2 {
        let mut d = end;
        while d > start && chars[d - 1].is_ascii_digit() {
            d -= 1;
        }
        if d < end && d > start && chars[d - 1] == ':' {
            end = d - 1;
        } else {
            break;
        }
    }
    if col >= end {
        // Pointer is over the stripped ref/punctuation, not the path itself.
        return None;
    }
    Some((start..end, chars[start..end].iter().collect()))
}

/// Resolve `token` against the shell's cwd and confirm it exists on disk. The
/// path-shape prefilter (slash, dot, or `~`) keeps plain prose words - the bulk
/// of what the pointer rests on - off the filesystem entirely.
fn probe_path(
    token: &str,
    shell_pid: i32,
    fallback_cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if !(token.contains('/') || token.contains('.') || token.starts_with('~')) {
        return None;
    }
    let cwd = crate::plat::pid_cwd(shell_pid).unwrap_or_else(|| fallback_cwd.to_path_buf());
    let p = resolve_link(token, &cwd)?;
    p.exists().then_some(p)
}

/// Resolve a path token to an absolute path: `~` expands to home, relative
/// tokens resolve against `cwd`. No filesystem access.
fn resolve_link(token: &str, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    if token == "~" {
        return dirs::home_dir();
    }
    if let Some(rest) = token.strip_prefix("~/") {
        return dirs::home_dir().map(|h| h.join(rest));
    }
    let p = std::path::Path::new(token);
    Some(if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) })
}

/// Cell (1-based col, row) under the pointer, for mouse wheel reports.
fn wheel_cell(
    pos: Option<Pos2>,
    origin: Pos2,
    cell_w: f32,
    cell_h: f32,
    cols: u16,
    rows: u16,
) -> (usize, usize) {
    match pos {
        Some(p) => {
            let c = (((p.x - origin.x) / cell_w) as i32).clamp(0, cols as i32 - 1) + 1;
            let r = (((p.y - origin.y) / cell_h) as i32).clamp(0, rows as i32 - 1) + 1;
            (c as usize, r as usize)
        },
        None => (1, 1),
    }
}

/// Encode a mouse wheel event: button 64 = up, 65 = down. SGR (1006) form when
/// the app enabled it, otherwise the legacy X10 form.
fn wheel_report(up: bool, col: usize, row: usize, sgr: bool) -> Vec<u8> {
    let cb = if up { 64 } else { 65 };
    if sgr {
        format!("\x1b[<{cb};{col};{row}M").into_bytes()
    } else {
        let cx = (col.min(223) + 32) as u8;
        let cy = (row.min(223) + 32) as u8;
        vec![0x1b, b'[', b'M', (cb + 32) as u8, cx, cy]
    }
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

fn encode_key(key: Key, mods: Modifiers, mode: TermMode, claude: bool) -> Option<Vec<u8>> {
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
            // Shift+Enter must reach claude as ESC+CR (Meta+Enter) so it inserts a
            // newline instead of submitting - claude does not push kitty flags and
            // ignores the CSI-u form, so plain \r is indistinguishable from Enter.
            // This is exactly what claude's own /terminal-setup writes for the
            // Alacritty backend this terminal is built on (chars = ESC CR).
            if kitty && has_mods {
                format!("\x1b[13;{m}u").into_bytes()
            } else if mods.alt || (claude && mods.shift) {
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

#[cfg(test)]
mod tests {
    use super::link_span;

    fn span(s: &str, col: usize) -> Option<(std::ops::Range<usize>, String)> {
        link_span(&s.chars().collect::<Vec<_>>(), col)
    }

    #[test]
    fn relative_path() {
        assert_eq!(span("edit src/main.rs now", 8), Some((5..16, "src/main.rs".into())));
    }

    #[test]
    fn strips_line_ref() {
        assert_eq!(span("at src/main.rs:42:10 x", 5), Some((3..14, "src/main.rs".into())));
    }

    #[test]
    fn strips_trailing_period() {
        assert_eq!(span("see foo.rs.", 5), Some((4..10, "foo.rs".into())));
    }

    #[test]
    fn stops_at_backtick() {
        // `src/main.rs` -> the backticks bound the token.
        assert_eq!(span("`src/main.rs`", 3), Some((1..12, "src/main.rs".into())));
    }

    #[test]
    fn none_on_whitespace() {
        assert_eq!(span("a b", 1), None);
    }

    #[test]
    fn none_over_line_number() {
        // Hovering the "42" (past the stripped path) yields no link.
        assert_eq!(span("main.rs:42", 8), None);
    }
}
