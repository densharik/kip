use egui::{
    Align2, Color32, CornerRadius, Event, EventFilter, FontId, Key, Modifiers, PointerButton,
    Pos2, Rect, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use crate::config::Settings;
use crate::palette;
use crate::session::Session;
use crate::vt::{self, CursorShape, Modes, Pos};

const RESIZE_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

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

/// A search hit: first and last cell, inclusive, in buffer positions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Match {
    pub start: Pos,
    pub end: Pos,
}

impl Match {
    fn contains(&self, p: Pos) -> bool {
        self.start <= p && p <= self.end
    }
}

/// A plain drag selection: where it started and on which half of that cell.
#[derive(Clone, Copy)]
pub struct SelDrag {
    anchor: Pos,
    right: bool,
}

/// Every case-insensitive occurrence of `query` in the screen and scrollback,
/// top to bottom. Matches across soft-wrapped lines too. Case folding is
/// Unicode-aware (Cyrillic and all), unlike the terminal core's own search.
pub fn find_all(term: &vt::Term, query: &str) -> Vec<Match> {
    const MAX: usize = 5000;
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut out = Vec::new();
    if q.is_empty() {
        return out;
    }
    // One logical line at a time: lowercased chars and where each came from.
    let (mut hay, mut pos): (Vec<char>, Vec<Pos>) = (Vec::new(), Vec::new());
    let scan = |hay: &[char], pos: &[Pos], out: &mut Vec<Match>| {
        let mut i = 0;
        while i + q.len() <= hay.len() {
            if hay[i] == q[0] && hay[i..i + q.len()] == q[..] {
                out.push(Match { start: pos[i], end: pos[i + q.len() - 1] });
                i += q.len();
            } else {
                i += 1;
            }
        }
    };
    for y in 0..term.total_rows() {
        let Some(row) = term.row_text(y) else { break };
        for (c, &x) in row.chars.iter().zip(&row.cols) {
            for lc in c.to_lowercase() {
                hay.push(lc);
                pos.push(Pos { y, x });
            }
        }
        if !row.wrap {
            scan(&hay, &pos, &mut out);
            hay.clear();
            pos.clear();
            if out.len() >= MAX {
                break;
            }
        }
    }
    scan(&hay, &pos, &mut out);
    out.truncate(MAX);
    out
}

/// Scroll so the match is in view (centered), unless it already is.
pub fn reveal(term: &mut vt::Term, m: &Match) {
    let rows = term.rows() as u32;
    let top = term.viewport_top();
    if m.start.y >= top && m.start.y < top + rows {
        return;
    }
    term.scroll_to(m.start.y.saturating_sub(rows / 2));
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

/// Select from the drag anchor to `head`, each end on the cell half the
/// pointer was over: starting on the right half of a cell leaves that cell out.
fn select_span(term: &mut vt::Term, d: SelDrag, head: Pos, head_right: bool) {
    let cols = term.cols().max(1) as u64;
    let idx = |p: Pos| p.y as u64 * cols + p.x as u64;
    let (a, h) = (idx(d.anchor), idx(head));
    let (start, end) = if h >= a {
        (a + d.right as u64, if head_right { Some(h) } else { h.checked_sub(1) })
    } else {
        (h + head_right as u64, if d.right { Some(a) } else { a.checked_sub(1) })
    };
    match end.filter(|e| *e >= start) {
        Some(end) => {
            let pos = |i: u64| Pos { y: (i / cols) as u32, x: (i % cols) as u16 };
            term.select(pos(start), pos(end));
        },
        None => term.clear_selection(),
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

    // Resize PTY and terminal to fit the widget, but only once the size has held
    // still. Claude redraws its live region by moving the cursor up as many lines
    // as it printed at the old width; a resize per frame of a window drag reflows
    // those lines under it dozens of times and leaves torn, duplicated text.
    // Until then the old grid is drawn as is, clipped to the widget.
    let want = (cols, rows);
    if want == (live.cols, live.rows) {
        live.pending_size = None;
    } else {
        let since = match live.pending_size {
            Some((size, since)) if size == want => since,
            _ => live.pending_size.insert((want, std::time::Instant::now())).1,
        };
        let left = RESIZE_SETTLE.saturating_sub(since.elapsed());
        if left.is_zero() {
            live.resize(cols, rows, cell_w.round() as u16, cell_h.round() as u16);
            live.pending_size = None;
        } else {
            ui.ctx().request_repaint_after(left);
        }
    }
    live.sync_theme();
    let (cols, rows) = (live.cols, live.rows);

    let term_arc = live.term.clone();
    let mut term = term_arc.lock();
    let mut frame = term.frame();

    // Bottom-anchor content that has not yet outgrown the viewport (Warp-style):
    // empty space stays above, the prompt and fresh output sit near the input.
    let history = term.history();
    info.grown = history > 0;
    let origin = if history == 0 && term.display_offset() == 0 {
        let mut bottom = frame.cursor.map_or(0, |(_, y)| y as i32);
        for c in &frame.cells {
            if (c.c != ' ' || c.bg.is_some()) && c.y as i32 > bottom {
                bottom = c.y as i32;
            }
        }
        let shift = (rows as i32 - 1 - bottom).max(0) as f32 * cell_h;
        Pos2::new(origin.x, origin.y + shift)
    } else {
        origin
    };

    let mode = term.modes();
    let mut out: Vec<u8> = Vec::new();
    // Scroll or selection changed this frame: the snapshot above is stale.
    let mut changed = false;
    let alt = mode.alt_screen;

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
                    let modifiers = backspace_accel(&mut session.bs_repeat, key, repeat, modifiers);
                    if let Some(bytes) = encode_key(key, modifiers, mode, fg_is_claude) {
                        out.extend_from_slice(&bytes);
                    }
                },
                Event::Paste(s) => {
                    if mode.bracketed_paste {
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
                    if let Some(text) = term.selection_text()
                        && !text.is_empty() {
                            ui.ctx().copy_text(text);
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
                if mode.mouse {
                    let (col, row) =
                        wheel_cell(response.hover_pos(), origin, cell_w, cell_h, cols, rows);
                    for _ in 0..lines.abs() {
                        out.extend_from_slice(&wheel_report(lines > 0, col, row, mode.sgr_mouse));
                    }
                } else if mode.alt_screen && mode.alt_scroll {
                    let seq: &[u8] = if lines > 0 {
                        if mode.app_cursor { b"\x1bOA" } else { b"\x1b[A" }
                    } else if mode.app_cursor {
                        b"\x1bOB"
                    } else {
                        b"\x1b[B"
                    };
                    for _ in 0..lines.abs() {
                        out.extend_from_slice(seq);
                    }
                } else {
                    term.scroll(lines);
                    changed = true;
                }
            }
        }
    }

    // Dragging a selection past the top or bottom edge scrolls the scrollback,
    // faster the farther the pointer is, and keeps going while it is held still.
    if response.dragged_by(PointerButton::Primary) && session.sel_drag.is_some() && !alt
        && let Some(pos) = response.interact_pointer_pos() {
            let over = if pos.y < rect.top() {
                pos.y - rect.top()
            } else if pos.y > rect.bottom() {
                pos.y - rect.bottom()
            } else {
                0.0
            };
            if over != 0.0 {
                let n = 1 + (over.abs() / cell_h) as i32;
                term.scroll(if over < 0.0 { n } else { -n });
                changed = true;
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(40));
            }
        }

    // Mouse selection.
    let top = term.viewport_top();
    let total = term.total_rows();
    let pos_to_point = |pos: Pos2| -> (Pos, bool) {
        let rel = pos - origin;
        let col = ((rel.x / cell_w).max(0.0) as u16).min(cols - 1);
        let row = ((rel.y / cell_h) as i32).clamp(0, rows as i32 - 1);
        let right = rel.x / cell_w % 1.0 > 0.5;
        (Pos { y: top + row as u32, x: col }, right)
    };

    // A filesystem path under the pointer becomes a clickable link (Warp-style):
    // hover underlines it, a plain click opens an Open / Reveal menu. Skip while
    // drag-selecting so a selection stroke over a path shows no stray underline.
    let mut hovered_link: Option<(Vec<Rect>, std::path::PathBuf)> = None;
    if let Some(pos) = response.hover_pos().filter(|_| !response.dragged()) {
        let rel = pos - origin;
        let col = (rel.x / cell_w) as i32;
        let row = (rel.y / cell_h) as i32;
        if rel.x >= 0.0 && rel.y >= 0.0 && col < cols as i32 && row < rows as i32 {
            let row_chars = |r: i32| -> Option<Vec<char>> {
                let y = u32::try_from(top as i64 + r as i64).ok().filter(|y| *y < total)?;
                let text = term.row_text(y)?;
                let mut chars = vec![' '; cols as usize];
                let mut last = None;
                for (c, &x) in text.chars.iter().zip(&text.cols) {
                    if last != Some(x) && (x as usize) < chars.len() {
                        chars[x as usize] = *c;
                    }
                    last = Some(x);
                }
                Some(chars)
            };
            let chars = row_chars(row).unwrap_or_default();
            if let Some((span, token)) = link_span(&chars, col as usize) {
                // A path broken across lines (claude wraps its own output at the
                // terminal width) is tried whole first, then just this line's part.
                let parts = wrapped_parts(&row_chars, row, span.clone());
                let joined: String = parts
                    .iter()
                    .filter_map(|(r, sp)| Some(row_chars(*r)?[sp.clone()].iter().collect::<String>()))
                    .collect();
                let joined = link_span(&joined.chars().collect::<Vec<_>>(), 0).map(|(_, t)| t);
                // Hit the filesystem only when the token changed since the last
                // frame; a parked pointer (or a redraw storm) must not stat every
                // frame, which would also freeze the UI over a hung network mount.
                let key = joined.clone().unwrap_or_else(|| token.clone());
                let probe_id = egui::Id::new(("kip_link_probe", session_id));
                let cached = ui
                    .ctx()
                    .data(|d| d.get_temp::<(String, Option<(std::path::PathBuf, bool)>)>(probe_id));
                let resolved = match &cached {
                    Some((t, r)) if *t == key => r.clone(),
                    _ => {
                        let whole = joined
                            .filter(|_| parts.len() > 1)
                            .and_then(|j| probe_path(&j, shell_pid, &session_cwd))
                            .map(|p| (p, true));
                        let r = whole.or_else(|| probe_path(&token, shell_pid, &session_cwd).map(|p| (p, false)));
                        ui.ctx().data_mut(|d| d.insert_temp(probe_id, (key, r.clone())));
                        r
                    },
                };
                if let Some((p, whole)) = resolved {
                    let spans = if whole { parts } else { vec![(row, span)] };
                    let rects = spans
                        .into_iter()
                        .map(|(r, sp)| {
                            let y = origin.y + r as f32 * cell_h;
                            Rect::from_min_max(
                                Pos2::new(origin.x + sp.start as f32 * cell_w, y),
                                Pos2::new(origin.x + sp.end as f32 * cell_w, y + cell_h),
                            )
                        })
                        .collect();
                    hovered_link = Some((rects, p));
                }
            }
        }
    }

    let mut clicked_link: Option<(std::path::PathBuf, Pos2)> = None;    if let Some(pos) = response.interact_pointer_pos() {
        info.interacted = true;
        let (point, right) = pos_to_point(pos);
        if response.triple_clicked() || response.double_clicked() {
            session.sel_drag = None;
            match term.unit_at(point, response.triple_clicked()) {
                Some((a, b)) => term.select(a, b),
                None => term.clear_selection(),
            }
            changed = true;
        } else if response.drag_started_by(PointerButton::Primary) {
            // egui only declares a drag after ~6px of travel; by then the pointer
            // has left the pressed cell and the first character would be lost.
            let anchor = ui.input(|i| i.pointer.press_origin()).unwrap_or(pos);
            let (apoint, aright) = pos_to_point(anchor);
            let d = SelDrag { anchor: apoint, right: aright };
            session.sel_drag = Some(d);
            select_span(&mut term, d, point, right);
            changed = true;
        } else if response.dragged_by(PointerButton::Primary) {
            if let Some(d) = session.sel_drag {
                select_span(&mut term, d, point, right);
                changed = true;
            }
        } else if response.clicked() {
            if let Some((_, path)) = &hovered_link {
                clicked_link = Some((path.clone(), pos));
            } else {
                term.clear_selection();
                session.sel_drag = None;
                changed = true;
            }
        }
    }
    if response.drag_stopped_by(PointerButton::Primary) {
        session.sel_drag = None;
    }
    if hovered_link.is_some() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    // Copy-on-select: as soon as a selection gesture completes.
    if settings.copy_on_select
        && (response.drag_stopped_by(PointerButton::Primary)
            || response.double_clicked()
            || response.triple_clicked())
        && let Some(text) = term.selection_text()
            && !text.is_empty() {
                ui.ctx().copy_text(text);
            }

    if !out.is_empty() {
        term.scroll_bottom();
        changed = true;
        session.scroll_accum = 0.0;
        info.had_input = true;
    }
    if changed {
        frame = term.frame();
    }
    let display_offset = term.display_offset();
    let top = term.viewport_top();
    drop(term);

    // ---- Paint from the snapshot; the lock is already released ----
    // Painting shapes ~1-3ms per frame; doing it under the lock would stall
    // the PTY reader thread on every frame during heavy output.
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

    // Search hits that touch the viewport, flagged when it is the current one.
    let (vtop, vbot) = (top, top + rows as u32 - 1);
    let hits: Vec<(&Match, bool)> = session
        .find_hits
        .iter()
        .enumerate()
        .filter(|(_, m)| m.end.y >= vtop && m.start.y <= vbot)
        .map(|(i, m)| (m, session.find_cur == Some(i)))
        .collect();

    let mut cells: Vec<DrawCell> = Vec::with_capacity(frame.cells.len());

    for cell in &mut frame.cells {
        if cell.y >= rows || cell.x >= cols {
            continue;
        }
        let x = origin.x + cell.x as f32 * cell_w;
        let y = origin.y + cell.y as f32 * cell_h;
        let (fg, mut bg) = palette::cell_colors(cell, &frame.palette, frame.fg, frame.bg);
        let point = Pos { y: top + cell.y as u32, x: cell.x };
        if let Some(&(_, cur)) = hits.iter().find(|(m, _)| m.contains(point)) {
            let a = palette::selection();
            bg = Some(if cur { a } else { a.gamma_multiply(0.45) });
        }
        let has_text = !cell.hidden && cell.c != ' ';
        let underline = cell.underline;
        let strike = cell.strike;
        if bg.is_none() && !has_text && !underline && !strike {
            continue;
        }
        cells.push(DrawCell {
            x,
            y,
            c: if has_text { substitute(cell.c) } else { ' ' },
            zw: if has_text { cell.extra.take() } else { None },
            fg,
            bg,
            wide: cell.wide,
            underline,
            strike,
        });
    }

    // ---- Render ----
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, palette::term_bg());

    // The whole grid goes out as one mesh: a Shape::Text per cell cost 2-17ms a
    // frame at 200x60 (layout cache misses on every new color, plus tessellating
    // ~13k shapes); copying cached glyph quads costs ~0.4ms. Each glyph stays at
    // its cell's origin, so fallback fonts and wide chars cannot shift the grid.
    let ppp = ui.ctx().pixels_per_point();
    let mut mesh = egui::Mesh::default();
    let mut i = 0;
    while i < cells.len() {
        let Some(bg) = cells[i].bg else {
            i += 1;
            continue;
        };
        // Merge neighbours with the same background into one rect.
        let (x0, y) = (cells[i].x, cells[i].y);
        let mut x1 = x0 + if cells[i].wide { cell_w * 2.0 } else { cell_w };
        let mut j = i + 1;
        while j < cells.len()
            && cells[j].y == y
            && cells[j].bg == Some(bg)
            && (cells[j].x - x1).abs() < 0.01
        {
            x1 = cells[j].x + if cells[j].wide { cell_w * 2.0 } else { cell_w };
            j += 1;
        }
        let w = if cells[j - 1].wide { x1 - x0 } else { x1 - x0 + 0.5 };
        mesh.add_colored_rect(Rect::from_min_size(Pos2::new(x0, y), Vec2::new(w, cell_h + 0.5)), bg);
        i = j;
    }
    let glyphs_from = mesh.vertices.len();
    let mut galleys: std::collections::HashMap<char, std::sync::Arc<egui::Galley>> = Default::default();
    let mut combining = Vec::new();
    for dc in &cells {
        if dc.c == ' ' {
            continue;
        }
        if dc.zw.is_some() {
            combining.push(dc);
            continue;
        }
        let g = galleys.entry(dc.c).or_insert_with(|| {
            painter.layout_no_wrap(dc.c.to_string(), font_id.clone(), Color32::PLACEHOLDER)
        });
        let pos = Pos2::new((dc.x * ppp).round() / ppp, (dc.y * ppp).round() / ppp);
        for row in &g.rows {
            let off = pos.to_vec2() + row.pos.to_vec2();
            let base = mesh.vertices.len() as u32;
            mesh.vertices.extend(row.visuals.mesh.vertices.iter().map(|v| egui::epaint::Vertex {
                pos: v.pos + off,
                uv: v.uv,
                color: dc.fg,
            }));
            mesh.indices.extend(row.visuals.mesh.indices.iter().map(|i| i + base));
        }
    }
    // Galley UVs are in atlas pixels. Normalize after the last layout above,
    // since a new glyph can grow the atlas mid-frame.
    let tex = ui.ctx().fonts_mut(|f| f.font_image_size());
    for v in &mut mesh.vertices[glyphs_from..] {
        v.uv = Pos2::new(v.uv.x / tex[0] as f32, v.uv.y / tex[1] as f32);
    }
    ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(ATLAS_KEY), tex));
    painter.add(egui::Shape::mesh(mesh));
    for dc in &cells {
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
    // Combining marks need real shaping to sit on their base char.
    let mut buf = String::with_capacity(8);
    for dc in combining {
        buf.clear();
        buf.push(dc.c);
        buf.extend(dc.zw.iter().flatten());
        painter.text(Pos2::new(dc.x, dc.y), Align2::LEFT_TOP, &buf, font_id.clone(), dc.fg);
    }

    // Underline the path under the pointer so it reads as clickable.
    if let Some((rects, _)) = &hovered_link {
        for lrect in rects {
            let uy = lrect.bottom() - 1.0;
            painter.line_segment(
                [Pos2::new(lrect.left(), uy), Pos2::new(lrect.right(), uy)],
                Stroke::new(1.0, palette::group_accent()),
            );
        }
    }

    // Cursor.
    if frame.cursor_visible
        && let Some((cx, cy)) = frame.cursor
        && cy < rows
    {
        let x = origin.x + cx as f32 * cell_w;
        let y = origin.y + cy as f32 * cell_h;
        let cell_rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_w, cell_h));
        let shape = if focused { frame.cursor_shape } else { CursorShape::HollowBlock };
        match shape {
            CursorShape::Block => {
                painter.rect_filled(cell_rect, 0.0, palette::cursor());
                let c = substitute(frame.cursor_char);
                if c != ' ' {
                    painter.text(Pos2::new(x, y), Align2::LEFT_TOP, c, font_id.clone(), palette::term_bg());
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

    live.notifier.notify(out);

    link_menu(ui, session_id, clicked_link);

    info
}

const ATLAS_KEY: &str = "kip-grid-atlas";

/// Call after the frame's last UI: text laid out after the grid can grow the
/// glyph atlas, leaving the grid's UVs scaled for the old size. Redraw then,
/// or an idle window keeps the garbled frame.
pub fn atlas_check(ctx: &egui::Context) {
    let used = ctx.data(|d| d.get_temp::<[usize; 2]>(egui::Id::new(ATLAS_KEY)));
    if used.is_some_and(|t| t != ctx.fonts_mut(|f| f.font_image_size())) {
        ctx.request_repaint();
    }
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

/// A line counts as wrapped when its text runs to within this many columns of
/// the right edge (claude keeps a small margin when it wraps its own output).
const WRAP_SLACK: usize = 4;

fn runs_to_edge(chars: &[char]) -> Option<usize> {
    let end = chars.iter().rposition(|c| !c.is_whitespace())? + 1;
    (end + WRAP_SLACK >= chars.len()).then_some(end)
}

/// The rows and column spans a path token covers when lines were broken in
/// the middle of it: a token touching the line's end continues at the start
/// of the next line (after its indent), one starting a line continues from the
/// end of the previous one. `row_chars(r)` is None outside the grid.
fn wrapped_parts(
    row_chars: &dyn Fn(i32) -> Option<Vec<char>>,
    row: i32,
    span: std::ops::Range<usize>,
) -> Vec<(i32, std::ops::Range<usize>)> {
    const MAX_LINES: i32 = 4;
    let raw_end = |c: &[char], start: usize| {
        let mut e = start;
        while e < c.len() && is_link_char(c[e]) {
            e += 1;
        }
        e
    };
    let mut parts = vec![(row, span.start..raw_end(&row_chars(row).unwrap_or_default(), span.start))];
    // Backward.
    let mut r = row;
    let mut start = span.start;
    while row - r < MAX_LINES {
        let Some(cur) = row_chars(r) else { break };
        if cur.iter().position(|c| !c.is_whitespace()) != Some(start) {
            break;
        }
        let Some(prev) = row_chars(r - 1) else { break };
        let Some(end) = runs_to_edge(&prev) else { break };
        let mut st = end;
        while st > 0 && is_link_char(prev[st - 1]) {
            st -= 1;
        }
        if st == end {
            break;
        }
        parts.insert(0, (r - 1, st..end));
        r -= 1;
        start = st;
    }
    // Forward.
    let mut r = row;
    while r - row < MAX_LINES {
        let Some(cur) = row_chars(r) else { break };
        let last_end = parts.last().map(|p| p.1.end).unwrap_or(0);
        if runs_to_edge(&cur) != Some(last_end) {
            break;
        }
        let Some(next) = row_chars(r + 1) else { break };
        let Some(st) = next.iter().position(|c| !c.is_whitespace()) else { break };
        let end = raw_end(&next, st);
        if end == st {
            break;
        }
        parts.push((r + 1, st..end));
        r += 1;
    }
    parts
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
    if p.exists() {
        return Some(p);
    }
    let rel = std::path::Path::new(token);
    if rel.is_absolute() || token.starts_with('~') {
        // Nothing left to guess about a rooted path except how it is spelled.
        return join_ci(std::path::Path::new("/"), p.strip_prefix("/").ok()?);
    }
    // A multi-component relative token is worth hunting for; a bare name is not
    // (too many files share one, and prose like "e.g." would start a walk).
    if !token.contains('/') {
        return None;
    }
    find_near(rel, &cwd)
}

/// `base/rel` with each component matched case-insensitively when the exact name
/// is missing. macOS volumes ignore case by default, so this is for the ones
/// that do not (case-sensitive APFS, linux).
fn join_ci(base: &std::path::Path, rel: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::Component;
    let mut cur = base.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(name) => {
                let exact = cur.join(name);
                if exact.exists() {
                    cur = exact;
                    continue;
                }
                let want = name.to_string_lossy().to_lowercase();
                let hit = std::fs::read_dir(&cur)
                    .ok()?
                    .flatten()
                    .find(|e| e.file_name().to_string_lossy().to_lowercase() == want)?;
                cur = hit.path();
            },
            Component::CurDir => {},
            Component::ParentDir => {
                cur.pop();
            },
            _ => return None,
        }
    }
    Some(cur)
}

/// Where else a relative path printed in the terminal can live. Two cases, both
/// common: an agent prints paths from its project root while the shell sits
/// deeper (look up), and the first folder of the path sits below the cwd -
/// `reels/01/cover.json` inside `content-factory/` (look down, bounded).
fn find_near(rel: &std::path::Path, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    const MAX_UP: usize = 6;
    const MAX_DEPTH: usize = 3;
    const MAX_DIRS: usize = 250;
    const SKIP: &[&str] = &["node_modules", "target", "dist", "build", ".next", ".venv"];

    for base in cwd.ancestors().skip(1).take(MAX_UP) {
        if let Some(p) = join_ci(base, rel) {
            return Some(p);
        }
    }
    // Downward: one stat per directory (the whole `rel` at once), never a listing
    // of the files inside, and capped so a deep tree cannot stall the hover. Exact
    // spelling only - fixing case here would cost a listing per path component
    // per directory visited.
    let mut queue = std::collections::VecDeque::from([(cwd.to_path_buf(), 0usize)]);
    let mut visited = 0usize;
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
                continue;
            }
            visited += 1;
            if visited > MAX_DIRS {
                return None;
            }
            let sub = e.path();
            let hit = sub.join(rel);
            if hit.exists() {
                return Some(hit);
            }
            if depth + 1 < MAX_DEPTH {
                queue.push_back((sub, depth + 1));
            }
        }
    }
    None
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

/// Auto-repeats a held Backspace spends erasing single characters before it
/// starts erasing whole words.
const BS_WORD_AFTER: u8 = 6;

/// A held Backspace switches to word-wise erase once it has repeated a few
/// times, so clearing a long prompt does not take one repeat per character.
/// `count` is the streak; any other key or a fresh press ends it.
fn backspace_accel(count: &mut u8, key: Key, repeat: bool, mods: Modifiers) -> Modifiers {
    if key != Key::Backspace || mods.alt || mods.ctrl || mods.command {
        *count = 0;
        return mods;
    }
    *count = if repeat { count.saturating_add(1) } else { 0 };
    if *count >= BS_WORD_AFTER { Modifiers { alt: true, ..mods } } else { mods }
}

fn encode_key(key: Key, mods: Modifiers, mode: Modes, claude: bool) -> Option<Vec<u8>> {
    // Cmd+Z = undo: Ctrl+_ is undo in Claude's prompt (chat:undo), zsh and bash.
    if mods.mac_cmd && !mods.shift && key == Key::Z {
        return Some(vec![0x1f]);
    }
    if mods.command {
        return None; // App-level shortcuts.
    }
    let kitty = mode.kitty;
    let app_cursor = mode.app_cursor;
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
            } else if mods.alt || mods.ctrl {
                // Meta+Backspace: readline, zsh and claude all read it as
                // "erase the word before the cursor". Ctrl+Backspace used to
                // send ^H, which those three treat as a plain backspace.
                b"\x1b\x7f".to_vec()
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
    use super::{backspace_accel, encode_key, find_near, join_ci, link_span, wrapped_parts, BS_WORD_AFTER};
    use super::{Key, Modifiers, Modes};

    fn span(s: &str, col: usize) -> Option<(std::ops::Range<usize>, String)> {
        link_span(&s.chars().collect::<Vec<_>>(), col)
    }

    #[test]
    fn backspace_erases_a_word_with_ctrl_or_alt() {
        let plain = Modes::default();
        let bs = |mods| encode_key(Key::Backspace, mods, plain, false);
        assert_eq!(bs(Modifiers::NONE), Some(b"\x7f".to_vec()));
        assert_eq!(bs(Modifiers::CTRL), Some(b"\x1b\x7f".to_vec()));
        assert_eq!(bs(Modifiers::ALT), Some(b"\x1b\x7f".to_vec()));
        // Apps that pushed kitty flags get the report instead.
        let kitty = Modes { kitty: true, ..Modes::default() };
        assert_eq!(
            encode_key(Key::Backspace, Modifiers::CTRL, kitty, false),
            Some(b"\x1b[127;5u".to_vec())
        );
    }

    #[test]
    fn held_backspace_switches_to_words() {
        let word = |mods: Modifiers| mods.alt;
        let mut n = 0;
        assert!(!word(backspace_accel(&mut n, Key::Backspace, false, Modifiers::NONE)));
        for _ in 1..BS_WORD_AFTER {
            assert!(!word(backspace_accel(&mut n, Key::Backspace, true, Modifiers::NONE)));
        }
        assert!(word(backspace_accel(&mut n, Key::Backspace, true, Modifiers::NONE)));
        assert!(word(backspace_accel(&mut n, Key::Backspace, true, Modifiers::NONE)));

        // Another key ends the streak, and the next hold starts over.
        backspace_accel(&mut n, Key::A, false, Modifiers::NONE);
        assert!(!word(backspace_accel(&mut n, Key::Backspace, true, Modifiers::NONE)));
    }

    #[test]
    fn path_broken_across_lines_is_joined() {
        // The real case: claude wrapped "Файл: /private/.../scratchp" + "  ad/comp/x.jpg".
        let lines = ["  Файл: /private/tmp/sc/scratchp", "  ad/comp/hand.jpg  done", "  next line"];
        let rows: Vec<Vec<char>> = lines.iter().map(|l| format!("{l:<32}").chars().collect()).collect();
        let get = |r: i32| rows.get(r as usize).cloned();
        let text = |parts: &[(i32, std::ops::Range<usize>)]| -> String {
            parts.iter().map(|(r, sp)| rows[*r as usize][sp.clone()].iter().collect::<String>()).collect()
        };
        // Hover on either half gives the whole path.
        let (sp, _) = link_span(&rows[0], 12).unwrap();
        assert_eq!(text(&wrapped_parts(&get, 0, sp)), "/private/tmp/sc/scratchpad/comp/hand.jpg");
        let (sp, _) = link_span(&rows[1], 4).unwrap();
        assert_eq!(text(&wrapped_parts(&get, 1, sp)), "/private/tmp/sc/scratchpad/comp/hand.jpg");
        // A short line is not a wrap: nothing is glued on.
        let (sp, _) = link_span(&rows[2], 3).unwrap();
        assert_eq!(wrapped_parts(&get, 2, sp).len(), 1);
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

    #[test]
    fn finds_path_below_and_above_the_cwd() {
        use std::fs;
        let base = std::env::temp_dir().join(format!("kip_link_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let deep = base.join("content-factory/reels/01-ferritin/inst/en");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir_all(base.join("node_modules/reels/01-ferritin/inst/en")).unwrap();
        fs::write(deep.join("cover.json"), "").unwrap();
        fs::write(base.join("node_modules/reels/01-ferritin/inst/en/cover.json"), "").unwrap();
        let rel = std::path::Path::new("reels/01-ferritin/inst/en/cover.json");

        // A folder deep: cwd is the session's, the path starts one level below it.
        assert_eq!(find_near(rel, &base), Some(deep.join("cover.json")));
        // The same path printed from a cwd deeper than the folder it starts at.
        assert_eq!(find_near(rel, &deep), Some(deep.join("cover.json")));
        // Nothing to find: no hit, and node_modules was never even entered.
        assert_eq!(find_near(std::path::Path::new("reels/nope.json"), &base), None);

        // Case only. A case-insensitive volume (the macOS default) resolves it by
        // the literal spelling, so compare what it points at, not how it reads.
        let ci = join_ci(&base, std::path::Path::new("Content-Factory/REELS")).unwrap();
        assert_eq!(ci.canonicalize().ok(), base.join("content-factory/reels").canonicalize().ok());
        let _ = fs::remove_dir_all(&base);
    }

    fn term_with(cols: u16, text: &str) -> crate::vt::Term {
        let mut t = crate::vt::Term::new(cols, 5, 1000);
        t.write(text.as_bytes());
        t
    }

    #[test]
    fn find_is_case_insensitive_literal_and_crosses_wraps() {
        let t = term_with(10, "Hello a.b\r\nhello axb\r\n0123456789abcHELLO\r\n");
        assert_eq!(super::find_all(&t, "HeLLo").len(), 3);
        assert_eq!(super::find_all(&t, "a.b").len(), 1);
        // "89abc" sits across the soft wrap at column 10.
        assert_eq!(super::find_all(&t, "89abc").len(), 1);
        assert!(super::find_all(&t, "").is_empty());
    }

    #[test]
    fn find_folds_cyrillic_case() {
        let t = term_with(40, "Ошибка: файл\r\nвторая ошибка\r\nОШИБКА\r\n");
        assert_eq!(super::find_all(&t, "ошибка").len(), 3);
        assert_eq!(super::find_all(&t, "Ошибка").len(), 3);
        assert_eq!(super::find_all(&t, "ОШИБКА").len(), 3);
        // Positions point at the cells: "ошибка" on row 1 starts at column 7.
        let m = super::find_all(&t, "ошибка")[1];
        assert_eq!((m.start.x, m.end.x), (7, 12));
    }

    #[test]
    fn cmd_z_sends_undo() {
        let mut m = Modifiers::MAC_CMD;
        m.command = true;
        assert_eq!(encode_key(Key::Z, m, Modes::default(), true), Some(vec![0x1f]));
    }
}
