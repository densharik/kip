//! The past-conversations browser (Cmd+Y): every Claude session on disk,
//! filtered by folder and text, with a preview and one-click continue.
use std::sync::atomic::{AtomicUsize, Ordering};

use egui::text::{LayoutJob, TextFormat};

use super::*;

const ROW_H: f32 = 46.0;
/// Preview shows at most this many of the latest messages, each cut to MSG_MAX chars.
const PREVIEW_MSGS: usize = 400;
const MSG_MAX: usize = 4000;

fn fmt_size(b: u64) -> String {
    if b >= 1 << 20 { format!("{:.1} MB", b as f64 / (1 << 20) as f64) } else { format!("{} KB", (b >> 10).max(1)) }
}

/// `text` with every match of `re` on an accent background.
fn highlighted(text: &str, re: Option<&regex::Regex>, font: FontId, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let plain = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let hit = TextFormat { font_id: font, color, background: palette::selection(), ..Default::default() };
    let mut at = 0;
    if let Some(re) = re {
        for m in re.find_iter(text) {
            job.append(&text[at..m.start()], 0.0, plain.clone());
            job.append(m.as_str(), 0.0, hit.clone());
            at = m.end();
        }
    }
    job.append(&text[at..], 0.0, plain);
    job
}

impl App {
    pub(super) fn toggle_past(&mut self, ctx: &egui::Context) {
        self.w.past_open = !self.w.past_open;
        if !self.w.past_open {
            return;
        }
        self.past.focus = true;
        if !self.past.loading && self.past.loaded_at.is_none_or(|t| t.elapsed() > Duration::from_secs(15)) {
            self.past.loading = true;
            let (tx, ctx) = (self.past_tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let _ = tx.send(PastMsg::Index(history::load_index()));
                ctx.request_repaint();
            });
        }
    }

    pub(super) fn drain_past(&mut self, ctx: &egui::Context) {
        let epoch = self.past.epoch.load(Ordering::Relaxed);
        while let Ok(msg) = self.past_rx.try_recv() {
            #[allow(clippy::collapsible_match)]
            match msg {
                PastMsg::Index(entries) => {
                    self.past.hay = entries
                        .iter()
                        .map(|e| format!("{}\n{}\n{}", e.name(), e.first, e.cwd.display()).to_lowercase())
                        .collect();
                    self.past.entries = entries;
                    self.past.loading = false;
                    self.past.loaded_at = Some(Instant::now());
                    // New or changed files: run the content search again.
                    self.past.searched.clear();
                    // A selection asked for before the index was there (show_history).
                    if self.past.preview.is_none()
                        && let Some(sid) = self.past.sel.take() {
                            self.past_select(sid);
                        }
                },
                PastMsg::Hit(g, sid, n) if g == epoch => {
                    self.past.hits.insert(sid, n);
                },
                // Workers report out of order; never step backwards.
                PastMsg::Progress(g, done) if g == epoch => self.past.done = self.past.done.max(done),
                PastMsg::Preview(sid, msgs) => {
                    if self.past.sel.as_deref() == Some(sid.as_str()) {
                        let re = history::matcher(&self.past.query).filter(|_| !self.past.query.is_empty());
                        self.past.jump = match &re {
                            Some(re) => msgs.iter().rposition(|(_, t)| re.is_match(t)),
                            None => None,
                        }
                        .or(msgs.len().checked_sub(1));
                        self.past.preview = Some((sid, msgs));
                    }
                },
                _ => {},
            }
        }
        if let Some(id) = self.past.focus_session.take()
            && let Ok(k) = self.win_of(id) {
                self.in_window(k, |a| {
                    a.w.past_open = false;
                    a.apply(vec![Act::Select(id)], ctx);
                });
                ctx.send_viewport_cmd_to(self.wins[k].viewport(), egui::ViewportCommand::Focus);
            }
    }

    /// Bring a past conversation back: switch to its tab if kip has it open,
    /// else open a terminal in its folder running `claude --resume`.
    pub(super) fn past_resume(&mut self, sid: &str, ctx: &egui::Context) {
        let Some(e) = self.past.entries.iter().find(|e| e.sid == sid).cloned() else { return };
        self.w.past_open = false;
        let mine = |s: &Session| s.claude_session_id.as_deref() == Some(sid);
        if let Some(id) = self.sessions.iter().find(|s| mine(s)).map(|s| s.id) {
            self.apply(vec![Act::Select(id)], ctx);
            return;
        }
        if let Some(id) = self.parked.iter().find(|s| mine(s)).map(|s| s.id) {
            self.past.focus_session = Some(id);
            return;
        }
        let cwd = if e.cwd.is_dir() { e.cwd.clone() } else { dirs::home_dir().unwrap_or_default() };
        self.spawn(cwd, None, ctx);
        if let Some(id) = self.w.active
            && let Some(idx) = self.idx_of(id) {
                self.sessions[idx].claude_session_id = Some(e.sid.clone());
                self.sessions[idx].claude_title = e.title.clone();
                self.resume(id, true, ctx);
            }
        self.persist();
    }

    /// Indices of entries to list, newest first.
    fn past_filtered(&self, scope: &std::path::Path) -> Vec<usize> {
        let q = self.past.query.trim().to_lowercase();
        (0..self.past.entries.len())
            .filter(|&i| {
                let e = &self.past.entries[i];
                (self.past.all || e.cwd.starts_with(scope))
                    && (q.is_empty() || self.past.hay[i].contains(&q) || self.past.hits.contains_key(&e.sid))
            })
            .collect()
    }

    fn past_search(&mut self, ctx: &egui::Context) {
        let q = self.past.query.trim().to_string();
        self.past.searched = q.clone();
        self.past.hits.clear();
        self.past.done = 0;
        let epoch = self.past.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let Some(re) = history::matcher(&q).filter(|_| q.chars().count() >= 2) else {
            // Nothing to wait for: too short to search the text, titles only.
            self.past.done = self.past.entries.len();
            return;
        };
        let files: Vec<(String, PathBuf)> = self.past.entries.iter().map(|e| (e.sid.clone(), e.path.clone())).collect();
        let (tx, ctx, cancel) = (self.past_tx.clone(), ctx.clone(), self.past.epoch.clone());
        std::thread::spawn(move || {
            let next = AtomicUsize::new(0);
            let done = AtomicUsize::new(0);
            std::thread::scope(|sc| {
                for _ in 0..4 {
                    sc.spawn(|| loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() || cancel.load(Ordering::Relaxed) != epoch {
                            return;
                        }
                        let n = history::count_hits(&files[i].0, &re);
                        if n > 0 {
                            let _ = tx.send(PastMsg::Hit(epoch, files[i].0.clone(), n));
                        }
                        let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                        if n > 0 || d.is_multiple_of(16) || d == files.len() {
                            let _ = tx.send(PastMsg::Progress(epoch, d));
                            ctx.request_repaint();
                        }
                    });
                }
            });
        });
    }

    pub(super) fn past_select(&mut self, sid: String) {
        if self.past.sel.as_ref() == Some(&sid) {
            return;
        }
        let Some(path) = self.past.entries.iter().find(|e| e.sid == sid).map(|e| e.path.clone()) else {
            if self.past.loading {
                self.past.sel = Some(sid);
            }
            return;
        };
        self.past.sel = Some(sid.clone());
        self.past.preview = None;
        self.past.at = None;
        let tx = self.past_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(PastMsg::Preview(sid, history::messages(&path, PREVIEW_MSGS)));
        });
    }

    pub(super) fn past_ui(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let ctx = ui.ctx().clone();
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, palette::term_bg());
        let field_id = egui::Id::new(("kip_past_field", self.cur_win));
        let scope = self.active_cwd();
        let mut list = self.past_filtered(&scope);
        // Nothing was ever run in this folder: show everything instead of a blank list.
        if list.is_empty() && !self.past.all && self.past.query.trim().is_empty() && !self.past.entries.is_empty() {
            self.past.all = true;
            list = self.past_filtered(&scope);
        }
        let sel_pos = self.past.sel.as_ref().and_then(|s| list.iter().position(|&i| &self.past.entries[i].sid == s));

        // Keys while the search field has focus.
        let (mut close, mut step, mut go) = (false, 0i32, false);
        if self.past.focus || ctx.memory(|m| m.has_focus(field_id)) {
            ctx.input_mut(|i| {
                close = consume_plain(i, Key::Escape);
                go = consume_plain(i, Key::Enter);
                if consume_plain(i, Key::ArrowDown) {
                    step = 1;
                }
                if consume_plain(i, Key::ArrowUp) {
                    step = -1;
                }
            });
        }
        if close {
            self.w.past_open = false;
            return;
        }
        if step != 0 && !list.is_empty() {
            let at = match sel_pos {
                Some(p) => (p as i32 + step).clamp(0, list.len() as i32 - 1) as usize,
                None => 0,
            };
            let sid = self.past.entries[list[at]].sid.clone();
            self.past_select(sid);
            self.past.reveal = true;
        }
        if go {
            // The selection only counts while the filter still shows it.
            let at = sel_pos.unwrap_or(0);
            if let Some(sid) = list.get(at).map(|&i| self.past.entries[i].sid.clone()) {
                acts.push(Act::PastResume(sid));
            }
        }

        // Top bar: query, scope, status, close.
        let accent = palette::selection();
        let mut changed = false;
        let mut toggle_all = false;
        let top = Rect::from_min_size(rect.min, Vec2::new(rect.width(), 44.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(top.shrink2(Vec2::new(12.0, 7.0))), |ui| {
            ui.horizontal_centered(|ui| {
                egui::Frame::new()
                    .stroke(Stroke::new(1.5, accent))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(6, 3))
                    .show(ui, |ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.past.query)
                                .id(field_id)
                                .frame(egui::Frame::NONE)
                                .desired_width(260.0)
                                .hint_text(tr("Поиск по названию и тексту разговоров", "Search titles and conversation text")),
                        );
                        if self.past.focus {
                            r.request_focus();
                            self.past.focus = false;
                        }
                        changed = r.changed();
                    });
                let here = scope.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| tilde(&scope));
                if ui.selectable_label(!self.past.all, RichText::new(here).size(12.5)).on_hover_text(tilde(&scope)).clicked()
                    && self.past.all
                {
                    toggle_all = true;
                }
                if ui.selectable_label(self.past.all, RichText::new(tr("Все проекты", "All projects")).size(12.5)).clicked()
                    && !self.past.all
                {
                    toggle_all = true;
                }
                let status = if self.past.loading && self.past.entries.is_empty() {
                    tr("Читаю историю...", "Reading history...").to_string()
                } else if !self.past.searched.is_empty() && self.past.done < self.past.entries.len() {
                    format!("{} {}/{}", tr("Ищу в тексте", "Searching text"), self.past.done, self.past.entries.len())
                } else {
                    format!("{} {}", list.len(), tr("разговоров", "conversations"))
                };
                ui.label(RichText::new(status).size(11.5).color(palette::text_dim()));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if find_btn(ui, 2).on_hover_text(tr("Закрыть (Esc)", "Close (Esc)")).clicked() {
                        acts.push(Act::TogglePast);
                    }
                });
            });
        });
        ui.painter().hline(rect.x_range(), top.bottom(), Stroke::new(1.0, palette::border_dim()));
        if toggle_all {
            self.past.all = !self.past.all;
        }
        if changed {
            self.past.edited = Some(Instant::now());
            if self.past.query.trim().is_empty() {
                self.past_search(&ctx);
            }
        }
        // Content search once typing pauses.
        let q = self.past.query.trim().to_string();
        if !q.is_empty() && q != self.past.searched && !self.past.entries.is_empty() {
            if self.past.edited.is_none_or(|t| t.elapsed() >= Duration::from_millis(250)) {
                self.past_search(&ctx);
            } else {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }

        let body = Rect::from_min_max(Pos2::new(rect.min.x, top.bottom() + 1.0), rect.max);
        let split = body.min.x + (body.width() * 0.4).clamp(260.0, 460.0);
        let left = Rect::from_min_max(body.min, Pos2::new(split, body.max.y));
        let right = Rect::from_min_max(Pos2::new(split + 1.0, body.min.y), body.max);
        ui.painter().vline(split, body.y_range(), Stroke::new(1.0, palette::border_dim()));

        // List.
        let open: HashSet<String> = self
            .sessions
            .iter()
            .chain(self.parked.iter())
            .filter_map(|s| s.claude_session_id.clone())
            .collect();
        let mut picked: Option<String> = None;
        let mut resume: Option<String> = None;
        let reveal = std::mem::take(&mut self.past.reveal);
        // show_rows lays out only the visible rows, so a row the keyboard moved
        // to off screen cannot scroll itself in: set the offset from here.
        let mut area = ScrollArea::vertical().id_salt("kip_past_list").auto_shrink([false, false]);
        if reveal {
            let (off, h) = self.past.list_view;
            if let Some(p) = self.past.sel.as_ref().and_then(|s| list.iter().position(|&i| &self.past.entries[i].sid == s)) {
                let y = p as f32 * ROW_H;
                if y < off {
                    area = area.vertical_scroll_offset(y);
                } else if y + ROW_H > off + h {
                    area = area.vertical_scroll_offset(y + ROW_H - h);
                }
            }
        }
        ui.scope_builder(egui::UiBuilder::new().max_rect(left), |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let out = area.show_rows(
                ui,
                ROW_H,
                list.len(),
                |ui, range| {
                    for i in range {
                        let e = &self.past.entries[list[i]];
                        let (r, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
                        let selected = self.past.sel.as_deref() == Some(e.sid.as_str());
                        let p = ui.painter();
                        if selected {
                            p.rect_filled(r, 0.0, palette::row_sel_bg());
                            p.rect_filled(Rect::from_min_size(r.min, Vec2::new(3.0, ROW_H)), 0.0, palette::accent_bar());
                        } else if resp.hovered() {
                            p.rect_filled(r, 0.0, palette::row_hover_bg());
                        }
                        let mut right_x = r.max.x - 10.0;
                        if let Some(n) = self.past.hits.get(&e.sid) {
                            let g = p.layout_no_wrap(n.to_string(), FontId::proportional(10.5), palette::term_bg());
                            let pill = Rect::from_min_size(
                                Pos2::new(right_x - g.size().x - 10.0, r.min.y + 7.0),
                                Vec2::new(g.size().x + 10.0, 15.0),
                            );
                            p.rect_filled(pill, 7.0, accent);
                            p.galley(pill.center() - g.size() / 2.0, g, palette::term_bg());
                            right_x = pill.min.x - 6.0;
                        }
                        if open.contains(&e.sid) {
                            p.circle_filled(Pos2::new(right_x - 3.0, r.min.y + 14.5), 3.0, palette::accent_bar());
                            right_x -= 12.0;
                        }
                        let chars = ((right_x - r.min.x - 14.0) / 7.0).max(4.0) as usize;
                        p.text(
                            Pos2::new(r.min.x + 12.0, r.min.y + 7.0),
                            Align2::LEFT_TOP,
                            truncate_end(e.name(), chars),
                            FontId::proportional(13.0),
                            palette::text_strong(),
                        );
                        let when = fmt_ago(UNIX_EPOCH + Duration::from_secs(e.mtime));
                        let mut meta = format!("{}  {}  {}", when, fmt_size(e.size), tilde(&e.cwd));
                        if !e.branch.is_empty() && e.branch != "HEAD" {
                            meta.push_str(&format!("  {}", e.branch));
                        }
                        p.text(
                            Pos2::new(r.min.x + 12.0, r.min.y + 26.0),
                            Align2::LEFT_TOP,
                            truncate_end(&meta, ((r.width() - 20.0) / 6.2).max(4.0) as usize),
                            FontId::proportional(11.0),
                            palette::text_dim(),
                        );
                        if resp.double_clicked() {
                            resume = Some(e.sid.clone());
                        } else if resp.clicked() {
                            picked = Some(e.sid.clone());
                        }
                        resp.on_hover_text(tr("Клик - посмотреть, двойной клик или Enter - продолжить", "Click to preview, double-click or Enter to continue"));
                    }
                },
            );
            self.past.list_view = (out.state.offset.y, out.inner_rect.height());
        });
        if let Some(sid) = picked {
            self.past_select(sid);
        }
        if let Some(sid) = resume {
            acts.push(Act::PastResume(sid));
        }

        // Preview.
        let Some(e) = self.past.sel.as_ref().and_then(|s| self.past.entries.iter().find(|e| &e.sid == s)).cloned() else {
            ui.painter().text(
                right.center(),
                Align2::CENTER_CENTER,
                if self.past.entries.is_empty() && !self.past.loading {
                    tr("Прошлых разговоров с Claude не найдено", "No past Claude conversations found")
                } else {
                    tr("Выберите разговор слева", "Pick a conversation on the left")
                },
                FontId::proportional(13.0),
                palette::text_dim(),
            );
            return;
        };
        let re = history::matcher(&q).filter(|_| !q.is_empty());
        let msgs: &[(bool, String)] = match &self.past.preview {
            Some((sid, m)) if *sid == e.sid => m,
            _ => &[],
        };
        let matching: Vec<usize> = match &re {
            Some(re) => msgs.iter().enumerate().filter(|(_, (_, t))| re.is_match(t)).map(|(i, _)| i).collect(),
            None => Vec::new(),
        };
        let mut jump = self.past.jump.take();
        ui.scope_builder(egui::UiBuilder::new().max_rect(right.shrink2(Vec2::new(14.0, 10.0))), |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(truncate_end(e.name(), 90)).size(14.5).color(palette::text_strong()));
                    let mut meta = tilde(&e.cwd);
                    if !e.branch.is_empty() && e.branch != "HEAD" {
                        meta.push_str(&format!("  {}", e.branch));
                    }
                    meta.push_str(&format!("  {}", fmt_ago(UNIX_EPOCH + Duration::from_secs(e.mtime))));
                    ui.label(RichText::new(meta).size(11.0).color(palette::text_dim()));
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if open.contains(&e.sid) {
                        tr("Перейти", "Go to")
                    } else {
                        tr("Продолжить", "Continue")
                    };
                    if ui
                        .button(RichText::new(label).size(12.5))
                        .on_hover_text(format!("claude --resume {} (Enter)", e.sid))
                        .clicked()
                    {
                        acts.push(Act::PastResume(e.sid.clone()));
                    }
                    if !matching.is_empty() {
                        // Walk the messages that contain the query.
                        let cur = self.past.at.unwrap_or(usize::MAX);
                        if find_btn(ui, 1).on_hover_text(tr("Ниже", "Down")).clicked() {
                            jump = matching.iter().find(|&&m| cur == usize::MAX || m > cur).or(matching.first()).copied();
                        }
                        if find_btn(ui, 0).on_hover_text(tr("Выше", "Up")).clicked() {
                            jump = matching.iter().rev().find(|&&m| m < cur).or(matching.last()).copied();
                        }
                        ui.label(
                            RichText::new(format!("{} {}", matching.len(), tr("сообщ. с совпадением", "matching messages")))
                                .size(11.0)
                                .color(palette::text_dim()),
                        );
                    }
                });
            });
            ui.add_space(6.0);
            if self.past.preview.as_ref().is_none_or(|(s, _)| *s != e.sid) {
                ui.label(RichText::new(tr("Загружаю...", "Loading...")).color(palette::text_dim()));
                return;
            }
            ScrollArea::vertical().id_salt(("kip_past_prev", e.sid.as_str())).auto_shrink([false, false]).show(ui, |ui| {
                for (i, (user, text)) in msgs.iter().enumerate() {
                    let (who, col) = if *user {
                        (tr("Вы", "You"), palette::accent_bar())
                    } else {
                        ("Claude", palette::text_dim())
                    };
                    let head = ui.label(RichText::new(who).size(11.0).strong().color(col));
                    let shown: String = if text.chars().count() > MSG_MAX {
                        let mut t: String = text.chars().take(MSG_MAX).collect();
                        t.push_str(" ...");
                        t
                    } else {
                        text.clone()
                    };
                    let job = highlighted(&shown, re.as_ref(), FontId::proportional(12.5), palette::text());
                    let body = ui.add(egui::Label::new(job).wrap());
                    if jump == Some(i) {
                        head.union(body).scroll_to_me(Some(egui::Align::Center));
                    }
                    ui.add_space(10.0);
                }
            });
        });
        // Remember where the arrows left off.
        if jump.is_some() {
            self.past.at = jump;
        }
    }
}
