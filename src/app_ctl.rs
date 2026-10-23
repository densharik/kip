//! What kip does on control-channel requests (MCP tools), and the offer to
//! connect kip to Claude Code as an MCP server.
use serde_json::{json, Value};

use super::*;

fn arg_id(args: &Value) -> Result<u64, String> {
    args["id"].as_u64().ok_or_else(|| "id is required".to_string())
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest.trim_start_matches(['/', '\\'])),
        None => PathBuf::from(path),
    }
}

/// Plain user/Claude text of a transcript tail, oldest first.
fn transcript_text(path: &std::path::Path, max_chars: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(4 * 1024 * 1024))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let mut out = String::new();
    for line in String::from_utf8_lossy(&buf).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        let Some((user, text)) = history::msg_text(&v) else { continue };
        out.push_str(&format!("{}: {text}\n\n", if user { "User" } else { "Claude" }));
    }
    if out.len() > max_chars {
        let cut = out.len() - max_chars;
        let cut = out.char_indices().map(|(i, _)| i).find(|&i| i >= cut).unwrap_or(0);
        out = out.split_off(cut);
    }
    Some(out)
}

impl App {
    pub(super) fn drain_ctl(&mut self, ctx: &egui::Context) {
        while let Ok(req) = self.ctl_rx.try_recv() {
            if req.op == "list_history" {
                self.list_history(req);
                continue;
            }
            let r = self.ctl(&req.op, &req.args, ctx);
            let _ = req.reply.send(r);
        }
        while let Ok(msg) = self.mcp_rx.try_recv() {
            match msg {
                McpMsg::Installed(yes) => self.mcp.show = !yes && !self.settings.mcp_offer_off,
                McpMsg::Done(r) => {
                    self.mcp.busy = false;
                    self.mcp.status = Some(r);
                },
            }
        }
    }

    /// Past Claude conversations for MCP. Reading and searching them takes
    /// seconds, so it runs off the UI thread and answers the request itself.
    fn list_history(&self, req: ctl::Req) {
        let open: std::collections::HashMap<String, u64> = self
            .sessions
            .iter()
            .filter_map(|s| Some((s.claude_session_id.clone()?, s.id)))
            .collect();
        std::thread::spawn(move || {
            // ctl gives up on a reply after 15s; the index read counts too.
            let deadline = Instant::now() + Duration::from_secs(10);
            let args = &req.args;
            let entries = history::load_index();
            let scope = args["cwd"].as_str().map(expand);
            let q = args["query"].as_str().map(str::trim).filter(|q| !q.is_empty());
            let re = q.and_then(history::matcher);
            let limit = args["limit"].as_u64().unwrap_or(20) as usize;
            let mut out = Vec::new();
            let mut partial = false;
            for e in entries.iter().filter(|e| scope.as_ref().is_none_or(|d| e.cwd.starts_with(d))) {
                if out.len() >= limit {
                    break;
                }
                let mut hits = 0;
                if let Some(re) = &re {
                    let named = re.is_match(e.name()) || re.is_match(&e.first);
                    if Instant::now() > deadline {
                        partial = true;
                        if !named {
                            continue;
                        }
                    } else {
                        hits = history::count_hits(&e.sid, re);
                    }
                    if !named && hits == 0 {
                        continue;
                    }
                }
                let age = SystemTime::now()
                    .duration_since(UNIX_EPOCH + Duration::from_secs(e.mtime))
                    .map_or(0.0, |d| d.as_secs_f64() / 3600.0);
                out.push(json!({
                    "sid": e.sid,
                    "title": e.title,
                    "first_prompt": e.first,
                    "cwd": e.cwd,
                    "branch": e.branch,
                    "age_hours": (age * 10.0).round() / 10.0,
                    "size_kb": e.size / 1024,
                    "hits": hits,
                    "open_in_kip": open.get(&e.sid),
                }));
            }
            let r = if partial { json!({"sessions": out, "partial": true}) } else { json!(out) };
            let _ = req.reply.send(Ok(r));
        });
    }

    fn win_index(&self, wid: u64) -> Result<usize, String> {
        self.wins.iter().position(|w| w.id == wid).ok_or_else(|| format!("no window {wid}"))
    }

    fn session_index(&self, id: u64) -> Result<usize, String> {
        self.idx_of(id).ok_or_else(|| format!("no session {id}"))
    }

    /// Window index of session `id` (call outside a window pass).
    pub(super) fn win_of(&self, id: u64) -> Result<usize, String> {
        let idx = self.session_index(id)?;
        self.win_index(self.sessions[idx].window)
    }

    fn session_json(&self, s: &Session) -> Value {
        let state = match s.phase {
            Phase::Live(_) => "live",
            Phase::Suspended => "suspended",
            Phase::Exited(_) => "exited",
        };
        let ctx = s.claude_session_id.as_deref().and_then(|sid| self.ctx_index.get(sid));
        json!({
            "id": s.id,
            "window": s.window,
            "name": s.display_name(),
            "custom_name": s.custom_name,
            "claude_title": s.claude_title,
            "cwd": s.cwd,
            "group": s.group,
            "project": project_path(s),
            "state": state,
            "busy": s.busy,
            "done": s.done,
            "claude_running": s.fg_is_claude,
            "running_cmd": s.running_cmd,
            "pinned": s.pinned,
            "on_screen": self.shown(s.id),
            "claude_session_id": s.claude_session_id,
            "context_pct": ctx.map(|c| c.pct.round()),
            "idle_minutes": s.last_activity.elapsed().as_secs() / 60,
        })
    }

    fn ctl(&mut self, op: &str, args: &Value, ctx: &egui::Context) -> Result<Value, String> {
        match op {
            "ping" => Ok(json!({"version": env!("CARGO_PKG_VERSION")})),
            // In the order the sidebar draws them.
            "list_sessions" => Ok(Value::Array(
                self.sidebar_order()
                    .into_iter()
                    .filter_map(|slot| if let Slot::Row(id) = slot { self.idx_of(id) } else { None })
                    .map(|i| self.session_json(&self.sessions[i]))
                    .collect(),
            )),
            "list_windows" => Ok(Value::Array(
                self.wins
                    .iter()
                    .map(|w| {
                        let n = self.sessions.iter().filter(|s| s.window == w.id).count();
                        json!({"id": w.id, "active_session": w.ui.active, "sessions": n})
                    })
                    .collect(),
            )),
            "open_session" => {
                let cwd = expand(args["cwd"].as_str().ok_or("cwd is required")?);
                if !cwd.is_dir() {
                    return Err(format!("not a folder: {}", cwd.display()));
                }
                let k = self.win_index(args["window"].as_u64().unwrap_or(0))?;
                let resume = args["resume"].as_str().filter(|s| session::valid_sid(s)).map(String::from);
                let command = if args["claude"].as_bool() == Some(true) {
                    let mut c = self.settings.claude_cmd.clone();
                    if self.settings.skip_permissions_default {
                        c.push_str(" --dangerously-skip-permissions");
                    }
                    Some(c)
                } else {
                    args["command"].as_str().map(String::from)
                };
                let id = self.in_window(k, |a| {
                    a.spawn(cwd, command.as_deref().map(keep_shell), ctx);
                    let id = a.w.active?;
                    let idx = a.idx_of(id)?;
                    if let Some(name) = args["name"].as_str().filter(|n| !n.trim().is_empty()) {
                        a.sessions[idx].custom_name = Some(name.trim().to_string());
                    }
                    // Unlike a click, no group given means ungrouped, not the active one's.
                    let g = args["group"].as_str().map(str::trim).filter(|g| !g.is_empty()).map(String::from);
                    if a.sessions[idx].group != g {
                        a.move_session(id, g, None);
                    }
                    if let Some(sid) = resume
                        && let Some(idx) = a.idx_of(id) {
                            a.sessions[idx].claude_session_id = Some(sid);
                            a.resume(id, true, ctx);
                        }
                    a.persist();
                    Some(id)
                });
                let id = id.ok_or("failed to open")?;
                let idx = self.session_index(id)?;
                Ok(self.session_json(&self.sessions[idx]))
            },
            "close_session" => {
                let id = arg_id(args)?;
                let idx = self.session_index(id)?;
                if self.sessions[idx].pinned && args["force"].as_bool() != Some(true) {
                    return Err("session is pinned; pass force to close it".into());
                }
                let k = self.win_of(id)?;
                self.in_window(k, |a| {
                    if let Some(i) = a.idx_of(id) {
                        a.remove_session(i);
                    }
                });
                Ok(json!("closed"))
            },
            "suspend_session" => {
                let idx = self.session_index(arg_id(args)?)?;
                if !matches!(self.sessions[idx].phase, Phase::Live(_)) {
                    return Err("session is not running".into());
                }
                self.sessions[idx].suspend();
                self.persist();
                Ok(json!("suspended"))
            },
            "wake_session" => {
                let id = arg_id(args)?;
                let idx = self.session_index(id)?;
                if matches!(self.sessions[idx].phase, Phase::Live(_)) {
                    return Err("session is already running".into());
                }
                let claude = args["claude"].as_bool() != Some(false) && self.sessions[idx].claude_session_id.is_some();
                let k = self.win_of(id)?;
                self.in_window(k, |a| a.resume(id, claude, ctx));
                self.persist();
                Ok(json!(if claude { "resumed with Claude" } else { "reopened as a shell" }))
            },
            "focus_session" => {
                let id = arg_id(args)?;
                let k = self.win_of(id)?;
                self.in_window(k, |a| a.apply(vec![Act::Select(id)], ctx));
                ctx.send_viewport_cmd_to(self.wins[k].viewport(), egui::ViewportCommand::Focus);
                Ok(json!("focused"))
            },
            "rename_session" => {
                let idx = self.session_index(arg_id(args)?)?;
                let name = args["name"].as_str().unwrap_or_default().trim();
                self.sessions[idx].custom_name = (!name.is_empty()).then(|| name.to_string());
                self.persist();
                Ok(json!(self.sessions[idx].display_name()))
            },
            "set_group" => {
                let id = arg_id(args)?;
                let idx = self.session_index(id)?;
                if self.sessions[idx].pinned {
                    return Err("session is pinned; unpin it first".into());
                }
                let g = args["group"].as_str().map(str::trim).filter(|g| !g.is_empty()).map(String::from);
                let k = self.win_of(id)?;
                self.in_window(k, |a| a.move_session(id, g, None));
                Ok(json!("moved"))
            },
            "pin_session" => {
                let idx = self.session_index(arg_id(args)?)?;
                self.sessions[idx].pinned = args["pinned"].as_bool().ok_or("pinned is required")?;
                self.persist();
                Ok(json!(self.sessions[idx].pinned))
            },
            "open_window" => {
                let cwd = args["cwd"].as_str().map(expand).unwrap_or_else(|| dirs::home_dir().unwrap_or_default());
                if !cwd.is_dir() {
                    return Err(format!("not a folder: {}", cwd.display()));
                }
                let wid = self.open_window();
                if let Some(w) = self.wins.iter_mut().find(|w| w.id == wid) {
                    w.pending_spawn = Some(cwd);
                }
                ctx.request_repaint();
                Ok(json!({"window": wid}))
            },
            "move_to_window" => {
                let id = arg_id(args)?;
                let idx = self.session_index(id)?;
                if self.sessions[idx].pinned {
                    return Err("session is pinned; unpin it first".into());
                }
                let win = args["window"].as_u64();
                if let Some(w) = win {
                    self.win_index(w)?;
                }
                let k = self.win_of(id)?;
                self.in_window(k, |a| a.apply(vec![Act::MoveToWindow { id, win }], ctx));
                // MoveToWindow parks the session; outside a pass it is back in `sessions`.
                let idx = self.session_index(id)?;
                Ok(json!({"window": self.sessions[idx].window}))
            },
            "save_state" => {
                for s in &mut self.sessions {
                    if matches!(s.phase, Phase::Live(_)) {
                        s.save_claude_session();
                    }
                }
                self.save_now();
                Ok(json!("saved"))
            },
            "read_screen" => {
                let s = &self.sessions[self.session_index(arg_id(args)?)?];
                let text = match s.live() {
                    Some(l) => session::grab_snapshot(&l.term),
                    None => s.snapshot.clone().unwrap_or_default(),
                };
                let n = args["lines"].as_u64().unwrap_or(60) as usize;
                let lines: Vec<&str> = text.lines().collect();
                Ok(json!(lines[lines.len().saturating_sub(n)..].join("\n")))
            },
            "style_group" => {
                // A manual group by name, or a project by its folder.
                let (key, name, project) = match (args["group"].as_str(), args["project"].as_str()) {
                    (Some(g), _) => {
                        if !self.sessions.iter().any(|s| s.group.as_deref() == Some(g)) {
                            return Err(format!("no group {g}"));
                        }
                        (g.to_string(), g.to_string(), false)
                    },
                    (None, Some(p)) => {
                        // Same spelling as the sidebar's key: no trailing slash, and an open project.
                        let p: PathBuf = expand(p).components().collect();
                        if !self.sessions.iter().any(|s| project_path(s) == p) {
                            return Err(format!("no open session in project {}", p.display()));
                        }
                        let p = p.to_string_lossy().into_owned();
                        (format!("project:{p}"), p, true)
                    },
                    _ => return Err("pass group or project".into()),
                };
                match &args["color"] {
                    Value::Null => {},
                    Value::String(c) if c.is_empty() => {
                        self.group_colors.remove(&key);
                    },
                    Value::String(c) => {
                        let c = c.trim_start_matches('#');
                        let hex = u32::from_str_radix(c, 16).ok().filter(|_| c.len() == 6).ok_or("color is #rrggbb")?;
                        self.group_colors.insert(key, [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8]);
                    },
                    _ => return Err("color is #rrggbb".into()),
                }
                if args.get("before").is_some() {
                    let before = args["before"].as_str().map(|b| {
                        if project { expand(b).components().collect::<PathBuf>().to_string_lossy().into_owned() } else { b.to_string() }
                    });
                    if project {
                        self.move_project(name, before);
                    } else {
                        self.move_group(name, before);
                    }
                }
                self.persist();
                Ok(json!("done"))
            },
            "open_folder" => {
                let dir = match args["path"].as_str() {
                    Some(p) => expand(p),
                    None => self.sessions[self.session_index(arg_id(args)?)?].cwd.clone(),
                };
                if !dir.is_dir() {
                    return Err(format!("not a folder: {}", dir.display()));
                }
                open_folder(&dir);
                Ok(json!("opened"))
            },
            "show_history" => {
                let k = self.win_index(args["window"].as_u64().unwrap_or(0))?;
                if let Some(q) = args["query"].as_str() {
                    self.past.query = q.to_string();
                    self.past.edited = None;
                }
                let sid = args["sid"].as_str().map(String::from);
                self.in_window(k, |a| {
                    if !a.w.past_open {
                        a.toggle_past(ctx);
                    }
                });
                if let Some(sid) = sid {
                    self.past_select(sid.clone());
                    if self.past.sel.as_ref() != Some(&sid) {
                        return Err(format!("no past conversation {sid}"));
                    }
                }
                ctx.send_viewport_cmd_to(self.wins[k].viewport(), egui::ViewportCommand::Focus);
                Ok(json!("shown"))
            },
            "search" => {
                use alacritty_terminal::grid::Dimensions;
                use alacritty_terminal::index::{Column, Point};
                let q = args["query"].as_str().filter(|q| !q.is_empty()).ok_or("query is required")?;
                let only = args["id"].as_u64();
                let mut found = Vec::new();
                for s in self.sessions.iter().filter(|s| only.is_none_or(|id| id == s.id)) {
                    let (n, lines): (usize, Vec<String>) = match s.live() {
                        Some(l) => {
                            let term = l.term.lock();
                            let hits = term_view::find_all(&term, q);
                            let lines = hits
                                .iter()
                                .rev()
                                .take(5)
                                .map(|m| {
                                    let ln = m.start().line;
                                    let end = Point::new(ln, term.last_column());
                                    term.bounds_to_string(Point::new(ln, Column(0)), end).trim().to_string()
                                })
                                .collect();
                            (hits.len(), lines)
                        },
                        None => {
                            let snap = s.snapshot.as_deref().unwrap_or_default();
                            let lq = q.to_lowercase();
                            let lines = snap.lines().filter(|l| l.to_lowercase().contains(&lq)).rev().take(5);
                            (snapshot_hits(Some(snap), q), lines.map(|l| l.trim().to_string()).collect())
                        },
                    };
                    if n > 0 {
                        found.push(json!({"id": s.id, "name": self.session_json(s)["name"], "hits": n, "lines": lines}));
                    }
                }
                Ok(Value::Array(found))
            },
            "read_transcript" => {
                let path = match args["sid"].as_str() {
                    // A sid from list_history may be newer than the jsonl map's last scan.
                    Some(sid) => self
                        .past
                        .entries
                        .iter()
                        .find(|e| e.sid == sid)
                        .map(|e| e.path.clone())
                        .or_else(|| ctx_index::resolve_jsonl(&self.jsonl_map, sid, None))
                        .or_else(|| ctx_index::scan_projects().remove(sid)),
                    None => {
                        let s = &self.sessions[self.session_index(arg_id(args)?)?];
                        let sid = s.claude_session_id.as_deref().ok_or("no Claude session in this terminal")?;
                        ctx_index::resolve_jsonl(&self.jsonl_map, sid, Some(&s.cwd))
                    },
                };
                let path = path.ok_or("transcript not found")?;
                let max = args["max_chars"].as_u64().unwrap_or(20_000) as usize;
                transcript_text(&path, max).map(Value::String).ok_or_else(|| "cannot read transcript".into())
            },
            "send_input" => {
                if !self.settings.mcp_send_input {
                    return Err("typing into sessions is off; enable it in kip Settings > MCP".into());
                }
                let id = arg_id(args)?;
                let text = args["text"].as_str().ok_or("text is required")?.to_string();
                let idx = self.session_index(id)?;
                if self.sessions[idx].live().is_none() {
                    return Err("session is not running; wake_session it first".into());
                }
                if args["enter"].as_bool() == Some(false) {
                    if let Some(l) = self.sessions[idx].live() {
                        l.notifier.notify(text.into_bytes());
                    }
                } else {
                    self.send_command_to(idx, &text, ctx);
                }
                Ok(json!("sent"))
            },
            "cleanup" => {
                let idle = Duration::from_secs(args["idle_minutes"].as_u64().unwrap_or(120) * 60);
                let remove = args["action"].as_str() == Some("remove");
                let dry = args["dry_run"].as_bool() != Some(false);
                let picked: Vec<u64> = self
                    .sessions
                    .iter()
                    .filter(|s| !s.pinned && !s.busy && !self.shown(s.id) && s.last_activity.elapsed() >= idle)
                    .filter(|s| matches!(s.phase, Phase::Live(_)) != remove)
                    .map(|s| s.id)
                    .collect();
                let listed: Vec<Value> = picked
                    .iter()
                    .filter_map(|id| self.idx_of(*id))
                    .map(|i| self.session_json(&self.sessions[i]))
                    .collect();
                if !dry {
                    for id in &picked {
                        let Ok(k) = self.win_of(*id) else { continue };
                        let id = *id;
                        self.in_window(k, |a| {
                            if let Some(i) = a.idx_of(id) {
                                if remove {
                                    a.remove_session(i);
                                } else {
                                    a.sessions[i].suspend();
                                    a.persist();
                                }
                            }
                        });
                    }
                }
                Ok(json!({"action": if remove { "remove" } else { "suspend" }, "dry_run": dry, "sessions": listed}))
            },
            "get_settings" => serde_json::to_value(&self.settings).map_err(|e| e.to_string()),
            "set_settings" => {
                let mut cur = serde_json::to_value(&self.settings).map_err(|e| e.to_string())?;
                let changes = args.as_object().ok_or("pass the settings to change as an object")?;
                for (k, v) in changes {
                    if cur.get(k).is_none() {
                        return Err(format!("no setting {k}"));
                    }
                    cur[k] = v.clone();
                }
                let new: config::Settings = serde_json::from_value(cur).map_err(|e| e.to_string())?;
                let old = std::mem::replace(&mut self.settings, new);
                if old.font != self.settings.font {
                    self.font_applied = install_fonts(ctx, &self.settings.font);
                }
                if old.ui_scale != self.settings.ui_scale {
                    ctx.set_zoom_factor(self.settings.ui_scale.clamp(0.5, 2.0));
                }
                if old.lang != self.settings.lang {
                    i18n::set(i18n::resolve(&self.settings.lang));
                }
                if old.theme != self.settings.theme || old.accent != self.settings.accent {
                    palette::apply(&self.settings.theme, self.settings.accent.map(rgb32));
                    apply_style(ctx);
                }
                // The statusline hook is installed by its own settings checkbox only.
                self.settings.ctx_hook = old.ctx_hook;
                self.settings.prev_statusline = old.prev_statusline;
                self.persist();
                ctx.request_repaint();
                serde_json::to_value(&self.settings).map_err(|e| e.to_string())
            },
            _ => Err(format!("unknown op {op}")),
        }
    }

    pub(super) fn start_mcp_install(&mut self, ctx: &egui::Context) {
        self.mcp.busy = true;
        self.mcp.status = None;
        let (tx, ctx, cmd) = (self.mcp_tx.clone(), ctx.clone(), self.settings.claude_cmd.clone());
        std::thread::spawn(move || {
            let _ = tx.send(McpMsg::Done(mcp::install(&cmd)));
            ctx.request_repaint();
        });
    }

    pub(super) fn mcp_status_ui(&self, ui: &mut egui::Ui) {
        match &self.mcp.status {
            Some(Ok(_)) => {
                ui.label(
                    RichText::new(tr(
                        "Готово. Новые сессии Claude Code увидят инструменты kip.",
                        "Done. New Claude Code sessions will see kip's tools.",
                    ))
                    .size(11.0)
                    .color(GIT_ADD),
                );
            },
            Some(Err(e)) => {
                ui.label(RichText::new(e).size(10.5).color(GIT_DEL));
            },
            None if self.mcp.busy => {
                ui.label(RichText::new(tr("устанавливаю...", "installing...")).size(10.5).color(palette::text_faint()));
            },
            None => {},
        }
    }

    pub(super) fn mcp_offer(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut install = false;
        let mut later = false;
        egui::Window::new(tr("Подключить kip к Claude Code", "Connect kip to Claude Code"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-16.0, -48.0))
            .show(ctx, |ui| {
                ui.set_max_width(380.0);
                ui.label(tr(
                    "Рекомендуем поставить MCP-сервер kip. Тогда Claude в любой сессии сможет управлять kip по вашей просьбе:",
                    "We recommend installing kip's MCP server. Claude in any session can then manage kip when you ask:",
                ));
                for line in [
                    tr("видеть все сессии, их папки, статус и % контекста", "see all sessions, their folders, status and context %"),
                    tr("открывать сессии в нужной папке и окне, сразу с Claude", "open sessions in a folder and window, with Claude started"),
                    tr("закрывать, усыплять и будить сессии", "close, suspend and wake sessions"),
                    tr("переименовывать, раскладывать по группам и окнам, закреплять", "rename, group, move between windows, pin"),
                    tr("читать экран и переписку сессии и делать саммари", "read a session's screen and conversation and summarize it"),
                    tr("чистить давно простаивающие сессии", "clean up long-idle sessions"),
                ] {
                    ui.label(RichText::new(format!("- {line}")).size(11.5).color(palette::text_faint()));
                }
                ui.label(
                    RichText::new(tr(
                        "Компактно: 24 инструмента, около 2.1 тыс. токенов контекста. Ставится одной командой: claude mcp add -s user kip. Печатать в чужие сессии MCP не может, пока вы не разрешите это в настройках.",
                        "Compact: 24 tools, about 2.1k tokens of context. Installs with one command: claude mcp add -s user kip. MCP cannot type into sessions unless you allow it in Settings.",
                    ))
                    .size(10.5)
                    .color(palette::text_faint()),
                );
                ui.add_space(4.0);
                ui.checkbox(&mut self.mcp.dont_remind, tr("Больше не напоминать", "Don't remind me again"));
                ui.horizontal(|ui| {
                    let ok = matches!(self.mcp.status, Some(Ok(_)));
                    if !ok && ui.add_enabled(!self.mcp.busy, egui::Button::new(tr("Установить", "Install"))).clicked() {
                        install = true;
                    }
                    if ui.button(if ok { tr("Закрыть", "Close") } else { tr("Не сейчас", "Not now") }).clicked() {
                        later = true;
                    }
                });
                self.mcp_status_ui(ui);
            });
        if install {
            self.start_mcp_install(ctx);
        }
        if !open || later {
            self.mcp.show = false;
            if self.mcp.dont_remind {
                self.settings.mcp_offer_off = true;
                self.persist();
            }
        }
    }
}
