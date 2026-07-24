// Release builds on Windows are GUI apps: suppress the extra console window.
// Debug keeps it so panics and logs stay visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod ctx_index;
mod i18n;
mod plat;
mod palette;
mod session;
mod term_view;
mod update;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use alacritty_terminal::event::{Event as TermEvent, Notify, WindowSize};
use alacritty_terminal::term::TermMode;
use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame,
    Key, Layout, Margin, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, Vec2, Visuals,
};

use config::{load_state, save_state, AppState, SavedSession, Settings};
use i18n::tr;
use session::{poll_git, spawn_live, EventProxy, GitStats, Phase, Session};

const TICK: Duration = Duration::from_secs(2);
const GIT_INTERVAL: Duration = Duration::from_secs(7);
const BUSY_NOTIFY_MIN: Duration = Duration::from_secs(5);

const TXT: Color32 = Color32::from_rgb(0xc4, 0xc4, 0xc4);
const TXT_DIM: Color32 = Color32::from_rgb(0x7a, 0x7a, 0x7a);
const TXT_FAINT: Color32 = Color32::from_rgb(0x5a, 0x5a, 0x5a);
const GIT_ADD: Color32 = Color32::from_rgb(0x8f, 0xb5, 0x7a);
const GIT_DEL: Color32 = Color32::from_rgb(0xc4, 0x7a, 0x7a);
const DOT_BUSY: Color32 = Color32::from_rgb(0x8f, 0xb5, 0x7a);
const DOT_LIVE: Color32 = Color32::from_rgb(0x8a, 0x8a, 0x8a);
const DOT_EXITED: Color32 = Color32::from_rgb(0xb0, 0x70, 0x70);
const UNREAD: Color32 = Color32::from_rgb(0x9c, 0xb5, 0xcc);
const ORANGE: Color32 = Color32::from_rgb(0xd4, 0xa0, 0x5a);
const POPUP_BG: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x1f);
const POPUP_STROKE: Color32 = Color32::from_rgb(0x35, 0x35, 0x35);

fn main() -> eframe::Result {
    // If kip itself was launched from a Claude Code session, its CLAUDE* markers
    // leak into our shells and a claude started there thinks it is a child session
    // and disables transcript saving (breaking resume and context tracking).
    let claude_vars: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("CLAUDE"))
        .collect();
    for k in claude_vars {
        // Safe: nothing else is running yet.
        unsafe { std::env::remove_var(k) };
    }

    // Clipboard-image pastes land in temp as kip-paste-*.png; sweep old ones.
    plat::sweep_paste_temp();

    // 256x256 raw RGBA, matching resources/icon_1024.png. Sets the taskbar/dock
    // icon at runtime (Windows has no embedded exe icon; macOS .app uses the icns).
    let icon = egui::IconData {
        rgba: include_bytes!("../resources/icon_256.rgba").to_vec(),
        width: 256,
        height: 256,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("kip")
            .with_icon(std::sync::Arc::new(icon))
            .with_inner_size([1160.0, 740.0])
            .with_min_inner_size([680.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native("kip", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(update::Release),
    Working,
    Failed(String),
}

enum Act {
    Select(u64),
    NewSame,
    NewPick,
    Suspend(u64),
    /// bool = resume the saved Claude session (vs plain shell).
    Resume(u64, bool),
    /// Open a fresh plain terminal in this session's directory, leaving the
    /// session (and its snapshot history) untouched.
    OpenTerminal(u64),
    /// Cmd+W: suspend a live session, remove a frozen one.
    Close(u64),
    /// X button / "Удалить": always removes, killing a live session.
    Remove(u64),
    ToggleAwake(u64),
    Settings,
}

struct App {
    settings: Settings,
    sessions: Vec<Session>,
    active: Option<u64>,
    next_id: u64,
    ev_tx: Sender<(u64, TermEvent)>,
    ev_rx: Receiver<(u64, TermEvent)>,
    git_tx: Sender<(u64, PathBuf, GitStats)>,
    git_rx: Receiver<(u64, PathBuf, GitStats)>,
    /// Mirrors `active` for PTY reader threads (repaint coalescing).
    active_shared: Arc<AtomicU64>,
    ctx_tx: Sender<(u64, session::ClaudeInfo)>,
    ctx_rx: Receiver<(u64, session::ClaudeInfo)>,
    /// Context-% index (single source of truth for badges) and its channel.
    ctx_index: ctx_index::CtxIndex,
    ctxi_tx: Sender<ctx_index::CtxMsg>,
    ctxi_rx: Receiver<ctx_index::CtxMsg>,
    jsonl_map: ctx_index::SharedMap,
    /// Own 500ms cadence of the context poller, independent of TICK.
    last_ctx_stat: Option<Instant>,
    hook_error: Option<String>,
    update_state: UpdateState,
    upd_tx: Sender<update::UpdateMsg>,
    upd_rx: Receiver<update::UpdateMsg>,
    settings_open: bool,
    last_tick: Instant,
    /// When the last background update check ran.
    last_update_check: Instant,
    /// Command editor pinned under the terminal.
    cmd_input: String,
    /// The typed text used as the history filter (nav-fill does not change it).
    hist_query: String,
    hist_sel: Option<usize>,
    hist_dismissed: bool,
    hist_forced: bool,
    /// Shell history (from $HISTFILE) merged with commands sent this run, oldest first.
    history: Vec<String>,
    /// Lowercased mirror of `history`, so per-frame filtering does not allocate.
    history_lc: Vec<String>,
    /// Commands sent from this app in this run (the shell persists them itself).
    session_cmds: Vec<String>,
    hist_mtime: Option<SystemTime>,
    last_hist_check: Option<Instant>,
    /// Directory switcher popup over the path chip.
    dir_open: bool,
    dir_query: String,
    dir_path: PathBuf,
    /// Unfiltered subdirs of dir_path, refreshed at most every 2s while the popup is open.
    dir_cache: Vec<String>,
    dir_cache_at: Option<(PathBuf, Instant)>,
    chip_rect: Option<Rect>,
    stats_tx: Sender<plat::SysStats>,
    stats_rx: Receiver<plat::SysStats>,
    stats: Option<plat::SysStats>,
    stats_at: Option<Instant>,
    stats_inflight: bool,
    /// Popup rect from the previous frame, to keep it open while hovered.
    stats_rect: Option<Rect>,
    /// Last measured terminal cell size in px, used for PTY pixel hints.
    cell: (u16, u16),
    /// Last measured grid size, used as the initial size for new PTYs.
    grid: (u16, u16),
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        let state = load_state();
        i18n::set(i18n::resolve(&state.settings.lang));
        palette::apply(&state.settings.theme, state.settings.accent.map(rgb32));
        apply_style(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(state.settings.ui_scale.clamp(0.5, 2.0));
        let (ev_tx, ev_rx) = mpsc::channel();
        let (git_tx, git_rx) = mpsc::channel();
        let (ctx_tx, ctx_rx) = mpsc::channel();
        let (ctxi_tx, ctxi_rx) = mpsc::channel();
        let (stats_tx, stats_rx) = mpsc::channel();
        let (upd_tx, upd_rx) = mpsc::channel();
        let jsonl_map: ctx_index::SharedMap = Default::default();
        ctx_index::spawn_initial_scan(jsonl_map.clone());
        ctx_index::sweep();
        #[cfg(not(windows))]
        if state.settings.ctx_hook {
            // Keep the installed hook script current across kip updates.
            let _ = ctx_index::write_hook_script();
        }
        let mut app = App {
            settings: state.settings,
            sessions: Vec::new(),
            active: None,
            next_id: 1,
            ev_tx,
            ev_rx,
            git_tx,
            git_rx,
            ctx_tx,
            ctx_rx,
            ctx_index: Default::default(),
            ctxi_tx,
            ctxi_rx,
            jsonl_map,
            last_ctx_stat: None,
            hook_error: None,
            update_state: UpdateState::Idle,
            upd_tx,
            upd_rx,
            active_shared: Arc::new(AtomicU64::new(0)),
            settings_open: false,
            last_tick: Instant::now(),
            last_update_check: Instant::now(),
            cmd_input: String::new(),
            hist_query: String::new(),
            hist_sel: None,
            hist_dismissed: false,
            hist_forced: false,
            history: Vec::new(),
            history_lc: Vec::new(),
            session_cmds: Vec::new(),
            hist_mtime: None,
            last_hist_check: None,
            dir_open: false,
            dir_query: String::new(),
            dir_path: dirs::home_dir().unwrap_or_else(|| "/".into()),
            dir_cache: Vec::new(),
            dir_cache_at: None,
            chip_rect: None,
            stats_tx,
            stats_rx,
            stats: None,
            stats_at: None,
            stats_inflight: false,
            stats_rect: None,
            cell: (8, 17),
            grid: (100, 28),
        };
        for saved in state.sessions {
            let id = app.next_id;
            app.next_id += 1;
            app.sessions.push(Session::from_saved(saved, id));
        }
        if app.sessions.is_empty() {
            app.spawn(dirs::home_dir().unwrap_or_else(|| "/".into()), None, &cc.egui_ctx);
        }
        app.active = app.sessions.first().map(|s| s.id);
        // Show saved sessions' context load right away, before claude ever runs.
        for s in &app.sessions {
            app.poll_ctx_now(s, &cc.egui_ctx);
        }
        // Clean up any leftover from a prior update, then check for a new one.
        update::cleanup();
        app.update_state = UpdateState::Checking;
        update::check(app.upd_tx.clone(), cc.egui_ctx.clone());
        app
    }

    fn drain_update(&mut self) {
        while let Ok(msg) = self.upd_rx.try_recv() {
            match msg {
                update::UpdateMsg::Checked(Ok(Some(r))) => {
                    self.update_state = UpdateState::Available(r)
                },
                update::UpdateMsg::Checked(Ok(None)) => self.update_state = UpdateState::UpToDate,
                update::UpdateMsg::Checked(Err(e)) | update::UpdateMsg::Applied(Err(e)) => {
                    self.update_state = UpdateState::Failed(e)
                },
                update::UpdateMsg::Applied(Ok(())) => {},
            }
        }
    }

    /// Immediate context poll by the saved session id (no live claude needed).
    fn poll_ctx_now(&self, s: &Session, ctx: &egui::Context) {
        if let Some(sid) = &s.claude_session_id {
            ctx_index::lookup(
                sid.clone(),
                s.cwd.clone(),
                self.jsonl_map.clone(),
                self.ctxi_tx.clone(),
                ctx.clone(),
            );
            session::poll_claude(
                s.id,
                s.cwd.clone(),
                s.spawned_at,
                s.claude_session_id.clone(),
                None,
                self.ctx_tx.clone(),
                ctx.clone(),
            );
        }
    }

    fn persist(&self) {
        save_state(&AppState {
            settings: self.settings.clone(),
            sessions: self.sessions.iter().map(|s| s.to_saved()).collect(),
        });
    }

    fn push_history(&mut self, cmd: &str) {
        self.session_cmds.retain(|h| h != cmd);
        self.session_cmds.push(cmd.to_string());
        if self.session_cmds.len() > 5000 {
            let cut = self.session_cmds.len() - 5000;
            self.session_cmds.drain(..cut);
        }
        // History is effectively unlimited (like a shell). Keep history_lc in
        // lockstep instead of rebuilding it fully - that would be O(n) per submit.
        if let Some(pos) = self.history.iter().position(|h| h == cmd) {
            self.history.remove(pos);
            self.history_lc.remove(pos);
        }
        self.history.push(cmd.to_string());
        self.history_lc.push(cmd.to_lowercase());
        if self.history.len() > 200_000 {
            let cut = self.history.len() - 190_000;
            self.history.drain(..cut);
            self.history_lc.drain(..cut);
        }
    }

    /// Re-read the shell history file when it changes (throttled).
    fn refresh_history(&mut self) {
        let now = Instant::now();
        if self.last_hist_check.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            return;
        }
        self.last_hist_check = Some(now);
        let mtime = shell_history_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        if mtime == self.hist_mtime && !self.history.is_empty() {
            return;
        }
        self.hist_mtime = mtime;
        let mut hist = load_shell_history();
        for cmd in &self.session_cmds {
            hist.retain(|h| h != cmd);
            hist.push(cmd.clone());
        }
        self.history = hist;
        self.history_lc = self.history.iter().map(|h| h.to_lowercase()).collect();
    }

    /// Insert file path text where input currently goes: the command editor at
    /// the prompt, or straight into the PTY while a program runs.
    fn insert_paths(&mut self, text: String) {
        let Some(idx) = self.active_idx() else { return };
        let s = &mut self.sessions[idx];
        let Some(live) = s.live() else { return };
        let busy = plat::foreground_pgid(live.master_fd, live.shell_pid).is_some_and(|pg| pg != live.shell_pid);
        if busy {
            let bracketed = live.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
            let mut out = Vec::new();
            if bracketed {
                out.extend_from_slice(b"\x1b[200~");
            }
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b" ");
            if bracketed {
                out.extend_from_slice(b"\x1b[201~");
            }
            live.notifier.notify(out);
            s.last_activity = Instant::now();
        } else {
            if !self.cmd_input.is_empty() && !self.cmd_input.ends_with(' ') {
                self.cmd_input.push(' ');
            }
            self.cmd_input.push_str(&text);
            self.cmd_input.push(' ');
            self.hist_query = self.cmd_input.clone();
        }
    }

    fn send_command_to(&mut self, idx: usize, cmd: &str, ctx: &egui::Context) {
        self.push_history(cmd);
        let mut bound = false;
        let s = &mut self.sessions[idx];
        if let Some(live) = s.live() {
            live.notifier.notify(format!("{cmd}\r").into_bytes());
            s.last_activity = Instant::now();
            s.pending_cmd = Some(cmd.to_string());
            // A resume command names its session up front: bind it and show its
            // context immediately instead of waiting for claude to boot.
            if let Some(hint) = session::parse_resume_hint(cmd) {
                let sid = match hint {
                    session::ResumeHint::Sid(sid) => Some(sid),
                    session::ResumeHint::Latest => session::detect_claude_session(&s.cwd, None),
                };
                if let Some(sid) = sid {
                    s.claude_session_id = Some(sid);
                    s.saw_claude = true;
                    s.last_ctx_poll = None;
                    s.burst_until = Some(Instant::now() + Duration::from_secs(3));
                    bound = true;
                }
            }
        }
        if bound {
            self.poll_ctx_now(&self.sessions[idx], ctx);
        }
    }

    fn remove_session(&mut self, idx: usize) {
        let id = self.sessions[idx].id;
        self.sessions.remove(idx);
        if self.active == Some(id) {
            let next = idx.min(self.sessions.len().saturating_sub(1));
            self.set_active(self.sessions.get(next).map(|s| s.id));
        }
        self.persist();
    }

    /// Switch the active session, dropping per-session UI state (popup, input, history nav).
    fn set_active(&mut self, id: Option<u64>) {
        self.active = id;
        self.dir_open = false;
        self.cmd_input.clear();
        self.hist_query.clear();
        self.hist_sel = None;
        self.hist_forced = false;
        self.hist_dismissed = false;
    }

    fn idx_of(&self, id: u64) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    fn active_idx(&self) -> Option<usize> {
        self.active.and_then(|id| self.idx_of(id))
    }

    /// Current directory of the active session's shell, for spawning siblings.
    fn active_cwd(&self) -> PathBuf {
        if let Some(idx) = self.active_idx() {
            let s = &self.sessions[idx];
            if let Some(live) = s.live() {
                if let Some(cwd) = plat::pid_cwd(live.shell_pid) {
                    return cwd;
                }
            }
            return s.cwd.clone();
        }
        dirs::home_dir().unwrap_or_else(|| "/".into())
    }

    fn spawn(&mut self, cwd: PathBuf, command: Option<String>, ctx: &egui::Context) {
        let id = self.next_id;
        self.next_id += 1;
        let mut s = Session::from_saved(
            SavedSession {
                cwd,
                claude_session_id: None,
                claude_title: None,
                skip_permissions: self.settings.skip_permissions_default,
                keep_awake: false,
                snapshot: None,
            },
            id,
        );
        self.attach_live(&mut s, command, ctx);
        self.sessions.push(s);
        self.set_active(Some(id));
        self.persist();
    }

    fn attach_live(&self, s: &mut Session, command: Option<String>, ctx: &egui::Context) {
        let proxy = EventProxy {
            id: s.id,
            tx: self.ev_tx.clone(),
            ctx: ctx.clone(),
            active: self.active_shared.clone(),
        };
        match spawn_live(s.id, &s.cwd, command, &self.settings, proxy, self.grid.0, self.grid.1, self.cell) {
            Ok(live) => {
                s.phase = Phase::Live(live);
                s.spawned_at = SystemTime::now();
                s.last_activity = Instant::now();
                s.busy = false;
                s.busy_since = None;
                s.unread = false;
                s.last_git_poll = None;
                s.fg_name = None;
                s.fg_is_claude = false;
                s.claude_pid = None;
                s.pending_cmd = None;
                s.running_cmd = None;
                s.last_ctx_poll = None;
            },
            Err(e) => {
                s.phase = Phase::Exited(None);
                s.snapshot =
                    Some(format!("{}: {e}", tr("Не удалось запустить терминал", "Failed to start terminal")));
            },
        }
    }

    fn resume(&mut self, id: u64, with_claude: bool, ctx: &egui::Context) {
        let Some(idx) = self.idx_of(id) else { return };
        let base = if with_claude { self.sessions[idx].resume_command(&self.settings) } else { None };
        // Drop to a fresh shell when claude exits instead of killing the tab.
        #[cfg(not(windows))]
        let command = base.as_ref().map(|cmd| format!("{cmd} ; exec ${{SHELL:-/bin/zsh}} -il"));
        #[cfg(windows)]
        let command = base.as_ref().map(|cmd| format!("{cmd}; powershell.exe -NoLogo"));
        let mut s = std::mem::replace(&mut self.sessions[idx], Session::from_saved(SavedSession::default(), 0));
        self.attach_live(&mut s, command, ctx);
        s.pending_cmd = base.clone();
        if base.is_some() {
            s.burst_until = Some(Instant::now() + Duration::from_secs(3));
        }
        self.sessions[idx] = s;
        self.set_active(Some(id));
        self.poll_ctx_now(&self.sessions[idx], ctx);
    }

    fn apply(&mut self, acts: Vec<Act>, ctx: &egui::Context) {
        for act in acts {
            match act {
                Act::Select(id) => {
                    self.set_active(Some(id));
                    if let Some(idx) = self.idx_of(id) {
                        self.sessions[idx].unread = false;
                        self.sessions[idx].last_git_poll = None;
                    }
                },
                Act::NewSame => {
                    let cwd = self.active_cwd();
                    self.spawn(cwd, None, ctx);
                },
                Act::NewPick => {
                    let start = self.active_cwd();
                    if let Some(dir) = rfd::FileDialog::new().set_directory(&start).pick_folder() {
                        self.spawn(dir, None, ctx);
                    }
                },
                Act::Suspend(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        self.sessions[idx].suspend();
                        self.persist();
                    }
                },
                Act::Resume(id, with_claude) => {
                    self.resume(id, with_claude, ctx);
                    self.persist();
                },
                Act::OpenTerminal(id) => {
                    // Keep the session suspended so its Claude history is not lost;
                    // just open a new plain terminal in the same directory.
                    if let Some(idx) = self.idx_of(id) {
                        let cwd = self.sessions[idx].cwd.clone();
                        self.spawn(cwd, None, ctx);
                        self.persist();
                    }
                },
                Act::Close(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        // Guards against reflexive Cmd+W killing a working agent.
                        if matches!(self.sessions[idx].phase, Phase::Live(_)) {
                            self.sessions[idx].suspend();
                            self.persist();
                        } else {
                            self.remove_session(idx);
                        }
                    }
                },
                Act::Remove(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        self.remove_session(idx);
                    }
                },
                Act::ToggleAwake(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        self.sessions[idx].keep_awake = !self.sessions[idx].keep_awake;
                        self.persist();
                    }
                },
                Act::Settings => self.settings_open = !self.settings_open,
            }
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        let mut persist = false;
        while let Ok((id, ev)) = self.ev_rx.try_recv() {
            let Some(idx) = self.idx_of(id) else { continue };
            let is_active = self.active == Some(id);
            let s = &mut self.sessions[idx];
            match ev {
                TermEvent::Wakeup => {
                    s.last_activity = Instant::now();
                    if !is_active {
                        s.unread = true;
                    }
                    // First output of a fresh command: spot claude right away
                    // instead of waiting for the housekeeping tick. Cooldown keeps
                    // heavy non-claude output from probing on every chunk.
                    if !s.busy
                        && !s.fg_is_claude
                        && s.last_fg_probe.is_none_or(|t| t.elapsed() >= Duration::from_millis(200))
                    {
                        s.last_fg_probe = Some(Instant::now());
                        if let Some(live) = s.live() {
                            let fg = plat::foreground_pgid(live.master_fd, live.shell_pid);
                            if fg.is_some_and(|pg| pg != live.shell_pid)
                                && fg.is_some_and(plat::is_claude_proc)
                            {
                                s.fg_is_claude = true;
                                s.claude_pid = fg;
                                s.saw_claude = true;
                                s.last_ctx_poll = None;
                                s.burst_until = Some(Instant::now() + Duration::from_secs(3));
                            }
                        }
                    }
                },
                TermEvent::Bell => {
                    if !is_active || !focused {
                        s.unread = true;
                        if self.settings.notify_bell {
                            let name = s.display_name();
                            plat::notify("kip", &format!("{name}: {}", tr("сигнал терминала", "terminal bell")), self.settings.notify_sound);
                        }
                    }
                },
                TermEvent::Title(t) => s.title = t,
                TermEvent::ResetTitle => s.title.clear(),
                TermEvent::ClipboardStore(_, text) => ctx.copy_text(text),
                // OSC 52 read is answered with an empty string on purpose: it would
                // let any program in the terminal silently read the user's clipboard.
                TermEvent::ClipboardLoad(_, fmt) => {
                    if let Some(live) = s.live() {
                        live.notifier.notify(fmt("").into_bytes());
                    }
                },
                TermEvent::ColorRequest(i, fmt) => {
                    if let Some(live) = s.live() {
                        let rgb = {
                            let term = live.term.lock();
                            palette::query_color(i, term.colors())
                        };
                        live.notifier.notify(fmt(rgb).into_bytes());
                    }
                },
                TermEvent::PtyWrite(text) => {
                    if let Some(live) = s.live() {
                        live.notifier.notify(text.into_bytes());
                    }
                },
                TermEvent::TextAreaSizeRequest(fmt) => {
                    if let Some(live) = s.live() {
                        let ws = WindowSize {
                            num_lines: live.rows,
                            num_cols: live.cols,
                            cell_width: self.cell.0,
                            cell_height: self.cell.1,
                        };
                        live.notifier.notify(fmt(ws).into_bytes());
                    }
                },
                TermEvent::ChildExit(status) => {
                    s.finalize_exit(status.code());
                    persist = true;
                },
                TermEvent::CursorBlinkingChange | TermEvent::MouseCursorDirty | TermEvent::Exit => {},
            }
        }
        if persist {
            self.persist();
        }
    }

    fn drain_ctx(&mut self, ctx: &egui::Context) {
        let mut lookups: Vec<(String, PathBuf)> = Vec::new();
        while let Ok((id, info)) = self.ctx_rx.try_recv() {
            if let Some(idx) = self.idx_of(id) {
                let s = &mut self.sessions[idx];
                if info.name.is_some() {
                    s.claude_title = info.name;
                }
                // Claude found behind a wrapper: adopt right away, don't wait
                // for the next housekeeping tick to re-validate.
                if let Some(cp) = info.claude_pid {
                    if s.claude_pid != Some(cp) {
                        s.claude_pid = Some(cp);
                        s.fg_is_claude = true;
                        s.saw_claude = true;
                        s.burst_until = Some(Instant::now() + Duration::from_secs(3));
                    }
                }
                if let Some(sid) = info.session_id {
                    if s.claude_session_id.as_deref() != Some(sid.as_str()) {
                        s.claude_session_id = Some(sid.clone());
                        s.ctx_stat.jsonl_path = None;
                        s.ctx_stat.path_sid = None;
                        lookups.push((sid, s.cwd.clone()));
                    }
                }
            }
        }
        for (sid, cwd) in lookups {
            ctx_index::lookup(sid, cwd, self.jsonl_map.clone(), self.ctxi_tx.clone(), ctx.clone());
        }
    }

    fn drain_ctx_index(&mut self) {
        while let Ok(msg) = self.ctxi_rx.try_recv() {
            match msg {
                ctx_index::CtxMsg::Update(u) => self.ctx_index.apply(u),
                ctx_index::CtxMsg::Rebind { session, update } => {
                    // The tab's claude switched sessions (/resume inside claude).
                    if let Some(idx) = self.idx_of(session) {
                        let s = &mut self.sessions[idx];
                        if s.fg_is_claude {
                            s.claude_session_id = Some(update.sid.clone());
                            s.ctx_stat = Default::default();
                        }
                    }
                    self.ctx_index.apply(update);
                },
            }
        }
    }

    fn drain_git(&mut self) {
        while let Ok((id, cwd, stats)) = self.git_rx.try_recv() {
            if let Some(idx) = self.idx_of(id) {
                let s = &mut self.sessions[idx];
                s.git_inflight = false;
                // Drop results that raced a cwd change.
                if s.cwd == cwd {
                    s.git = Some(stats);
                }
            }
        }
    }

    fn housekeeping(&mut self, ctx: &egui::Context) {
        let now = Instant::now();

        if now.duration_since(self.last_tick) >= TICK {
            self.last_tick = now;
            // Background update check a couple of times a day, on top of the
            // one at launch. Skip while a check/update is already in flight or
            // an update is already offered.
            if self.last_update_check.elapsed() >= Duration::from_secs(12 * 3600)
                && matches!(
                    self.update_state,
                    UpdateState::Idle | UpdateState::UpToDate | UpdateState::Failed(_)
                )
            {
                self.last_update_check = now;
                self.update_state = UpdateState::Checking;
                update::check(self.upd_tx.clone(), ctx.clone());
            }
            let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
            let active = self.active;
            let idle_limit = self.settings.idle_suspend_min as u64 * 60;
            let mut suspend_any = false;

            for s in &mut self.sessions {
                let (master_fd, shell_pid) = match &s.phase {
                    Phase::Live(l) => (l.master_fd, l.shell_pid),
                    _ => continue,
                };
                let fg = plat::foreground_pgid(master_fd, shell_pid);
                let busy_now = fg.is_some_and(|pg| pg != shell_pid);
                let was_claude = s.fg_is_claude;
                s.fg_name = if busy_now { fg.and_then(plat::process_name) } else { None };
                let leader_claude = busy_now && fg.is_some_and(plat::is_claude_proc);
                if leader_claude {
                    s.claude_pid = fg;
                } else if !busy_now {
                    s.claude_pid = None;
                } else if s.claude_pid.is_some_and(|cp| !plat::is_claude_proc(cp)) {
                    // Wrapper case: the claude child exited (or its pid was reused).
                    s.claude_pid = None;
                }
                s.fg_is_claude = leader_claude || (busy_now && s.claude_pid.is_some());
                let is_claude = s.fg_is_claude;
                if is_claude {
                    s.saw_claude = true;
                    if !was_claude {
                        s.last_ctx_poll = None;
                        s.burst_until = Some(Instant::now() + Duration::from_secs(3));
                    }
                }
                // Name/id capture while claude runs.
                if busy_now
                    && is_claude
                    && s.last_ctx_poll.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(8))
                {
                    s.last_ctx_poll = Some(now);
                    session::poll_claude(
                        s.id,
                        s.cwd.clone(),
                        s.spawned_at,
                        s.claude_session_id.clone(),
                        fg,
                        self.ctx_tx.clone(),
                        ctx.clone(),
                    );
                }
                if busy_now && !s.busy {
                    s.busy = true;
                    s.busy_since = Some(now);
                    s.running_cmd = s.pending_cmd.take();
                } else if !busy_now && s.busy {
                    s.busy = false;
                    s.running_cmd = None;
                    let dur = s.busy_since.take().map(|t| now - t).unwrap_or_default();
                    if dur >= BUSY_NOTIFY_MIN && (active != Some(s.id) || !focused) {
                        s.unread = true;
                        if self.settings.notify_job_done {
                            plat::notify(
                                "kip",
                                &format!("{}: {} ({})", s.display_name(), tr("агент завершил работу", "agent finished"), fmt_dur(dur)),
                                self.settings.notify_sound,
                            );
                        }
                    }
                }
                if let Some(cwd) = plat::pid_cwd(shell_pid) {
                    if cwd != s.cwd {
                        s.cwd = cwd;
                        s.git = None;
                        s.last_git_poll = None;
                    }
                }
                // Idle = no PTY output and no user interaction. An interactive claude
                // stays foreground even while it sleeps at its prompt, so busy alone is
                // not activity - but a silent non-claude job (make, rsync) must survive.
                if idle_limit > 0
                    && !s.keep_awake
                    && (!busy_now || is_claude)
                    && s.last_activity.elapsed().as_secs() >= idle_limit
                {
                    s.suspend();
                    suspend_any = true;
                }
            }
            if suspend_any {
                self.persist();
            }

            if focused {
                if let Some(idx) = self.active_idx() {
                    let s = &mut self.sessions[idx];
                    let due = s.last_git_poll.is_none_or(|t| now.duration_since(t) >= GIT_INTERVAL);
                    let stuck = s.last_git_poll.is_some_and(|t| now.duration_since(t) >= Duration::from_secs(60));
                    if due && (!s.git_inflight || stuck) {
                        s.git_inflight = true;
                        s.last_git_poll = Some(now);
                        poll_git(s.id, s.cwd.clone(), self.git_tx.clone(), ctx.clone());
                    }
                }
            }
        }

        self.ctx_poll(now, ctx);

        if self.sessions.iter().any(|s| s.live().is_some()) {
            ctx.request_repaint_after(TICK);
        }
    }

    /// Live context poll: own 500ms cadence (300ms during a post-start burst),
    /// NOT under the 2s TICK. Stat-only on the UI thread; any changed file is
    /// read and parsed in a spawned thread that reports via the ctxi channel.
    fn ctx_poll(&mut self, now: Instant, ctx: &egui::Context) {
        let burst_any = self.sessions.iter().any(|s| s.burst_until.is_some_and(|t| now < t));
        let interval =
            if burst_any { Duration::from_millis(300) } else { Duration::from_millis(500) };
        if self.last_ctx_stat.is_some_and(|t| now.duration_since(t) < interval) {
            if burst_any || self.sessions.iter().any(|s| s.fg_is_claude) {
                ctx.request_repaint_after(interval);
            }
            return;
        }
        self.last_ctx_stat = Some(now);
        let tx = self.ctxi_tx.clone();
        let meta_tx = self.ctx_tx.clone();
        let map = self.jsonl_map.clone();
        let mut watching = false;

        for s in &mut self.sessions {
            let bursting = s.burst_until.is_some_and(|t| now < t);
            let (master_fd, shell_pid) = match &s.phase {
                Phase::Live(l) => (l.master_fd, l.shell_pid),
                _ => continue,
            };
            let fg = plat::foreground_pgid(master_fd, shell_pid).filter(|pg| *pg != shell_pid);
            // Busy but not (yet) claude: a wrapper (cchb etc) may be about to
            // spawn claude as a child - look for it in the background.
            if fg.is_some() && !s.fg_is_claude {
                if s.ctx_stat.last_finder.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(2)) {
                    s.ctx_stat.last_finder = Some(now);
                    session::poll_claude(
                        s.id,
                        s.cwd.clone(),
                        s.spawned_at,
                        s.claude_session_id.clone(),
                        fg,
                        meta_tx.clone(),
                        ctx.clone(),
                    );
                }
            }
            if !s.fg_is_claude && !bursting {
                continue;
            }
            watching = true;

            if let Some(cp) = s.claude_pid.or(fg) {
                // Universal fast binding: claude writes its own sessions/<pid>.json
                // for ANY launch method (picker, --continue, wrappers) - the moment
                // it changes, read the sessionId. No hook needed.
                if let Some(p) = session::meta_path(cp) {
                    let mt = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok());
                    if mt.is_some() && mt != s.ctx_stat.meta_mtime {
                        s.ctx_stat.meta_mtime = mt;
                        let min = s.busy_since.map(|t| SystemTime::now() - t.elapsed());
                        session::spawn_meta_read(s.id, cp, min, meta_tx.clone(), ctx.clone());
                    }
                }
                // by-pid hook snapshot: exact % for THIS tab + rebinding.
                if let Some(p) = ctx_index::by_pid_path(cp) {
                    let mt = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok());
                    if mt.is_some() && mt != s.ctx_stat.bypid_mtime {
                        s.ctx_stat.bypid_mtime = mt;
                        ctx_index::spawn_bypid_read(
                            s.id,
                            cp,
                            s.claude_session_id.clone(),
                            tx.clone(),
                            ctx.clone(),
                        );
                    }
                }
            }

            let Some(sid) = s.claude_session_id.clone() else { continue };
            // by-sid snapshot from the hook.
            if let Some(p) = ctx_index::by_sid_path(&sid) {
                let mt = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok());
                if mt.is_some() && mt != s.ctx_stat.snap_mtime {
                    s.ctx_stat.snap_mtime = mt;
                    ctx_index::spawn_sid_read(sid.clone(), tx.clone(), ctx.clone());
                }
            }
            // Transcript jump: the estimate lands before the next hook tick.
            if s.ctx_stat.path_sid.as_deref() != Some(sid.as_str()) {
                let hit = map.try_lock().ok().and_then(|m| m.map.get(&sid).cloned());
                if let Some(p) = hit {
                    s.ctx_stat.jsonl_path = Some(p);
                    s.ctx_stat.path_sid = Some(sid.clone());
                    s.ctx_stat.jsonl_state = None;
                }
            }
            match s.ctx_stat.jsonl_path.clone() {
                Some(p) if s.ctx_stat.path_sid.as_deref() == Some(sid.as_str()) => {
                    let st = std::fs::metadata(&p)
                        .ok()
                        .and_then(|m| Some((m.modified().ok()?, m.len())));
                    if st.is_some() && st != s.ctx_stat.jsonl_state {
                        s.ctx_stat.jsonl_state = st;
                        ctx_index::spawn_estimate(sid.clone(), p, tx.clone(), ctx.clone());
                    }
                },
                _ => {
                    // Unknown path: full background lookup (map rescan inside
                    // is throttled), at most every 5s per session.
                    if s.ctx_stat
                        .last_resolve
                        .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(5))
                    {
                        s.ctx_stat.last_resolve = Some(now);
                        ctx_index::lookup(
                            sid.clone(),
                            s.cwd.clone(),
                            map.clone(),
                            tx.clone(),
                            ctx.clone(),
                        );
                    }
                },
            }
        }
        if watching {
            ctx.request_repaint_after(interval);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let mut acts = Vec::new();
        let active = self.active;
        ctx.input_mut(|i| {
            if consume_cmd(i, Key::T) {
                acts.push(Act::NewSame);
            }
            if consume_cmd(i, Key::N) {
                acts.push(Act::NewPick);
            }
            if consume_cmd(i, Key::Comma) {
                acts.push(Act::Settings);
            }
            if let Some(id) = active {
                if consume_cmd(i, Key::W) {
                    acts.push(Act::Close(id));
                }
            }
            for (n, key) in [
                Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5,
                Key::Num6, Key::Num7, Key::Num8, Key::Num9,
            ]
            .iter()
            .enumerate()
            {
                if consume_cmd(i, *key) {
                    if let Some(s) = self.sessions.get(n) {
                        acts.push(Act::Select(s.id));
                    }
                }
            }
        });
        self.apply(acts, ctx);
    }

    // ---- UI ----

    fn sidebar(&mut self, ui: &mut egui::Ui) -> Vec<Act> {
        let mut acts = Vec::new();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            if ui
                .button(RichText::new(tr("+ Терминал", "+ Terminal")).size(12.5))
                .on_hover_text(tr("Новый терминал в директории активной сессии (Cmd+T)", "New terminal in the active session's directory (Cmd+T)"))
                .clicked()
            {
                acts.push(Act::NewSame);
            }
        });
        ui.add_space(8.0);

        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let ids: Vec<u64> = self.sessions.iter().map(|s| s.id).collect();
            for id in ids {
                let idx = self.idx_of(id).unwrap();
                self.session_row(ui, idx, &mut acts);
            }
        });
        acts
    }

    fn session_row(&self, ui: &mut egui::Ui, idx: usize, acts: &mut Vec<Act>) {
        let s = &self.sessions[idx];
        let selected = self.active == Some(s.id);
        let row_h = 48.0;
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        // Geometric hover: overlapping child widgets (close button, ctx corner)
        // must not make the row highlight and the button flicker.
        let hovered = ui.rect_contains_pointer(rect);
        let painter = ui.painter();

        if selected {
            painter.rect_filled(rect, 0.0, Color32::from_rgb(0x24, 0x24, 0x24));
            painter.rect_filled(
                Rect::from_min_size(rect.min, Vec2::new(2.0, row_h)),
                0.0,
                Color32::from_rgb(0x9a, 0x9a, 0x9a),
            );
        } else if hovered {
            painter.rect_filled(rect, 0.0, Color32::from_rgb(0x1f, 0x1f, 0x1f));
        }

        // Status: green = working (recent output), orange = waiting for input,
        // red = exited with error. A star instead of a dot means claude is running.
        let dot = Pos2::new(rect.min.x + 16.0, rect.center().y);
        let is_claude = s.fg_is_claude;
        match &s.phase {
            Phase::Live(_) if s.busy => {
                let working = s.last_activity.elapsed().as_secs() < 3;
                let color = if working { DOT_BUSY } else { ORANGE };
                if is_claude {
                    draw_star(painter, dot, 5.0, color);
                } else {
                    painter.circle_filled(dot, 3.5, color);
                }
            },
            Phase::Live(_) => {
                painter.circle_filled(dot, 3.0, DOT_LIVE);
            },
            Phase::Suspended => {
                painter.circle_stroke(dot, 3.0, Stroke::new(1.2, TXT_DIM));
            },
            Phase::Exited(code) => {
                let color = if code.is_some_and(|c| c != 0) { DOT_EXITED } else { Color32::from_gray(0x6a) };
                painter.circle_filled(dot, 3.0, color);
            },
        }

        // Context badge: pill with the session's context %, top-right.
        // No index entry = no badge (never a fake 0%).
        let ctx_entry = s.claude_session_id.as_deref().and_then(|sid| self.ctx_index.get(sid));
        let mut name_max = 26;
        if let Some(e) = ctx_entry {
            let pct = e.pct.clamp(1.0, 100.0);
            let (bg, fg) = if pct >= 70.0 {
                // Pulse at ~10fps while visible.
                let t = ui.input(|i| i.time);
                let a = ((t * 4.0).sin() * 0.5 + 0.5) as f32;
                let lerp = |lo: u8, hi: u8| (lo as f32 + (hi as f32 - lo as f32) * a) as u8;
                (
                    Color32::from_rgb(lerp(0x38, 0x5c), lerp(0x1e, 0x22), lerp(0x1e, 0x22)),
                    Color32::from_rgb(0xe2, 0x8f, 0x8f),
                )
            } else if pct >= 50.0 {
                (Color32::from_rgb(0x33, 0x2f, 0x1a), Color32::from_rgb(0xd4, 0xc4, 0x5a))
            } else {
                (Color32::from_rgb(0x21, 0x2b, 0x1d), GIT_ADD)
            };
            let galley =
                painter.layout_no_wrap(format!("{pct:.0}%"), FontId::monospace(9.5), fg);
            let pad = Vec2::new(5.0, 2.0);
            let size = galley.size() + pad * 2.0;
            let pill = Rect::from_min_size(
                Pos2::new(rect.max.x - 8.0 - size.x, rect.min.y + 5.0),
                size,
            );
            painter.rect_filled(pill, CornerRadius::same(7), bg);
            painter.galley(pill.min + pad, galley, fg);
            if pct >= 70.0 {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            name_max = 21;
        }

        let text_x = rect.min.x + 28.0;
        let name_color = if selected { Color32::from_rgb(0xde, 0xde, 0xde) } else { TXT };
        painter.text(
            Pos2::new(text_x, rect.center().y - 9.0),
            Align2::LEFT_CENTER,
            truncate_end(&s.display_name(), name_max),
            FontId::proportional(13.0),
            name_color,
        );
        painter.text(
            Pos2::new(text_x, rect.center().y + 8.0),
            Align2::LEFT_CENTER,
            truncate_head(&tilde(&s.cwd), 34),
            FontId::proportional(10.5),
            TXT_FAINT,
        );

        // Right side: close button on hover, otherwise unread / suspended marker.
        // The interact widget exists every frame; only the drawing is conditional,
        // otherwise hovering the button hides it and clicks fall through.
        let mark = Pos2::new(rect.max.x - 17.0, rect.center().y + 4.0);
        let hit = Rect::from_center_size(mark, Vec2::splat(20.0));
        let close_resp = ui.interact(hit, ui.id().with(("close", s.id)), Sense::click());
        if close_resp.clicked() {
            acts.push(Act::Remove(s.id));
        }
        if hovered || close_resp.hovered() {
            let (bg, fg) = if close_resp.hovered() {
                (Color32::from_rgb(0x45, 0x2c, 0x2c), Color32::from_rgb(0xe2, 0x9a, 0x9a))
            } else {
                (Color32::from_rgb(0x30, 0x30, 0x30), Color32::from_rgb(0xc4, 0xc4, 0xc4))
            };
            ui.painter().circle_filled(mark, 9.0, bg);
            let d = 3.4;
            let st = Stroke::new(1.5, fg);
            ui.painter().line_segment([mark + Vec2::new(-d, -d), mark + Vec2::new(d, d)], st);
            ui.painter().line_segment([mark + Vec2::new(-d, d), mark + Vec2::new(d, -d)], st);
            close_resp.on_hover_text(tr("Закрыть сессию", "Close session"));
        } else if s.unread {
            ui.painter().circle_filled(mark, 3.0, UNREAD);
        } else if s.keep_awake {
            ui.painter().text(mark, Align2::CENTER_CENTER, "!", FontId::proportional(11.0), TXT_FAINT);
        }

        if resp.clicked() {
            acts.push(Act::Select(s.id));
        }
        resp.context_menu(|ui| {
            match &s.phase {
                Phase::Live(_) => {
                    if ui.button(tr("Усыпить", "Suspend")).clicked() {
                        acts.push(Act::Suspend(s.id));
                        ui.close();
                    }
                    let label = if s.keep_awake { tr("Разрешить усыпление", "Allow suspend") } else { tr("Не усыплять", "Keep awake") };
                    if ui.button(label).clicked() {
                        acts.push(Act::ToggleAwake(s.id));
                        ui.close();
                    }
                    if ui.button(tr("Закрыть", "Close")).clicked() {
                        acts.push(Act::Remove(s.id));
                        ui.close();
                    }
                },
                _ => {
                    if s.claude_session_id.is_some() && ui.button(tr("Продолжить Claude", "Resume Claude")).clicked() {
                        acts.push(Act::Resume(s.id, true));
                        ui.close();
                    }
                    if ui.button(tr("Открыть терминал", "Open terminal")).clicked() {
                        acts.push(Act::OpenTerminal(s.id));
                        ui.close();
                    }
                    if ui.button(tr("Удалить", "Delete")).clicked() {
                        acts.push(Act::Remove(s.id));
                        ui.close();
                    }
                },
            }
        });
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) -> Vec<Act> {
        let mut acts = Vec::new();
        let Some(idx) = self.active_idx() else {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new(tr("Нет активной сессии", "No active session")).size(11.5).color(TXT_FAINT));
            });
            return acts;
        };

        let (is_live, busy_now) = match &self.sessions[idx].phase {
            Phase::Live(l) => (
                true,
                plat::foreground_pgid(l.master_fd, l.shell_pid).is_some_and(|pg| pg != l.shell_pid),
            ),
            _ => (false, false),
        };
        let mut chip_clicked = false;
        let mut chip_rect = Rect::NOTHING;
        {
            let git = self.sessions[idx].git.clone();
            let s = &mut self.sessions[idx];
            ui.horizontal_centered(|ui| {
                ui.add_space(6.0);
                // Warp-style path chip: click opens the directory switcher.
                let path_full = tilde(&s.cwd);
                let chip = ui
                    .add_enabled(
                        !(is_live && busy_now),
                        egui::Button::new(
                            RichText::new(truncate_head(&path_full, 44)).monospace().size(11.5).color(TXT),
                        ),
                    )
                    .on_hover_text(format!("{path_full}\n{}", tr("Сменить папку", "Change folder")))
                    .on_disabled_hover_text(tr("Терминал занят", "Terminal busy"));
                if chip.clicked() {
                    chip_clicked = true;
                }
                chip_rect = chip.rect;

                match &git {
                    Some(g) if g.is_repo => {
                        ui.add_space(6.0);
                        ui.label(RichText::new(&g.branch).size(11.5).color(TXT_DIM));
                        if g.added > 0 || g.deleted > 0 {
                            ui.add_space(2.0);
                            ui.label(
                                RichText::new(format!("+{}", g.added)).size(13.0).strong().color(GIT_ADD),
                            );
                            ui.label(
                                RichText::new(format!("-{}", g.deleted)).size(13.0).strong().color(GIT_DEL),
                            );
                        } else {
                            ui.add_space(2.0);
                            ui.label(RichText::new(tr("чисто", "clean")).size(11.0).color(TXT_FAINT));
                        }
                    },
                    Some(_) => {
                        ui.add_space(6.0);
                        ui.label(RichText::new(tr("не git", "not git")).size(11.0).color(TXT_FAINT));
                    },
                    None => {},
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(10.0);
                    let upd = matches!(self.update_state, UpdateState::Available(_));
                    let label = if upd {
                        RichText::new(tr("Настройки", "Settings")).size(11.5).color(GIT_ADD).strong()
                    } else {
                        RichText::new(tr("Настройки", "Settings")).size(11.5)
                    };
                    let btn = ui.button(label);
                    let btn = if upd { btn.on_hover_text(tr("Доступно обновление", "Update available")) } else { btn };
                    if btn.clicked() {
                        acts.push(Act::Settings);
                    }
                    ui.add_space(4.0);

                    match &s.phase {
                        Phase::Live(_) => {
                            // skip-permissions only matters when (re)launching claude,
                            // so it lives on the restart card, not here.
                            if let Some(cid) = &s.claude_session_id {
                                ui.label(
                                    RichText::new(short_id(cid)).size(10.5).monospace().color(TXT_FAINT),
                                )
                                .on_hover_text(format!("{}: {cid}", tr("Сохранённая сессия Claude", "Saved Claude session")));
                            }
                        },
                        Phase::Suspended | Phase::Exited(_) => {
                            // Resume/terminal actions live on the card in the
                            // terminal area; the status bar just shows state.
                            if let Phase::Exited(code) = &s.phase {
                                let txt = match code {
                                    Some(c) => format!("{} {c}", tr("завершено, код", "exited, code")),
                                    None => tr("завершено", "done").into(),
                                };
                                ui.label(RichText::new(txt).size(11.0).color(TXT_FAINT));
                            } else {
                                ui.label(RichText::new(tr("усыплена", "suspended")).size(11.0).color(TXT_FAINT));
                            }
                        },
                    }
                });
            });
        }
        self.chip_rect = Some(chip_rect);
        if chip_clicked {
            self.dir_open = !self.dir_open;
            if self.dir_open {
                self.dir_query.clear();
                self.dir_path = self.sessions[idx].cwd.clone();
            }
        }
        acts
    }

    fn central(&mut self, ui: &mut egui::Ui) -> Vec<Act> {
        let mut acts = Vec::new();
        if self.sessions.is_empty() {
            self.empty_state(ui, &mut acts);
            return acts;
        }
        let Some(idx) = self.active_idx() else { return acts };

        let is_live = matches!(self.sessions[idx].phase, Phase::Live(_));
        if is_live {
            let busy_now = {
                let Phase::Live(l) = &self.sessions[idx].phase else { unreachable!() };
                plat::foreground_pgid(l.master_fd, l.shell_pid).is_some_and(|pg| pg != l.shell_pid)
            };
            // Own command editor while the shell is at its prompt; a running
            // program (claude, vim, ...) gets the keyboard directly.
            let submitted = if busy_now { None } else { self.cmd_panel(ui) };
            let settings = self.settings.clone();
            let accept = busy_now && !self.settings_open && !self.dir_open;
            let term_rect = ui.available_rect_before_wrap();
            let s = &mut self.sessions[idx];
            let info = term_view::show(ui, s, &settings, accept);
            self.cell = (info.cell_w.round().max(1.0) as u16, info.cell_h.round().max(1.0) as u16);
            self.grid = (info.cols, info.rows);
            if info.had_input || info.interacted {
                self.sessions[idx].last_activity = Instant::now();
            }

            // Sticky header once the content fills the viewport: path, command, duration.
            if busy_now && info.grown {
                let s = &self.sessions[idx];
                let dur = s.busy_since.map(|t| t.elapsed()).unwrap_or_default();
                let cmd_text = s
                    .running_cmd
                    .clone()
                    .or_else(|| s.fg_name.clone())
                    .unwrap_or_default();
                let head = Rect::from_min_size(term_rect.min, Vec2::new(term_rect.width(), 40.0));
                let p = ui.painter();
                p.rect_filled(head, 0.0, Color32::from_rgba_unmultiplied(0x15, 0x15, 0x15, 236));
                p.hline(head.x_range(), head.bottom(), Stroke::new(1.0, Color32::from_rgb(0x28, 0x28, 0x28)));
                p.text(
                    head.min + Vec2::new(10.0, 5.0),
                    Align2::LEFT_TOP,
                    format!("{} ({})", tilde(&s.cwd), fmt_dur(dur)),
                    FontId::monospace(10.5),
                    TXT_DIM,
                );
                p.text(
                    head.min + Vec2::new(10.0, 20.0),
                    Align2::LEFT_TOP,
                    truncate_end(&cmd_text, 110),
                    FontId::monospace(12.0),
                    TXT,
                );
                ui.ctx().request_repaint_after(Duration::from_secs(1));
            }

            if let Some(cmd) = submitted {
                let cctx = ui.ctx().clone();
                self.send_command_to(idx, &cmd, &cctx);
            }
        } else {
            self.frozen_view(ui, idx, &mut acts);
        }
        acts
    }

    /// Command editor pinned under the terminal + filtered history popup.
    /// Returns a command to execute.
    fn cmd_panel(&mut self, ui: &mut egui::Ui) -> Option<String> {
        self.refresh_history();
        let ctx = ui.ctx().clone();
        let interactive = !self.settings_open && !self.dir_open;
        let mut submit: Option<String> = None;

        // Multiline once the command has a newline (added with Shift+Enter, which
        // the singleline consume below lets through to the editor). Plain Enter
        // still submits. While multiline, arrows move the caret, not history.
        let multiline = self.cmd_input.contains('\n');
        let (mut enter, mut up, mut down, mut esc) = (false, false, false, false);
        if interactive {
            ctx.input_mut(|i| {
                enter = consume_plain(i, Key::Enter);
                if !multiline {
                    up = consume_plain(i, Key::ArrowUp);
                    down = consume_plain(i, Key::ArrowDown);
                }
                esc = consume_plain(i, Key::Escape);
                // Tab would move egui focus away from the editor.
                consume_plain(i, Key::Tab);
            });
        }

        let q = self.hist_query.to_lowercase();
        let mut display: Vec<String> = self
            .history
            .iter()
            .zip(self.history_lc.iter())
            .rev()
            .filter(|(_, lc)| q.is_empty() || lc.contains(&q))
            .take(40)
            .map(|(h, _)| h.clone())
            .collect();
        display.reverse();
        let show_n = display.len();
        // While settings or the directory switcher is open the editor is not
        // interactive; close the history popup so it does not sit there frozen.
        if !interactive {
            self.hist_forced = false;
            self.hist_sel = None;
        }
        let mut hist_visible = interactive
            && !self.hist_dismissed
            && (!self.cmd_input.is_empty() || self.hist_forced)
            && show_n > 0;

        // Any programmatic fill (history nav) or a plain ArrowDown puts the caret
        // at the end of the line - Down means "end of line" by habit.
        let mut caret_end = false;
        if up {
            if hist_visible {
                let sel = match self.hist_sel {
                    None => show_n - 1,
                    Some(0) => 0,
                    Some(i) => i - 1,
                };
                self.hist_sel = Some(sel);
                self.cmd_input = display[sel].clone();
                caret_end = true;
            } else if self.cmd_input.is_empty() && show_n > 0 {
                // First Up on an empty line: open and land on the most recent
                // command right away, not on a second press.
                self.hist_forced = true;
                self.hist_dismissed = false;
                self.hist_sel = Some(show_n - 1);
                self.cmd_input = display[show_n - 1].clone();
                caret_end = true;
                hist_visible = true;
            }
        }
        if down {
            match self.hist_sel {
                Some(i) if hist_visible && i + 1 < show_n => {
                    self.hist_sel = Some(i + 1);
                    self.cmd_input = display[i + 1].clone();
                },
                Some(_) if hist_visible => {
                    self.hist_sel = None;
                    self.cmd_input = self.hist_query.clone();
                },
                _ => {},
            }
            caret_end = true;
        }
        if esc {
            if hist_visible {
                self.hist_dismissed = true;
                self.hist_forced = false;
                self.hist_sel = None;
                hist_visible = false;
            } else {
                self.cmd_input.clear();
                self.hist_query.clear();
            }
        }
        if enter {
            let text = self.cmd_input.trim();
            if !text.is_empty() {
                submit = Some(text.to_string());
            }
        }

        let font = FontId::monospace(self.settings.font_size);
        // Grow the editor with the number of lines (Shift+Enter), capped.
        let n_lines = (self.cmd_input.matches('\n').count() + 1).clamp(1, 8);
        let panel_h = 36.0 + (n_lines as f32 - 1.0) * (self.settings.font_size + 6.0);
        let field_rect = egui::Panel::bottom("cmdline")
            .exact_size(panel_h)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::new().fill(palette::chrome_bar()))
            .show(ui, |ui| {
                let top = ui.max_rect().top();
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    top,
                    Stroke::new(1.0, Color32::from_rgb(0x2a, 0x2a, 0x2a)),
                );
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(">").font(font.clone()).color(TXT_DIM));
                    // Multiline, but egui only inserts a newline on its return_key -
                    // point that at Shift+Enter. Plain Enter is consumed above and
                    // submits, so the editor never sees it.
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut self.cmd_input)
                            .frame(Frame::new())
                            .font(font.clone())
                            .desired_rows(1)
                            .return_key(egui::KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                            .hint_text(RichText::new(tr("команда...", "command...")).font(font).color(TXT_FAINT))
                            .desired_width(ui.available_width() - 8.0),
                    );
                    if interactive {
                        resp.request_focus();
                    }
                    // Keep arrows/Tab from moving egui focus off the editor
                    // (Down would land on the path chip below otherwise).
                    ui.memory_mut(|m| {
                        m.set_focus_lock_filter(
                            resp.id,
                            egui::EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: false,
                            },
                        );
                    });
                    if caret_end {
                        if let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), resp.id) {
                            let end = egui::text::CCursor::new(self.cmd_input.chars().count());
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                            state.store(ui.ctx(), resp.id);
                        }
                    }
                    if resp.changed() {
                        self.hist_query = self.cmd_input.clone();
                        self.hist_dismissed = false;
                        self.hist_forced = false;
                        self.hist_sel = None;
                    }
                    resp.rect
                })
                .inner
            })
            .inner;

        if hist_visible {
            egui::Area::new(egui::Id::new("hist-popup"))
                .order(egui::Order::Foreground)
                .pivot(Align2::LEFT_BOTTOM)
                .fixed_pos(field_rect.left_top() + Vec2::new(-6.0, -8.0))
                .show(&ctx, |ui| {
                    Frame::new()
                        .fill(POPUP_BG)
                        .stroke(Stroke::new(1.0, POPUP_STROKE))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_width(field_rect.width().min(760.0));
                            ui.label(RichText::new(tr("История", "History")).size(9.5).color(TXT_FAINT));
                            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                                for (i, cmd) in display.iter().enumerate() {
                                    let selected = self.hist_sel == Some(i);
                                    let resp = ui.selectable_label(
                                        selected,
                                        RichText::new(truncate_end(cmd, 90)).monospace().size(11.5),
                                    );
                                    if selected {
                                        resp.scroll_to_me(None);
                                    }
                                    if resp.clicked() {
                                        submit = Some(cmd.clone());
                                    }
                                }
                            });
                            ui.label(
                                RichText::new(tr("стрелки - выбор   esc - закрыть   enter - выполнить", "arrows - select   esc - close   enter - run"))
                                    .size(9.0)
                                    .color(TXT_FAINT),
                            );
                        });
                });
        }

        if submit.is_some() {
            self.cmd_input.clear();
            self.hist_query.clear();
            self.hist_sel = None;
            self.hist_forced = false;
            self.hist_dismissed = false;
        }
        submit
    }

    /// Resource monitor button in the top-right corner; hover shows a live
    /// per-session breakdown (cpu + memory of each session's process tree, GPU).
    fn stats_ui(&mut self, ctx: &egui::Context) {
        let chip = egui::Area::new(egui::Id::new("stats-chip"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_TOP, Vec2::new(-10.0, 8.0))
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(30.0, 20.0), Sense::hover());
                let active = resp.hovered() || self.stats_rect.is_some();
                let bg = if active { Color32::from_rgb(0x2c, 0x2c, 0x2c) } else { Color32::from_rgb(0x20, 0x20, 0x20) };
                ui.painter().rect_filled(rect, CornerRadius::same(5), bg);
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::same(5),
                    Stroke::new(1.0, Color32::from_rgb(0x35, 0x35, 0x35)),
                    egui::StrokeKind::Inside,
                );
                let fg = if active { TXT } else { TXT_DIM };
                let base = rect.center() + Vec2::new(0.0, 6.0);
                for (i, h) in [5.0, 9.0, 7.0].iter().enumerate() {
                    let x = base.x - 5.0 + i as f32 * 5.0;
                    ui.painter().line_segment(
                        [Pos2::new(x, base.y), Pos2::new(x, base.y - h)],
                        Stroke::new(2.0, fg),
                    );
                }
                resp
            });

        let pointer = ctx.input(|i| i.pointer.interact_pos());
        let over_popup = self
            .stats_rect
            .is_some_and(|r| pointer.is_some_and(|p| r.expand(6.0).contains(p)));
        let open = chip.inner.hovered() || over_popup;
        if !open {
            self.stats_rect = None;
            return;
        }

        // Sample only while the panel is open.
        let stale = self.stats_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(2));
        if stale && !self.stats_inflight {
            self.stats_inflight = true;
            let mut targets: Vec<(String, i32)> =
                vec![("kip".into(), std::process::id() as i32)];
            for s in &self.sessions {
                if let Some(live) = s.live() {
                    targets.push((s.display_name(), live.shell_pid));
                }
            }
            let tx = self.stats_tx.clone();
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let stats = plat::sample_stats(&targets);
                if tx.send(stats).is_ok() {
                    ctx2.request_repaint();
                }
            });
        }
        ctx.request_repaint_after(Duration::from_secs(2));

        let area = egui::Area::new(egui::Id::new("stats-popup"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_TOP, Vec2::new(-10.0, 32.0))
            .show(ctx, |ui| {
                Frame::new()
                    .fill(POPUP_BG)
                    .stroke(Stroke::new(1.0, POPUP_STROKE))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 12))
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 18,
                        spread: 0,
                        color: Color32::from_black_alpha(120),
                    })
                    .show(ui, |ui| {
                        let w = 240.0;
                        ui.set_min_width(w);
                        ui.label(RichText::new(tr("Ресурсы", "Resources")).size(10.0).strong().color(TXT_DIM));
                        ui.add_space(6.0);
                        let Some(st) = &self.stats else {
                            ui.label(RichText::new(tr("измеряю...", "measuring...")).size(11.0).color(TXT_FAINT));
                            return;
                        };
                        let max_rss = st.procs.iter().map(|p| p.2).max().unwrap_or(1).max(1);
                        let mut total = 0u64;
                        for (i, (name, cpu, rss)) in st.procs.iter().enumerate() {
                            total += rss;
                            let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 30.0), Sense::hover());
                            let p = ui.painter();
                            // Line 1: name left, memory right.
                            p.text(
                                rect.left_top() + Vec2::new(0.0, 1.0),
                                Align2::LEFT_TOP,
                                truncate_end(name, 22),
                                FontId::proportional(12.0),
                                if i == 0 { TXT_DIM } else { TXT },
                            );
                            p.text(
                                rect.right_top() + Vec2::new(0.0, 1.0),
                                Align2::RIGHT_TOP,
                                fmt_mem(*rss),
                                FontId::monospace(11.5),
                                TXT,
                            );
                            // Line 2: memory bar + cpu.
                            let bar_w = w - 52.0;
                            let by = rect.top() + 21.0;
                            let track = Rect::from_min_size(
                                Pos2::new(rect.left(), by),
                                Vec2::new(bar_w, 3.5),
                            );
                            p.rect_filled(track, CornerRadius::same(2), Color32::from_rgb(0x2a, 0x2a, 0x2a));
                            let frac = (*rss as f32 / max_rss as f32).clamp(0.02, 1.0);
                            let fill = Rect::from_min_size(
                                track.min,
                                Vec2::new(bar_w * frac, 3.5),
                            );
                            let bar_color = if i == 0 {
                                Color32::from_rgb(0x8a, 0x8a, 0x8a)
                            } else {
                                Color32::from_rgb(0x7d, 0x93, 0xa8)
                            };
                            p.rect_filled(fill, CornerRadius::same(2), bar_color);
                            p.text(
                                Pos2::new(rect.right(), by - 3.5),
                                Align2::RIGHT_TOP,
                                format!("{cpu:.0}% cpu"),
                                FontId::proportional(9.5),
                                if *cpu >= 50.0 { ORANGE } else { TXT_FAINT },
                            );
                            if i == 0 && st.procs.len() > 1 {
                                ui.add_space(3.0);
                                let sep = ui.available_rect_before_wrap();
                                ui.painter().hline(
                                    sep.left()..=sep.left() + w,
                                    sep.top(),
                                    Stroke::new(1.0, Color32::from_rgb(0x2c, 0x2c, 0x2c)),
                                );
                                ui.add_space(5.0);
                            }
                        }
                        ui.add_space(4.0);
                        let sep = ui.available_rect_before_wrap();
                        ui.painter().hline(
                            sep.left()..=sep.left() + w,
                            sep.top(),
                            Stroke::new(1.0, Color32::from_rgb(0x2c, 0x2c, 0x2c)),
                        );
                        ui.add_space(5.0);
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 14.0), Sense::hover());
                        let p = ui.painter();
                        p.text(
                            rect.left_top(),
                            Align2::LEFT_TOP,
                            tr("всего", "total"),
                            FontId::proportional(10.5),
                            TXT_FAINT,
                        );
                        p.text(
                            rect.right_top(),
                            Align2::RIGHT_TOP,
                            fmt_mem(total),
                            FontId::monospace(11.5),
                            TXT,
                        );
                    });
            });
        self.stats_rect = Some(area.response.rect);
    }

    /// Directory switcher over the path chip: navigating runs `cd` in the shell.
    fn dir_popup(&mut self, ctx: &egui::Context) {
        if !self.dir_open {
            return;
        }
        let Some(chip) = self.chip_rect else { return };

        let (mut esc, mut enter) = (false, false);
        ctx.input_mut(|i| {
            esc = consume_plain(i, Key::Escape);
            enter = consume_plain(i, Key::Enter);
        });
        if esc {
            self.dir_open = false;
            return;
        }

        let stale = self
            .dir_cache_at
            .as_ref()
            .is_none_or(|(p, t)| p != &self.dir_path || t.elapsed() >= Duration::from_secs(2));
        if stale {
            self.dir_cache = std::fs::read_dir(&self.dir_path)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .filter_map(|e| e.file_name().to_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            self.dir_cache.sort_by_key(|d| d.to_lowercase());
            self.dir_cache.truncate(2000);
            self.dir_cache_at = Some((self.dir_path.clone(), Instant::now()));
        }
        let q = self.dir_query.to_lowercase();
        let mut dirs: Vec<String> = self
            .dir_cache
            .iter()
            .filter(|d| !d.starts_with('.') || q.starts_with('.'))
            .filter(|d| q.is_empty() || d.to_lowercase().contains(&q))
            .cloned()
            .collect();
        dirs.truncate(200);

        // None = up one level, Some(name) = enter subdirectory.
        let mut nav: Option<Option<String>> = None;
        let area = egui::Area::new(egui::Id::new("dir-popup"))
            .order(egui::Order::Foreground)
            .pivot(Align2::LEFT_BOTTOM)
            .fixed_pos(chip.left_top() + Vec2::new(0.0, -8.0))
            .show(ctx, |ui| {
                Frame::new()
                    .fill(POPUP_BG)
                    .stroke(Stroke::new(1.0, POPUP_STROKE))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_width(430.0);
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.dir_query)
                                .frame(Frame::new())
                                .font(FontId::proportional(12.5))
                                .hint_text(tr("Поиск папок...", "Search folders..."))
                                .desired_width(f32::INFINITY),
                        );
                        resp.request_focus();
                        ui.separator();
                        ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            if self.dir_path.parent().is_some()
                                && ui
                                    .selectable_label(false, RichText::new(tr("..  (наверх)", "..  (up)")).size(12.0).color(TXT_DIM))
                                    .clicked()
                            {
                                nav = Some(None);
                            }
                            for d in &dirs {
                                if ui
                                    .selectable_label(false, RichText::new(truncate_end(d, 48)).size(12.5))
                                    .clicked()
                                {
                                    nav = Some(Some(d.clone()));
                                }
                            }
                            if dirs.is_empty() {
                                ui.label(RichText::new(tr("нет подпапок", "no subfolders")).size(11.0).color(TXT_FAINT));
                            }
                        });
                        ui.label(
                            RichText::new(truncate_head(&tilde(&self.dir_path), 52))
                                .monospace()
                                .size(9.5)
                                .color(TXT_FAINT),
                        );
                    });
            });

        if enter && nav.is_none() {
            if let Some(first) = dirs.first() {
                nav = Some(Some(first.clone()));
            }
        }
        if let Some(step) = nav {
            self.dir_navigate(step, ctx);
        }

        let clicked_outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .is_some_and(|p| !area.response.rect.contains(p) && !chip.contains(p))
        });
        if clicked_outside {
            self.dir_open = false;
        }
    }

    fn dir_navigate(&mut self, step: Option<String>, ctx: &egui::Context) {
        let new_path = match &step {
            None => match self.dir_path.parent() {
                Some(p) => p.to_path_buf(),
                None => return,
            },
            Some(name) => self.dir_path.join(name),
        };
        if let Some(idx) = self.active_idx() {
            let is_live = matches!(self.sessions[idx].phase, Phase::Live(_));
            if is_live {
                let cmd = match &step {
                    None => "cd ..".to_string(),
                    Some(name) => format!("cd '{}'", name.replace('\'', "'\\''")),
                };
                self.send_command_to(idx, &cmd, ctx);
            } else {
                let s = &mut self.sessions[idx];
                s.cwd = new_path.clone();
                s.git = None;
                s.last_git_poll = None;
                self.persist();
            }
        }
        self.dir_path = new_path;
        self.dir_query.clear();
    }

    fn frozen_view(&mut self, ui: &mut egui::Ui, idx: usize, acts: &mut Vec<Act>) {
        let font_size = self.settings.font_size;
        let mut skip_changed = false;
        let s = &mut self.sessions[idx];
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, palette::term_bg());

        if let Some(snap) = &s.snapshot {
            ScrollArea::vertical()
                .id_salt(("snap", s.id))
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(snap)
                                .font(FontId::monospace(font_size - 1.0))
                                .color(Color32::from_rgb(0x6a, 0x6a, 0x6a)),
                        );
                    });
                    ui.add_space(70.0);
                });
        }

        let (title, hint) = match &s.phase {
            Phase::Exited(Some(code)) => (
                tr("Процесс завершён", "Process exited").to_string(),
                format!("{} {code}", tr("код выхода", "exit code")),
            ),
            Phase::Exited(None) => (tr("Процесс завершён", "Process exited").to_string(), String::new()),
            _ => (tr("Сессия усыплена", "Session suspended").to_string(), tr("процессы остановлены, память освобождена", "processes stopped, memory freed").to_string()),
        };

        // Lower-middle of the terminal area, floating clear of the bottom.
        let card_pos = Pos2::new(rect.center().x, rect.top() + rect.height() * 0.68);
        egui::Area::new(ui.id().with(("frozen", s.id)))
            .pivot(Align2::CENTER_CENTER)
            .fixed_pos(card_pos)
            .show(ui.ctx(), |ui| {
                Frame::new()
                    .fill(Color32::from_rgb(0x1f, 0x1f, 0x1f))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(0x35, 0x35, 0x35)))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.set_min_width(300.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(title).size(14.0).strong().color(TXT));
                            if !hint.is_empty() {
                                ui.label(RichText::new(hint).size(11.0).color(TXT_DIM));
                            }
                            ui.label(RichText::new(tilde(&s.cwd)).size(11.0).color(TXT_DIM));
                            if let Some(cid) = &s.claude_session_id {
                                ui.label(
                                    RichText::new(format!("claude: {}", short_id(cid)))
                                        .size(10.5)
                                        .monospace()
                                        .color(TXT_FAINT),
                                );
                            }
                            if s.claude_session_id.is_some()
                                && ui
                                    .checkbox(
                                        &mut s.skip_permissions,
                                        RichText::new("skip-permissions").size(11.0),
                                    )
                                    .changed()
                            {
                                skip_changed = true;
                            }
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if s.claude_session_id.is_some() {
                                    let btn = egui::Button::new(
                                        RichText::new(tr("Продолжить Claude", "Resume Claude")).size(12.5).color(Color32::from_rgb(0xd8, 0xe4, 0xd0)),
                                    )
                                    .fill(Color32::from_rgb(0x2e, 0x3a, 0x2a));
                                    if ui.add(btn).clicked() {
                                        acts.push(Act::Resume(s.id, true));
                                    }
                                }
                                if ui.button(RichText::new(tr("Открыть терминал", "Open terminal")).size(12.5)).clicked() {
                                    acts.push(Act::OpenTerminal(s.id));
                                }
                                if ui.button(RichText::new(tr("Удалить", "Delete")).size(12.5).color(TXT_DIM)).clicked() {
                                    acts.push(Act::Remove(s.id));
                                }
                            });
                        });
                    });
            });
        if skip_changed {
            self.persist();
        }
    }

    fn empty_state(&self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, palette::term_bg());
        ui.vertical_centered(|ui| {
            ui.add_space(rect.height() * 0.35);
            ui.label(RichText::new("kip").size(22.0).color(TXT_DIM));
            ui.label(RichText::new(tr("Нет открытых сессий", "No open sessions")).size(12.5).color(TXT_FAINT));
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.add_space(rect.width() / 2.0 - 130.0);
                if ui.button(tr("Новый терминал", "New terminal")).clicked() {
                    acts.push(Act::NewSame);
                }
                if ui.button(tr("Выбрать папку...", "Choose folder...")).clicked() {
                    acts.push(Act::NewPick);
                }
            });
        });
    }

    fn settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.settings_open;
        egui::Window::new(tr("Настройки", "Settings"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                egui::Grid::new("settings-grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                    ui.label(tr("Масштаб интерфейса", "UI scale"));
                    let resp = ui.add(
                        egui::Slider::new(&mut self.settings.ui_scale, 0.75..=1.75)
                            .step_by(0.05)
                            .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                    )
                    .on_hover_text(tr("Также работает Cmd+= / Cmd+- / Cmd+0", "Also works: Cmd+= / Cmd+- / Cmd+0"));
                    // Apply on release so the layout does not jump under the drag.
                    if resp.drag_stopped() || (resp.changed() && !resp.dragged()) {
                        ctx.set_zoom_factor(self.settings.ui_scale);
                    }
                    ui.end_row();

                    ui.label(tr("Размер шрифта", "Font size"));
                    ui.add(egui::Slider::new(&mut self.settings.font_size, 9.0..=20.0).step_by(0.5));
                    ui.end_row();

                    ui.label(tr("Скроллбэк (строк)", "Scrollback (lines)"));
                    ui.add(
                        egui::DragValue::new(&mut self.settings.scrollback)
                            .range(200..=50_000)
                            .speed(100),
                    )
                    .on_hover_text(tr("Применяется к новым терминалам", "Applies to new terminals"));
                    ui.end_row();

                    ui.label(tr("Усыплять после (мин)", "Suspend after (min)"));
                    ui.add(egui::DragValue::new(&mut self.settings.idle_suspend_min).range(0..=240))
                        .on_hover_text(tr("0 = не усыплять. Сессия без вывода дольше этого времени завершается с возможностью продолжить", "0 = never. A session idle longer than this is suspended and can be resumed"));
                    ui.end_row();

                    ui.label(tr("Команда Claude", "Claude command"));
                    ui.add(egui::TextEdit::singleline(&mut self.settings.claude_cmd).desired_width(160.0));
                    ui.end_row();

                    ui.label(tr("Язык", "Language"));
                    let langs = [
                        ("auto", tr("Авто", "Auto")),
                        ("ru", "Русский"),
                        ("en", "English"),
                    ];
                    let cur_lang = langs
                        .iter()
                        .find(|(k, _)| *k == self.settings.lang)
                        .map(|(_, l)| *l)
                        .unwrap_or(tr("Авто", "Auto"));
                    egui::ComboBox::from_id_salt("lang")
                        .selected_text(cur_lang)
                        .width(160.0)
                        .show_ui(ui, |ui| {
                            for (k, label) in langs {
                                if ui.selectable_label(self.settings.lang == k, label).clicked()
                                    && self.settings.lang != k
                                {
                                    self.settings.lang = k.to_string();
                                    i18n::set(i18n::resolve(k));
                                    ctx.request_repaint();
                                }
                            }
                        });
                    ui.end_row();

                    ui.label(tr("Тема", "Theme"));
                    let cur = palette::PRESETS
                        .iter()
                        .find(|p| p.key == self.settings.theme)
                        .map(|p| p.label)
                        .unwrap_or(palette::PRESETS[0].label);
                    let mut theme_changed = false;
                    egui::ComboBox::from_id_salt("theme-preset")
                        .selected_text(cur)
                        .width(160.0)
                        .show_ui(ui, |ui| {
                            for p in palette::PRESETS.iter() {
                                if ui.selectable_label(self.settings.theme == p.key, p.label).clicked()
                                    && self.settings.theme != p.key
                                {
                                    self.settings.theme = p.key.to_string();
                                    // A fresh preset drops the custom accent so
                                    // its native selection color shows.
                                    self.settings.accent = None;
                                    theme_changed = true;
                                }
                            }
                        });
                    ui.end_row();

                    ui.label(tr("Акцент выделения", "Selection accent"));
                    ui.horizontal(|ui| {
                        let s = palette::selection();
                        let mut acc = self.settings.accent.unwrap_or([s.r(), s.g(), s.b()]);
                        if ui.color_edit_button_srgb(&mut acc).changed() {
                            self.settings.accent = Some(acc);
                            theme_changed = true;
                        }
                        if self.settings.accent.is_some()
                            && ui.small_button(tr("сброс", "reset")).clicked()
                        {
                            self.settings.accent = None;
                            theme_changed = true;
                        }
                    });
                    ui.end_row();

                    if theme_changed {
                        palette::apply(&self.settings.theme, self.settings.accent.map(rgb32));
                        apply_style(ctx);
                        ctx.request_repaint();
                    }
                });
                ui.separator();
                ui.checkbox(&mut self.settings.notify_job_done, tr("Уведомлять, когда агент завершил работу", "Notify when the agent finishes"));
                ui.checkbox(&mut self.settings.notify_bell, tr("Уведомлять по сигналу терминала (bell)", "Notify on terminal bell"));
                ui.checkbox(&mut self.settings.notify_sound, tr("Звук уведомлений", "Notification sound"));
                ui.checkbox(&mut self.settings.copy_on_select, tr("Копировать выделенное сразу в буфер", "Copy selection to clipboard immediately"));
                ui.checkbox(
                    &mut self.settings.skip_permissions_default,
                    tr(
                        "skip-permissions по умолчанию для новых сессий",
                        "skip-permissions by default for new sessions",
                    ),
                );
                // The exact-% statusline hook is a POSIX shell script; on
                // Windows the badge falls back to the transcript estimate.
                #[cfg(not(windows))]
                {
                    ui.separator();
                    let mut hook_on = self.settings.ctx_hook;
                    let resp = ui
                        .checkbox(&mut hook_on, tr("Точный % контекста Claude (statusline-хук)", "Exact Claude context % (statusline hook)"))
                        .on_hover_text(tr(
                            "Ставит крошечный скрипт в ~/.kip/bin и подключает его statusline-хуком \
                             Claude Code - % будет ровно тот, что видит сам Claude.\n\
                             Уже настроенный statusline не ломается: он оборачивается и продолжает \
                             работать. Снятие галочки возвращает всё как было.",
                            "Installs a tiny script in ~/.kip/bin and wires it as a Claude Code \
                             statusline hook - the % is exactly what Claude itself shows.\n\
                             An existing statusline is not broken: it gets wrapped and keeps \
                             working. Unchecking restores everything.",
                        ));
                    if resp.changed() {
                        let res = if hook_on {
                            ctx_index::install_hook(&mut self.settings)
                        } else {
                            ctx_index::uninstall_hook(&mut self.settings)
                        };
                        match res {
                            Ok(()) => {
                                self.settings.ctx_hook = hook_on;
                                self.hook_error = None;
                            },
                            Err(e) => self.hook_error = Some(e),
                        }
                    }
                    if let Some(e) = &self.hook_error {
                        ui.label(RichText::new(e).size(10.5).color(GIT_DEL));
                    }
                }

                // Updates. Flags are collected during the immutable borrow of
                // update_state and acted on afterwards to avoid a borrow clash.
                ui.separator();
                let busy = matches!(
                    self.update_state,
                    UpdateState::Checking | UpdateState::Working
                );
                let mut do_check = false;
                let mut do_update: Option<update::Release> = None;
                let mut do_open = false;
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{} {}", tr("Версия", "Version"), update::current_label()))
                            .size(11.5)
                            .color(TXT_DIM),
                    );
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(RichText::new(tr("Проверить обновления", "Check for updates")).size(11.5)),
                        )
                        .clicked()
                    {
                        do_check = true;
                    }
                });
                match &self.update_state {
                    UpdateState::Checking => {
                        ui.label(RichText::new(tr("проверяю...", "checking...")).size(10.5).color(TXT_FAINT));
                    },
                    UpdateState::UpToDate => {
                        ui.label(
                            RichText::new(tr("установлена последняя версия", "you're on the latest version")).size(10.5).color(TXT_FAINT),
                        );
                    },
                    UpdateState::Working => {
                        ui.label(
                            RichText::new(tr("загружаю и устанавливаю, сейчас перезапущусь...", "downloading and installing, restarting soon..."))
                                .size(10.5)
                                .color(ORANGE),
                        );
                    },
                    UpdateState::Failed(e) => {
                        ui.label(RichText::new(e).size(10.5).color(GIT_DEL));
                        if ui.button(RichText::new(tr("Открыть страницу загрузки", "Open download page")).size(11.0)).clicked() {
                            do_open = true;
                        }
                    },
                    UpdateState::Available(r) => {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(format!("{} {}", tr("Доступна версия", "Update available:"), r.display))
                                    .size(11.5)
                                    .color(GIT_ADD),
                            );
                            if ui
                                .button(RichText::new(tr("Обновить", "Update")).size(11.5).color(GIT_ADD))
                                .clicked()
                            {
                                do_update = Some(r.clone());
                            }
                        });
                    },
                    UpdateState::Idle => {},
                }
                if do_check {
                    self.update_state = UpdateState::Checking;
                    update::check(self.upd_tx.clone(), ctx.clone());
                }
                if let Some(rel) = do_update {
                    self.persist();
                    self.update_state = UpdateState::Working;
                    update::apply(rel, self.upd_tx.clone(), ctx.clone());
                }
                if do_open {
                    update::open_releases();
                }
            });
        // Settings apply live from memory; the file is written once, on close
        // (and again by on_exit), not on every slider-drag frame.
        if self.settings_open && !open {
            self.persist();
            self.hook_error = None;
        }
        self.settings_open = open;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        self.drain_git();
        self.drain_ctx(&ctx);
        self.drain_ctx_index();
        self.drain_update();
        while let Ok(mut st) = self.stats_rx.try_recv() {
            // The kip tree includes every session's shell as a descendant -
            // subtract them so the first row is kip itself, not the whole app.
            if let Some((first, rest)) = st.procs.split_first_mut() {
                for (_, cpu, rss) in rest.iter() {
                    first.1 = (first.1 - cpu).max(0.0);
                    first.2 = first.2.saturating_sub(*rss);
                }
            }
            self.stats = Some(st);
            self.stats_at = Some(Instant::now());
            self.stats_inflight = false;
        }

        // Paste without text (Finder file, screenshot image) -> insert as a path.
        let mut file_paste = false;
        ctx.input_mut(|i| {
            i.events.retain(|e| {
                if matches!(e, egui::Event::Paste(t) if t.trim().is_empty()) {
                    file_paste = true;
                    false
                } else {
                    true
                }
            })
        });
        if file_paste {
            if let Some(p) = plat::clipboard_paths() {
                self.insert_paths(shell_escape(&p));
            }
        }
        // Drag & drop of files from Finder.
        let dropped: Vec<String> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.as_ref().map(|p| p.to_string_lossy().into_owned()))
                .collect()
        });
        if !dropped.is_empty() {
            let text = dropped.iter().map(|p| shell_escape(p)).collect::<Vec<_>>().join(" ");
            self.insert_paths(text);
        }
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("dnd-overlay"),
            ));
            let r = ctx.content_rect();
            painter.rect_filled(r, 0.0, Color32::from_black_alpha(90));
            painter.rect_stroke(
                r.shrink(12.0),
                CornerRadius::same(10),
                Stroke::new(2.0, UNREAD),
                egui::StrokeKind::Inside,
            );
            painter.text(
                r.center(),
                Align2::CENTER_CENTER,
                tr("Отпусти - вставлю путь к файлу", "Drop to insert the file path"),
                FontId::proportional(15.0),
                TXT,
            );
        }
        self.housekeeping(&ctx);
        self.shortcuts(&ctx);

        if self.active.is_some() && self.active_idx().is_none() {
            self.active = self.sessions.first().map(|s| s.id);
        }
        self.active_shared.store(self.active.unwrap_or(0), Ordering::Relaxed);

        // Adopt zoom changed via Cmd+= / Cmd+- (egui built-in); while the
        // settings window is open the slider owns the value instead.
        if !self.settings_open {
            let z = ctx.zoom_factor();
            if (z - self.settings.ui_scale).abs() > 0.001 {
                self.settings.ui_scale = z;
            }
        }

        let acts = egui::Panel::left("sidebar")
            .exact_size(236.0)
            .resizable(false)
            .frame(Frame::new().fill(palette::chrome_sidebar()))
            .show(ui, |ui| self.sidebar(ui))
            .inner;
        self.apply(acts, &ctx);

        let acts = egui::Panel::bottom("statusbar")
            .exact_size(34.0)
            .frame(Frame::new().fill(palette::chrome_bar()))
            .show(ui, |ui| self.bottom_bar(ui))
            .inner;
        self.apply(acts, &ctx);

        let acts = egui::CentralPanel::default()
            .frame(Frame::new().fill(palette::term_bg()))
            .show(ui, |ui| self.central(ui))
            .inner;
        self.apply(acts, &ctx);

        if self.settings_open {
            self.settings_window(&ctx);
        }
        self.dir_popup(&ctx);
        self.stats_ui(&ctx);
    }

    fn on_exit(&mut self) {
        // Capture Claude session ids of live terminals so "Продолжить Claude"
        // is available after restart even without an explicit save.
        for s in &mut self.sessions {
            if matches!(s.phase, Phase::Live(_)) {
                s.save_claude_session();
            }
        }
        self.persist();
    }
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    // Bundled JetBrains Mono guarantees full Latin+Cyrillic coverage so the
    // terminal renders identically on every machine. macOS Menlo ships as a
    // .ttc collection that egui mis-loads, which dropped Cyrillic into a
    // proportional fallback (mixed fonts in the terminal). A system font is
    // inserted ahead of it below when present; jbmono stays in the family as
    // the fallback that backs any glyph the system font is missing.
    fonts.font_data.insert(
        "jbmono".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../resources/JetBrainsMono-Regular.ttf"
        ))),
    );
    fonts.families.get_mut(&FontFamily::Monospace).unwrap().insert(0, "jbmono".into());

    // Prefer a native single-file system mono when present (SF Mono on macOS),
    // with jbmono behind it as the coverage fallback. Menlo.ttc is left out on
    // purpose - it is the collection that fails to load.
    let candidates = [
        "/System/Library/Fonts/SFNSMono.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "C:\\Windows\\Fonts\\consola.ttf",
        "C:\\Windows\\Fonts\\CascadiaMono.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("sys-mono".into(), Arc::new(FontData::from_owned(bytes)));
            fonts.families.get_mut(&FontFamily::Monospace).unwrap().insert(0, "sys-mono".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn rgb32([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn apply_style(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.panel_fill = palette::chrome_sidebar();
    v.window_fill = Color32::from_rgb(0x20, 0x20, 0x20);
    v.extreme_bg_color = Color32::from_rgb(0x14, 0x14, 0x14);
    v.override_text_color = None;
    v.selection.bg_fill = palette::selection();
    v.widgets.noninteractive.fg_stroke.color = TXT;
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x26, 0x26, 0x26);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x26, 0x26, 0x26);
    v.widgets.inactive.fg_stroke.color = TXT;
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x30, 0x30, 0x30);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x30, 0x30, 0x30);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x45, 0x45, 0x45));
    v.widgets.active.bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x3a);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(0x3a, 0x3a, 0x3a);
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(0x38, 0x38, 0x38));
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(5);
    }
    ctx.set_visuals(v);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
    });
}

fn shell_history_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    for name in [".zsh_history", ".bash_history"] {
        let p = home.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// Tail of the shell history file, most recent last, deduplicated.
/// Handles zsh extended format (`: ts:dur;cmd`) and zsh metafication.
fn load_shell_history() -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = shell_history_path() else { return Vec::new() };
    let Ok(mut f) = std::fs::File::open(&path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // Read essentially the whole history (cap the tail only to guard against a
    // pathologically huge file); the entry cap below is the real bound.
    let tail = 64 * 1024 * 1024;
    if len > tail {
        let _ = f.seek(SeekFrom::Start(len - tail));
    }
    let mut raw = Vec::new();
    if f.read_to_end(&mut raw).is_err() {
        return Vec::new();
    }
    // zsh "metafies" bytes >= 0x83 in the histfile: 0x83 escapes the next byte ^ 0x20.
    let mut bytes = Vec::with_capacity(raw.len());
    let mut it = raw.iter();
    while let Some(&b) = it.next() {
        if b == 0x83 {
            if let Some(&n) = it.next() {
                bytes.push(n ^ 0x20);
            }
        } else {
            bytes.push(b);
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().rev() {
        let cmd = if line.starts_with(": ") {
            line.split_once(';').map_or(line, |(_, rest)| rest)
        } else {
            line
        }
        .trim_end_matches('\\')
        .trim();
        if cmd.is_empty() || !seen.insert(cmd.to_string()) {
            continue;
        }
        out.push(cmd.to_string());
        if out.len() >= 200_000 {
            break;
        }
    }
    out.reverse();
    out
}


/// Claude marker: an 8-ray star drawn with four crossing lines.
fn draw_star(p: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    for k in 0..4 {
        let a = k as f32 * std::f32::consts::FRAC_PI_4;
        let d = Vec2::new(a.cos(), a.sin()) * r;
        p.line_segment([c - d, c + d], Stroke::new(1.5, color));
    }
}

/// Keep the tail of a long path/string, char-boundary safe.
fn truncate_head(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 3)).collect();
    format!("...{tail}")
}

/// Keep the head, char-boundary safe.
fn truncate_end(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max - 3).collect();
    format!("{head}...")
}

/// Unmodified key press, consumed so no widget sees it.
fn consume_plain(i: &mut egui::InputState, target: Key) -> bool {
    let mut hit = false;
    i.events.retain(|e| {
        if !hit {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = e {
                if modifiers.is_none() && *key == target {
                    hit = true;
                    return false;
                }
            }
        }
        true
    });
    hit
}

/// Cmd+key shortcut match on both logical and physical key, so it works in any keyboard layout.
fn consume_cmd(i: &mut egui::InputState, target: Key) -> bool {
    let mut hit = false;
    i.events.retain(|e| {
        if !hit {
            if let egui::Event::Key { key, physical_key, pressed: true, modifiers, .. } = e {
                if modifiers.matches_logically(Modifiers::COMMAND)
                    && (*key == target || *physical_key == Some(target))
                {
                    hit = true;
                    return false;
                }
            }
        }
        true
    });
    hit
}

fn tilde(path: &std::path::Path) -> String {
    let p = path.to_string_lossy();
    if let Some(home) = dirs::home_dir() {
        let h = home.to_string_lossy();
        if let Some(rest) = p.strip_prefix(h.as_ref()) {
            return format!("~{rest}");
        }
    }
    p.into_owned()
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Backslash-escape a path for the shell; claude also understands this form.
fn shell_escape(s: &str) -> String {
    let plain = |c: char| c.is_alphanumeric() || "/._-~+@%:=".contains(c);
    if !s.is_empty() && s.chars().all(plain) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        // Control chars cannot be backslash-escaped (\<newline> is a line
        // continuation in shell) - drop them, such paths are broken anyway.
        if c.is_control() {
            continue;
        }
        if !plain(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn fmt_mem(rss_kb: u64) -> String {
    if rss_kb < 1024 * 1024 {
        format!("{} MB", rss_kb / 1024)
    } else {
        format!("{:.1} GB", rss_kb as f64 / 1024.0 / 1024.0)
    }
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}{}", tr("с", "s"))
    } else if s < 3600 {
        format!("{}{} {}{}", s / 60, tr("м", "m"), s % 60, tr("с", "s"))
    } else {
        format!("{}{} {}{}", s / 3600, tr("ч", "h"), s % 3600 / 60, tr("м", "m"))
    }
}
