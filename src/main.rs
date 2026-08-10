// Release builds on Windows are GUI apps: suppress the extra console window.
// Debug keeps it so panics and logs stay visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod ctx_index;
mod i18n;
#[cfg(target_os = "macos")]
mod mac_service;
mod plat;
mod palette;
mod session;
mod term_view;
mod update;
mod usage;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event as TermEvent, Notify, WindowSize};
use alacritty_terminal::term::TermMode;
use eframe::egui;
use egui::text::CCursor;
use egui::text_selection::CCursorRange;
use egui::{
    Align, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Frame,
    Key, Layout, Margin, Modifiers, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    Vec2, Visuals,
};

use config::{load_state, save_state, AppState, SavedSession, Settings};
use i18n::tr;
use session::{poll_git, spawn_live, EventProxy, GitStats, Phase, Session};

const TICK: Duration = Duration::from_secs(2);
const GIT_INTERVAL: Duration = Duration::from_secs(7);
/// Transcript re-check for sessions claude is not currently running in.
const IDLE_SCAN: Duration = Duration::from_secs(10);
const BUSY_NOTIFY_MIN: Duration = Duration::from_secs(5);
/// How long a pinned row's refusal wobble lasts.
const SHAKE_SECS: f32 = 0.45;
/// Narrowest usable session panel: below this a row's name loses its path.
const SIDEBAR_MIN: f32 = 170.0;

// Theme-adaptive text/surface colors live in palette:: (text(), popup_bg(),
// border(), ...). These mid-tone status accents read on both light and dark.
const GIT_ADD: Color32 = Color32::from_rgb(0x8f, 0xb5, 0x7a);
const GIT_DEL: Color32 = Color32::from_rgb(0xc4, 0x7a, 0x7a);
const DOT_BUSY: Color32 = Color32::from_rgb(0x8f, 0xb5, 0x7a);
const DOT_LIVE: Color32 = Color32::from_rgb(0x8a, 0x8a, 0x8a);
const DOT_EXITED: Color32 = Color32::from_rgb(0xb0, 0x70, 0x70);
const UNREAD: Color32 = Color32::from_rgb(0x9c, 0xb5, 0xcc);
const ORANGE: Color32 = Color32::from_rgb(0xd4, 0xa0, 0x5a);

fn main() -> eframe::Result {
    // If kip itself was launched from a Claude Code session, its CLAUDE* markers
    // leak into our shells and a claude started there thinks it is a child session
    // and disables transcript saving (breaking resume and context tracking).
    // CLAUDE_CONFIG_DIR is configuration, not a nesting marker: claude_dir() reads
    // it to find sessions/transcripts, and dropping it here would leave us looking
    // in ~/.claude while the shells (which re-export it from the user's rc) run a
    // claude that writes somewhere else.
    let claude_vars: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("CLAUDE") && k != "CLAUDE_CONFIG_DIR")
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
    /// bool = resume the saved Claude session (vs plain shell). Either way the
    /// same session is revived in place - no new tab.
    Resume(u64, bool),
    /// Type the resume command into a session's LIVE shell. Unlike `Resume` this
    /// touches no PTY: the terminal keeps its cwd, environment and scrollback.
    ResumeInPlace(u64),
    /// Cmd+W: suspend a live session, remove a frozen one.
    Close(u64),
    /// X button / "Удалить": always removes, killing a live session.
    Remove(u64),
    ToggleAwake(u64),
    /// Protect a session: pinned rows refuse to close, drag or auto-suspend.
    TogglePin(u64),
    /// Start inline editing of a session's title.
    BeginRename(u64),
    /// Commit the rename buffer as the session's custom name (empty = reset to auto).
    RenameCommit(u64),
    RenameCancel,
    /// Move a session: set its group and drop it before `before` (None = end of
    /// that group's run). Backs both drag & drop and the context-menu group menu.
    MoveSession { id: u64, group: Option<String>, before: Option<u64> },
    /// Reorder a whole group before `before` group (None = last), by moving its
    /// contiguous block of sessions.
    MoveGroup { name: String, before: Option<String> },
    /// Put a session into a fresh group and immediately rename that group.
    NewGroup(u64),
    ToggleGroup(String),
    BeginRenameGroup(String),
    RenameGroupCommit(String),
    /// Dissolve a group: its members become ungrouped.
    DeleteGroup(String),
    Settings,
    /// Show/hide the file explorer panel.
    ToggleExplorer,
}

/// Lightweight file explorer panel (between the session list and the terminal).
#[derive(Default, PartialEq, Clone, Copy)]
enum ExMode {
    #[default]
    Browse,
    Search,
    New,
}

struct ExEntry {
    name: String,
    is_dir: bool,
}

#[derive(Default)]
struct Explorer {
    open: bool,
    dir: PathBuf,
    mode: ExMode,
    query: String,
    new_name: String,
    /// Grab keyboard focus for the active input on the frame it opens.
    focus: bool,
    /// Cached listing of `dir` (browse mode) and the dir it was read for.
    list: Vec<ExEntry>,
    list_dir: Option<PathBuf>,
    list_at: Option<Instant>,
    /// Cached recursive search hits and the (dir, query) they were computed for.
    hits: Vec<PathBuf>,
    hits_key: Option<(PathBuf, String)>,
}

/// One entry in the sidebar's vertical layout (a group header or a session row),
/// captured each frame to resolve where a drag would drop.
struct DropItem {
    rect: Rect,
    id: Option<u64>,
    group: Option<String>,
    is_header: bool,
}

struct DropTarget {
    group: Option<String>,
    before: Option<u64>,
    line_y: f32,
}

/// One line of the sidebar in display order.
enum Slot {
    Header(String),
    Row(u64),
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
    /// Slow cadence of the transcript pass for sessions without a live claude.
    last_idle_scan: Option<Instant>,
    hook_error: Option<String>,
    update_state: UpdateState,
    upd_tx: Sender<update::UpdateMsg>,
    upd_rx: Receiver<update::UpdateMsg>,
    settings_open: bool,
    last_tick: Instant,
    /// When the last background update check ran.
    last_update_check: Instant,
    /// Id of the session whose title is being edited inline (double-click rename).
    renaming: Option<u64>,
    /// Name of the group whose header is being renamed inline (mutually exclusive
    /// with `renaming`; both feed `rename_buf`).
    renaming_group: Option<String>,
    rename_buf: String,
    /// Grab keyboard focus for the rename field on its first frame only.
    rename_focus: bool,
    /// Session id being dragged in the sidebar (reorder / move between groups).
    dragging: Option<u64>,
    /// A blocked action (close, drag, move) on a pinned session: its pin shakes
    /// instead, so the refusal is visible where the reason lives.
    pin_shake: Option<(u64, Instant)>,
    /// Group whose header is being dragged (reorder groups among themselves).
    dragging_group: Option<String>,
    /// Collapsed group names.
    collapsed_groups: HashSet<String>,
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
    /// Background histfile reads: (entries, lowercase mirror).
    hist_tx: Sender<(Vec<String>, Vec<String>)>,
    hist_rx: Receiver<(Vec<String>, Vec<String>)>,
    hist_inflight: bool,
    /// A read has landed at least once (an absent histfile stays empty forever,
    /// and must not re-spawn a reader every 5s).
    hist_loaded: bool,
    /// Directory switcher popup over the path chip.
    dir_open: bool,
    dir_query: String,
    dir_path: PathBuf,
    /// Unfiltered subdirs of dir_path, refreshed at most every 2s while the popup is open.
    dir_cache: Vec<String>,
    dir_cache_at: Option<(PathBuf, Instant)>,
    /// Inline "new folder" editor inside the switcher: Some = its name is being
    /// typed. `new_dir_focus` selects the placeholder on the first frame so that
    /// typing replaces it.
    new_dir: Option<String>,
    new_dir_focus: bool,
    new_dir_err: Option<String>,
    chip_rect: Option<Rect>,
    stats_tx: Sender<plat::SysStats>,
    stats_rx: Receiver<plat::SysStats>,
    stats: Option<plat::SysStats>,
    stats_at: Option<Instant>,
    stats_inflight: bool,
    /// Popup rect from the previous frame, to keep it open while hovered.
    stats_rect: Option<Rect>,
    usage_tx: Sender<usage::UsageMsg>,
    usage_rx: Receiver<usage::UsageMsg>,
    /// Claude.ai subscription limits, whatever the account has (session, week,
    /// per-model week). Empty until the first fetch lands.
    usage: Vec<usage::Limit>,
    usage_err: Option<String>,
    usage_at: Option<Instant>,
    usage_inflight: bool,
    usage_rect: Option<Rect>,
    /// Last measured terminal cell size in px, used for PTY pixel hints.
    cell: (u16, u16),
    /// Last measured grid size, used as the initial size for new PTYs.
    grid: (u16, u16),
    explorer: Explorer,
    /// Font actually in use - the pick can silently fall back (settings show it).
    font_applied: &'static str,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let state = load_state();
        let font_applied = install_fonts(&cc.egui_ctx, &state.settings.font);
        i18n::set(i18n::resolve(&state.settings.lang));
        palette::apply(&state.settings.theme, state.settings.accent.map(rgb32));
        apply_style(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(state.settings.ui_scale.clamp(0.5, 2.0));
        let (ev_tx, ev_rx) = mpsc::channel();
        let (git_tx, git_rx) = mpsc::channel();
        let (ctx_tx, ctx_rx) = mpsc::channel();
        let (ctxi_tx, ctxi_rx) = mpsc::channel();
        let (stats_tx, stats_rx) = mpsc::channel();
        let (usage_tx, usage_rx) = mpsc::channel();
        let (upd_tx, upd_rx) = mpsc::channel();
        let (hist_tx, hist_rx) = mpsc::channel();
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
            last_idle_scan: None,
            hook_error: None,
            update_state: UpdateState::Idle,
            upd_tx,
            upd_rx,
            active_shared: Arc::new(AtomicU64::new(0)),
            settings_open: false,
            last_tick: Instant::now(),
            last_update_check: Instant::now(),
            renaming: None,
            renaming_group: None,
            rename_buf: String::new(),
            rename_focus: false,
            dragging: None,
            pin_shake: None,
            dragging_group: None,
            collapsed_groups: state.collapsed_groups.into_iter().collect(),
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
            hist_tx,
            hist_rx,
            hist_inflight: false,
            hist_loaded: false,
            dir_open: false,
            dir_query: String::new(),
            dir_path: dirs::home_dir().unwrap_or_else(|| "/".into()),
            dir_cache: Vec::new(),
            dir_cache_at: None,
            new_dir: None,
            new_dir_focus: false,
            new_dir_err: None,
            chip_rect: None,
            stats_tx,
            stats_rx,
            stats: None,
            stats_at: None,
            stats_inflight: false,
            stats_rect: None,
            usage_tx,
            usage_rx,
            usage: Vec::new(),
            usage_err: None,
            usage_at: None,
            usage_inflight: false,
            usage_rect: None,
            cell: (8, 17),
            grid: (100, 28),
            explorer: Explorer::default(),
            font_applied,
        };
        for saved in state.sessions {
            let id = app.next_id;
            app.next_id += 1;
            app.sessions.push(Session::from_saved(saved, id));
        }
        // Saved state from older builds may not be in display order; make the vec
        // contiguous so drag/drop math is correct from the first interaction.
        app.normalize_order();
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
        // Finder "New kip Window Here" -> a new session at the picked folder.
        #[cfg(target_os = "macos")]
        {
            mac_service::register(cc.egui_ctx.clone());
            mac_service::hook_paste();
        }
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
        // Only keep collapse state for groups that still exist.
        let live: HashSet<&String> = self.sessions.iter().filter_map(|s| s.group.as_ref()).collect();
        save_state(&AppState {
            settings: self.settings.clone(),
            sessions: self.sessions.iter().map(|s| s.to_saved()).collect(),
            collapsed_groups: self
                .collapsed_groups
                .iter()
                .filter(|g| live.contains(g))
                .cloned()
                .collect(),
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

    /// Re-read the shell history file when it changes (throttled). The read,
    /// the dedup and the lowercase mirror are the expensive part - tens of ms on
    /// a multi-megabyte histfile - and EVERY command run in any shell touches the
    /// file's mtime, so this fires regularly while working. It runs off the UI
    /// thread and lands in `drain_history`.
    fn refresh_history(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.last_hist_check.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            return;
        }
        self.last_hist_check = Some(now);
        let mtime = shell_history_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        if self.hist_inflight || (mtime == self.hist_mtime && self.hist_loaded) {
            return;
        }
        self.hist_mtime = mtime;
        self.hist_inflight = true;
        let tx = self.hist_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let hist = load_shell_history();
            let lc: Vec<String> = hist.iter().map(|h| h.to_lowercase()).collect();
            if tx.send((hist, lc)).is_ok() {
                ctx.request_repaint();
            }
        });
    }

    /// Adopt a finished background read. `session_cmds` is the authoritative
    /// record of what THIS run sent, replayed on top of every snapshot - so a
    /// command submitted while the read was in flight is not swallowed by it.
    fn drain_history(&mut self) {
        while let Ok((mut hist, mut lc)) = self.hist_rx.try_recv() {
            self.hist_inflight = false;
            self.hist_loaded = true;
            for cmd in &self.session_cmds {
                if let Some(p) = hist.iter().position(|h| h == cmd) {
                    hist.remove(p);
                    lc.remove(p);
                }
                hist.push(cmd.clone());
                lc.push(cmd.to_lowercase());
            }
            self.history = hist;
            self.history_lc = lc;
        }
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

    /// A pinned session refuses close/drag/move: true means "handled - do
    /// nothing", with the pin shaking so the refusal is not silent.
    fn refuse_pinned(&mut self, idx: usize) -> bool {
        if !self.sessions[idx].pinned {
            return false;
        }
        self.pin_shake = Some((self.sessions[idx].id, Instant::now()));
        true
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

    /// Reorder within the session vec and set the dragged session's group.
    /// `before` = the session to drop in front of (None = end of that group's run).
    fn move_session(&mut self, id: u64, group: Option<String>, before: Option<u64>) {
        if before == Some(id) {
            // Dropped onto itself; only the group might still need updating.
            if let Some(i) = self.idx_of(id) {
                if self.sessions[i].group != group {
                    self.sessions[i].group = group;
                    self.persist();
                }
            }
            return;
        }
        let anchors = self.pin_anchors();
        let Some(pos) = self.idx_of(id) else { return };
        let mut s = self.sessions.remove(pos);
        s.group = group.clone();
        let insert = match before.and_then(|b| self.idx_of(b)) {
            Some(p) => p,
            None => match self.sessions.iter().rposition(|x| x.group == group) {
                Some(p) => p + 1,
                None => {
                    // No members yet: ungrouped lands above the first group, a
                    // brand-new group at the very end.
                    if group.is_none() {
                        self.sessions.iter().position(|x| x.group.is_some()).unwrap_or(self.sessions.len())
                    } else {
                        self.sessions.len()
                    }
                },
            },
        };
        self.sessions.insert(insert, s);
        self.normalize_order();
        // Everything moves freely past a pinned row - the pinned row itself just
        // never leaves its slot number, so whoever lands on it is pushed one
        // place further down.
        self.reseat_pinned(&anchors);
        self.persist();
    }

    /// Slot each pinned session occupies right now, counted inside its own group
    /// (or the ungrouped block). Captured before a move, replayed after it.
    fn pin_anchors(&self) -> Vec<(u64, usize)> {
        self.sessions
            .iter()
            .filter(|s| s.pinned)
            .map(|s| {
                let slot = self
                    .sessions
                    .iter()
                    .filter(|x| x.group == s.group)
                    .position(|x| x.id == s.id)
                    .unwrap_or(0);
                (s.id, slot)
            })
            .collect()
    }

    fn reseat_pinned(&mut self, anchors: &[(u64, usize)]) {
        if anchors.is_empty() {
            return;
        }
        let mut order: Vec<u64> = Vec::with_capacity(self.sessions.len());
        let mut done: Vec<Option<String>> = Vec::new();
        for g in self.sessions.iter().map(|s| s.group.clone()) {
            if done.contains(&g) {
                continue;
            }
            let members: Vec<u64> =
                self.sessions.iter().filter(|x| x.group == g).map(|x| x.id).collect();
            order.extend(seat_pinned(&members, anchors));
            done.push(g);
        }
        self.sessions.sort_by_key(|s| order.iter().position(|&id| id == s.id).unwrap_or(usize::MAX));
    }

    /// Move a whole group's contiguous block of sessions before `before`'s block
    /// (None = after all groups).
    fn move_group(&mut self, name: String, before: Option<String>) {
        if before.as_deref() == Some(name.as_str()) {
            return;
        }
        let mut block = Vec::new();
        let mut rest = Vec::new();
        for s in self.sessions.drain(..) {
            if s.group.as_deref() == Some(name.as_str()) {
                block.push(s);
            } else {
                rest.push(s);
            }
        }
        self.sessions = rest;
        if block.is_empty() {
            return;
        }
        let at = match before {
            Some(h) => self
                .sessions
                .iter()
                .position(|s| s.group.as_deref() == Some(h.as_str()))
                .unwrap_or(self.sessions.len()),
            None => self.sessions.len(),
        };
        for (k, s) in block.into_iter().enumerate() {
            self.sessions.insert(at + k, s);
        }
        self.persist();
    }

    /// Where a dragged group would land: the group to insert before (None = end).
    /// Groups are treated as blocks spanning from their header to the last member.
    fn resolve_group_drop(&self, items: &[DropItem], py: f32) -> Option<(Option<String>, f32)> {
        let mut blocks: Vec<(String, f32, f32)> = Vec::new();
        let mut i = 0;
        while i < items.len() {
            if items[i].is_header {
                let name = items[i].group.clone()?;
                let top = items[i].rect.top();
                let mut bottom = items[i].rect.bottom();
                let mut j = i + 1;
                while j < items.len() && !items[j].is_header {
                    bottom = items[j].rect.bottom();
                    j += 1;
                }
                blocks.push((name, top, bottom));
                i = j;
            } else {
                i += 1;
            }
        }
        let first = blocks.first()?;
        if py < first.1 {
            return Some((Some(first.0.clone()), first.1));
        }
        let last = blocks.last()?;
        if py > last.2 {
            return Some((None, last.2));
        }
        for (k, (_, top, bottom)) in blocks.iter().enumerate() {
            if py >= *top && py <= *bottom {
                return if py < (top + bottom) / 2.0 {
                    Some((Some(blocks[k].0.clone()), *top))
                } else {
                    let next = blocks.get(k + 1).map(|b| b.0.clone());
                    Some((next, *bottom))
                };
            }
        }
        Some((None, last.2))
    }

    fn unique_group_name(&self) -> String {
        let base = tr("Группа", "Group");
        let taken: HashSet<&str> = self.sessions.iter().filter_map(|s| s.group.as_deref()).collect();
        if !taken.contains(base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base} {n}")).find(|c| !taken.contains(c.as_str())).unwrap()
    }

    /// Where a drop at pointer-y `py` would land, given the frame's row/header
    /// geometry. `before`/`group` feed `move_session`; `line_y` draws the marker.
    fn resolve_drop(&self, items: &[DropItem], py: f32) -> Option<DropTarget> {
        let first = items.first()?;
        if py < first.rect.top() {
            return Some(DropTarget { group: first.group.clone(), before: first.id, line_y: first.rect.top() });
        }
        let last = items.last()?;
        if py > last.rect.bottom() {
            let g = items.iter().rev().find(|i| i.id.is_some()).and_then(|i| i.group.clone());
            return Some(DropTarget { group: g, before: None, line_y: last.rect.bottom() });
        }
        // The item under the pointer, else the nearest one above it.
        let hit = items
            .iter()
            .find(|i| py >= i.rect.top() && py <= i.rect.bottom())
            .or_else(|| items.iter().rfind(|i| i.rect.bottom() <= py))
            .unwrap_or(first);
        if hit.is_header {
            let before = self.sessions.iter().find(|s| s.group == hit.group).map(|s| s.id);
            return Some(DropTarget { group: hit.group.clone(), before, line_y: hit.rect.bottom() });
        }
        let sid = hit.id?;
        if py < hit.rect.center().y {
            Some(DropTarget { group: hit.group.clone(), before: Some(sid), line_y: hit.rect.top() })
        } else {
            let next = self
                .idx_of(sid)
                .and_then(|p| self.sessions.get(p + 1))
                .filter(|n| n.group == hit.group)
                .map(|n| n.id);
            Some(DropTarget { group: hit.group.clone(), before: next, line_y: hit.rect.bottom() })
        }
    }

    /// Write back a pending inline rename instead of dropping it. The editor
    /// loses focus and the click that ended it land in the SAME frame, and
    /// sidebar rows queue their actions top-down: clicking a row above the one
    /// being renamed put Select first, which used to clear `renaming` and turn
    /// the queued RenameCommit into a no-op, silently losing the typed name.
    /// Empty buffer clears the override, same as RenameCommit.
    fn commit_rename(&mut self) {
        let Some(id) = self.renaming.take() else { return };
        let Some(idx) = self.idx_of(id) else { return };
        let name = self.rename_buf.trim();
        self.sessions[idx].custom_name = (!name.is_empty()).then(|| name.to_string());
        self.persist();
    }

    /// Switch the active session, dropping per-session UI state (popup, input, history nav).
    fn set_active(&mut self, id: Option<u64>) {
        self.commit_rename();
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
        // A new terminal joins the active session's group (create a sibling in
        // whatever project/group you are looking at).
        let group = self.active_idx().and_then(|i| self.sessions[i].group.clone());
        let mut s = Session::from_saved(
            SavedSession {
                cwd,
                claude_session_id: None,
                claude_title: None,
                custom_name: None,
                group: group.clone(),
                skip_permissions: self.settings.skip_permissions_default,
                keep_awake: false,
                pinned: false,
                snapshot: None,
            },
            id,
        );
        self.attach_live(&mut s, command, None, ctx);
        // Keep it next to its group; ungrouped sessions stay a contiguous block
        // above the groups instead of landing below them (which would break the
        // ungrouped-run adjacency that drag/drop relies on).
        let pos = match &group {
            Some(g) => self
                .sessions
                .iter()
                .rposition(|x| x.group.as_ref() == Some(g))
                .map(|p| p + 1)
                .unwrap_or(self.sessions.len()),
            None => self.sessions.iter().position(|x| x.group.is_some()).unwrap_or(self.sessions.len()),
        };
        self.sessions.insert(pos, s);
        self.normalize_order();
        self.set_active(Some(id));
        self.persist();
    }

    fn attach_live(&self, s: &mut Session, command: Option<String>, seed: Option<&str>, ctx: &egui::Context) {
        let proxy = EventProxy {
            id: s.id,
            tx: self.ev_tx.clone(),
            ctx: ctx.clone(),
            active: self.active_shared.clone(),
        };
        match spawn_live(s.id, &s.cwd, command, &self.settings, proxy, self.grid.0, self.grid.1, self.cell, seed) {
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
        // Reopening as a plain terminal: seed the fresh shell's scrollback with
        // the suspended snapshot so the on-screen history is not wiped. Claude
        // resume redraws its own conversation, so it needs no seed.
        let seed = (!with_claude)
            .then_some(s.snapshot.as_deref())
            .flatten()
            .map(|snap| {
                format!(
                    "{}\r\n\x1b[90m{}\x1b[0m\r\n",
                    // Normalize first so an existing CRLF does not become CR CR LF.
                    snap.replace("\r\n", "\n").replace('\n', "\r\n"),
                    tr(
                        "---- терминал открыт заново, история выше ----",
                        "---- terminal reopened, history above ----",
                    ),
                )
            });
        self.attach_live(&mut s, command, seed.as_deref(), ctx);
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
                Act::ResumeInPlace(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        if let Some(cmd) = self.sessions[idx].resume_command(&self.settings) {
                            self.send_command_to(idx, &cmd, ctx);
                        }
                    }
                },
                Act::Close(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        if self.refuse_pinned(idx) {
                            continue;
                        }
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
                        if !self.refuse_pinned(idx) {
                            self.remove_session(idx);
                        }
                    }
                },
                Act::ToggleAwake(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        self.sessions[idx].keep_awake = !self.sessions[idx].keep_awake;
                        self.persist();
                    }
                },
                Act::TogglePin(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        self.sessions[idx].pinned = !self.sessions[idx].pinned;
                        self.persist();
                    }
                },
                Act::BeginRename(id) => {
                    if let Some(idx) = self.idx_of(id) {
                        self.renaming = Some(id);
                        self.renaming_group = None;
                        self.rename_buf = self.sessions[idx].display_name();
                        self.rename_focus = true;
                    }
                },
                Act::RenameCommit(id) => {
                    if self.renaming == Some(id) {
                        if let Some(idx) = self.idx_of(id) {
                            let name = self.rename_buf.trim();
                            // Empty = clear the override, falling back to the
                            // Claude/directory name.
                            self.sessions[idx].custom_name =
                                (!name.is_empty()).then(|| name.to_string());
                            self.persist();
                        }
                        self.renaming = None;
                    }
                },
                Act::RenameCancel => {
                    self.renaming = None;
                    self.renaming_group = None;
                },
                Act::MoveSession { id, group, before } => {
                    if let Some(idx) = self.idx_of(id) {
                        if !self.refuse_pinned(idx) {
                            self.move_session(id, group, before);
                        }
                    }
                },
                Act::MoveGroup { name, before } => self.move_group(name, before),
                Act::NewGroup(id) => {
                    let name = self.unique_group_name();
                    self.move_session(id, Some(name.clone()), None);
                    self.renaming_group = Some(name.clone());
                    self.renaming = None;
                    self.rename_buf = name;
                    self.rename_focus = true;
                },
                Act::ToggleGroup(name) => {
                    if !self.collapsed_groups.remove(&name) {
                        self.collapsed_groups.insert(name);
                    }
                    self.persist();
                },
                Act::BeginRenameGroup(name) => {
                    self.rename_buf = name.clone();
                    self.renaming_group = Some(name);
                    self.renaming = None;
                    self.rename_focus = true;
                },
                Act::RenameGroupCommit(old) => {
                    if self.renaming_group.as_deref() == Some(old.as_str()) {
                        let new = self.rename_buf.trim().to_string();
                        // Empty or a name that already exists = keep the old one.
                        let clash = self.sessions.iter().any(|s| s.group.as_deref() == Some(new.as_str()));
                        if !new.is_empty() && (new == old || !clash) {
                            for s in &mut self.sessions {
                                if s.group.as_deref() == Some(old.as_str()) {
                                    s.group = Some(new.clone());
                                }
                            }
                            if self.collapsed_groups.remove(&old) {
                                self.collapsed_groups.insert(new);
                            }
                            self.persist();
                        }
                        self.renaming_group = None;
                    }
                },
                Act::DeleteGroup(name) => {
                    for s in &mut self.sessions {
                        if s.group.as_deref() == Some(name.as_str()) {
                            s.group = None;
                        }
                    }
                    self.collapsed_groups.remove(&name);
                    // Ex-members were ungrouped in place, mid-vec; pull them back
                    // into the ungrouped block so ordering stays contiguous.
                    self.normalize_order();
                    self.persist();
                },
                Act::Settings => self.settings_open = !self.settings_open,
                Act::ToggleExplorer => {
                    self.explorer.open = !self.explorer.open;
                    if self.explorer.open {
                        // Open on the active session's directory each time.
                        self.explorer.dir = self.active_cwd();
                        self.explorer.mode = ExMode::Browse;
                        self.explorer.list_dir = None;
                    }
                },
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
            let mut kill_pids: Vec<i32> = Vec::new();

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
                    && !s.pinned
                    && (!busy_now || is_claude)
                    && s.last_activity.elapsed().as_secs() >= idle_limit
                {
                    // Disarm the per-session kill and collect the pid: this sweep
                    // can suspend several sessions at once, and LiveTerm::drop
                    // would run a `ps` for each of them right here on the UI
                    // thread. One batched snapshot below does the same work once.
                    if let Phase::Live(l) = &mut s.phase {
                        l.kill_on_drop = false;
                    }
                    kill_pids.push(shell_pid);
                    s.suspend();
                    suspend_any = true;
                }
            }
            if !kill_pids.is_empty() {
                plat::kill_trees(&kill_pids);
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
        }

        // Transcript pass, deliberately outside the loop above: that one only
        // follows the tab claude is running in, but the last-reply time has to
        // be right for a suspended, exited or just-restored tab too - and the
        // transcript is the only source that survives a restart. Live claude
        // keeps the 500ms cadence, everything else is re-checked every
        // IDLE_SCAN, and a file whose (mtime, len) has not moved is never read.
        let slow = self.last_idle_scan.is_none_or(|t| now.duration_since(t) >= IDLE_SCAN);
        if slow {
            self.last_idle_scan = Some(now);
        }
        for s in &mut self.sessions {
            let fast = s.fg_is_claude || s.burst_until.is_some_and(|t| now < t);
            if !fast && !slow {
                continue;
            }
            let Some(sid) = s.claude_session_id.clone() else { continue };
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
                        ctx_index::spawn_estimate(sid, p, tx.clone(), ctx.clone());
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
                        ctx_index::lookup(sid, s.cwd.clone(), map.clone(), tx.clone(), ctx.clone());
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
        // A rename editor can outlive its target: the last group member gets
        // dragged out, the group is dissolved, Cmd+W closes the renamed session,
        // or its group is collapsed and the row stops being drawn. The editor
        // then stops being drawn, so nothing commits or cancels it, and a stuck
        // renaming* flag freezes terminal input (accept/interactive gate on it).
        // Commit the orphan so input never dead-ends and the edit is not lost.
        let orphan = self.renaming.is_some_and(|id| match self.idx_of(id) {
            None => true,
            Some(i) => self.sessions[i]
                .group
                .as_deref()
                .is_some_and(|g| self.collapsed_groups.contains(g)),
        });
        if orphan {
            self.commit_rename();
        }
        if self
            .renaming_group
            .as_deref()
            .is_some_and(|g| !self.sessions.iter().any(|s| s.group.as_deref() == Some(g)))
        {
            self.renaming_group = None;
        }
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
            ui.add_space(4.0);
            let files = ui
                .selectable_label(self.explorer.open, RichText::new(tr("Файлы", "Files")).size(12.5))
                .on_hover_text(tr("Проводник файлов", "File explorer"));
            if files.clicked() {
                acts.push(Act::ToggleExplorer);
            }
        });
        ui.add_space(8.0);

        let mut items: Vec<DropItem> = Vec::new();
        let mut released = false;
        let mut group_released = false;
        // Optional hairline in the gap above each row. Drawn here rather than
        // inside the rows because only this loop knows what came before: the
        // first line drawn would be a stray rule under the toolbar.
        let sep = self.settings.row_separators;
        let mut first = true;
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let rule = |ui: &egui::Ui, rect: Rect, first: &mut bool| {
                if sep && !*first {
                    let x = ui.max_rect().x_range();
                    ui.painter().hline(
                        egui::Rangef::new(x.min + 10.0, x.max - 10.0),
                        rect.min.y - 3.0,
                        Stroke::new(1.0, palette::border_dim()),
                    );
                }
                *first = false;
            };
            for slot in self.sidebar_order() {
                match slot {
                    Slot::Header(name) => {
                        let collapsed = self.collapsed_groups.contains(&name);
                        let (rect, rel) = self.group_header(ui, &name, collapsed, &mut acts);
                        rule(ui, rect, &mut first);
                        group_released |= rel;
                        items.push(DropItem { rect, id: None, group: Some(name), is_header: true });
                    },
                    Slot::Row(id) => {
                        let idx = self.idx_of(id).unwrap();
                        let group = self.sessions[idx].group.clone();
                        // Hide members of a collapsed group (header still shows).
                        if group.as_ref().is_some_and(|g| self.collapsed_groups.contains(g)) {
                            continue;
                        }
                        // Members sit indented under their header.
                        let indent = if group.is_some() { 14.0 } else { 0.0 };
                        let (rect, rel) = self.session_row(ui, idx, indent, &mut acts);
                        rule(ui, rect, &mut first);
                        released |= rel;
                        items.push(DropItem { rect, id: Some(id), group, is_header: false });
                    },
                }
            }
            ui.add_space(40.0);
        });

        // Drag overlay: insertion marker every frame, the move on release.
        let py = ui.input(|i| i.pointer.interact_pos()).map(|p| p.y);
        if let Some(drag) = self.dragging {
            if let Some(tgt) = py.and_then(|py| self.resolve_drop(&items, py)) {
                let x = ui.max_rect().x_range();
                ui.painter().hline(x, tgt.line_y, Stroke::new(2.0, palette::accent_bar()));
                if released {
                    acts.push(Act::MoveSession { id: drag, group: tgt.group, before: tgt.before });
                }
            }
            ui.ctx().request_repaint();
        } else if let Some(name) = self.dragging_group.clone() {
            if let Some((before, line_y)) = py.and_then(|py| self.resolve_group_drop(&items, py)) {
                let x = ui.max_rect().x_range();
                ui.painter().hline(x, line_y, Stroke::new(2.0, palette::group_accent()));
                if group_released {
                    acts.push(Act::MoveGroup { name, before });
                }
            }
            ui.ctx().request_repaint();
        }
        if released {
            self.dragging = None;
        }
        if group_released {
            self.dragging_group = None;
        }
        acts
    }

    fn explorer_panel(&mut self, ui: &mut egui::Ui) -> Vec<Act> {
        let dir = self.explorer.dir.clone();
        // Refresh the browse listing (single-dir read, throttled to 1.5s).
        let stale = self.explorer.list_dir.as_deref() != Some(dir.as_path())
            || self.explorer.list_at.is_none_or(|t| t.elapsed().as_millis() > 1500);
        if stale {
            self.explorer.list = read_dir_sorted(&dir);
            self.explorer.list_dir = Some(dir.clone());
            self.explorer.list_at = Some(Instant::now());
        }
        // Recompute recursive search hits only when the query or dir changes.
        let query = self.explorer.query.trim().to_string();
        if self.explorer.mode == ExMode::Search && !query.is_empty() {
            let key = (dir.clone(), query.clone());
            if self.explorer.hits_key.as_ref() != Some(&key) {
                let mut hits = Vec::new();
                ex_search(&dir, &query, &mut hits);
                self.explorer.hits = hits;
                self.explorer.hits_key = Some(key);
            }
        }

        let mut go_dir: Option<PathBuf> = None;
        let mut open_file: Option<PathBuf> = None;
        let mut create = false;

        ui.add_space(10.0);
        // Header: directory name on the left, magnifier + new-file on the right.
        ui.horizontal(|ui| {
            ui.add_space(10.0);
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned());
            ui.label(RichText::new(truncate_head(&name, 20)).size(12.5).strong().color(palette::text()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(8.0);
                let new_on = self.explorer.mode == ExMode::New;
                if ex_icon_button(ui, new_on, draw_newfile).on_hover_text(tr("Новый файл", "New file")).clicked() {
                    self.explorer.mode = if new_on { ExMode::Browse } else { ExMode::New };
                    self.explorer.new_name.clear();
                    self.explorer.focus = true;
                }
                ui.add_space(2.0);
                let search_on = self.explorer.mode == ExMode::Search;
                if ex_icon_button(ui, search_on, draw_search).on_hover_text(tr("Поиск", "Search")).clicked() {
                    self.explorer.mode = if search_on { ExMode::Browse } else { ExMode::Search };
                    self.explorer.query.clear();
                    self.explorer.focus = true;
                }
            });
        });
        ui.add_space(6.0);

        // Input row for the active mode.
        match self.explorer.mode {
            ExMode::Search => {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut self.explorer.query)
                            .desired_width(f32::INFINITY)
                            .hint_text(tr("Поиск файлов...", "Find files...")),
                    );
                    if self.explorer.focus {
                        te.request_focus();
                        self.explorer.focus = false;
                    }
                });
                ui.add_space(4.0);
            },
            ExMode::New => {
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut self.explorer.new_name)
                            .desired_width(f32::INFINITY)
                            .hint_text(tr("Имя файла, Enter", "File name, Enter")),
                    );
                    if self.explorer.focus {
                        te.request_focus();
                        self.explorer.focus = false;
                    }
                    if te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        create = true;
                    }
                });
                ui.add_space(4.0);
            },
            ExMode::Browse => {},
        }

        let searching = self.explorer.mode == ExMode::Search && !query.is_empty();
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if searching {
                if self.explorer.hits.is_empty() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(RichText::new(tr("Ничего не найдено", "No matches")).size(12.0).color(palette::text_faint()));
                    });
                }
                for hit in &self.explorer.hits {
                    let rel = hit.strip_prefix(&dir).unwrap_or(hit);
                    let is_dir = hit.is_dir();
                    if ex_row(ui, &rel.to_string_lossy(), is_dir).clicked() {
                        if is_dir {
                            go_dir = Some(hit.clone());
                        } else {
                            open_file = Some(hit.clone());
                        }
                    }
                }
            } else {
                if let Some(parent) = dir.parent() {
                    if ex_row(ui, "..", true).clicked() {
                        go_dir = Some(parent.to_path_buf());
                    }
                }
                for e in &self.explorer.list {
                    if ex_row(ui, &e.name, e.is_dir).clicked() {
                        let p = dir.join(&e.name);
                        if e.is_dir {
                            go_dir = Some(p);
                        } else {
                            open_file = Some(p);
                        }
                    }
                }
            }
            ui.add_space(20.0);
        });

        if create {
            let name = self.explorer.new_name.trim().to_string();
            // Only allow a new file inside the browsed tree: reject empty, absolute,
            // or `..`-escaping names. create_new never clobbers an existing file.
            let safe = !name.is_empty()
                && !std::path::Path::new(&name).components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                });
            let path = dir.join(&name);
            let created = safe
                && path.parent().is_none_or(|p| std::fs::create_dir_all(p).is_ok())
                && std::fs::OpenOptions::new().write(true).create_new(true).open(&path).is_ok();
            if created {
                self.explorer.mode = ExMode::Browse;
                self.explorer.new_name.clear();
                self.explorer.list_dir = None; // force a refresh so it shows up
            }
            // On failure (bad name / already exists / no permission) keep the New
            // field open so the no-op is visible rather than silently swallowed.
        }
        if let Some(d) = go_dir {
            self.explorer.dir = d;
            self.explorer.list_dir = None;
            self.explorer.mode = ExMode::Browse;
            self.explorer.query.clear();
        }
        if let Some(f) = open_file {
            self.insert_paths(shell_escape(&f.to_string_lossy()));
        }
        Vec::new()
    }

    /// Sidebar display order: ungrouped rows first, then each group (header +
    /// its members) in first-appearance order.
    fn sidebar_order(&self) -> Vec<Slot> {
        let mut out = Vec::new();
        for s in &self.sessions {
            if s.group.is_none() {
                out.push(Slot::Row(s.id));
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for s in &self.sessions {
            let Some(g) = s.group.as_deref() else { continue };
            if seen.contains(&g) {
                continue;
            }
            seen.push(g);
            out.push(Slot::Header(g.to_string()));
            for m in &self.sessions {
                if m.group.as_deref() == Some(g) {
                    out.push(Slot::Row(m.id));
                }
            }
        }
        out
    }

    /// Reorder `sessions` into sidebar display order: ungrouped first, then each
    /// group's members contiguously. Reorders only, never drops. `resolve_drop`
    /// and the drag insertion math read vec adjacency, so they only stay correct
    /// while the vec matches what is drawn; run this after any grouping/order
    /// change (and once on load) to keep that invariant.
    fn normalize_order(&mut self) {
        let order: Vec<u64> = self
            .sidebar_order()
            .into_iter()
            .filter_map(|s| if let Slot::Row(id) = s { Some(id) } else { None })
            .collect();
        self.sessions
            .sort_by_key(|s| order.iter().position(|&id| id == s.id).unwrap_or(usize::MAX));
    }

    /// A group header: colored band, collapse triangle, name, member count.
    /// Returns its rect (a drop target) and whether a group drag ended on it.
    /// Click toggles collapse, double-click renames, drag reorders groups.
    fn group_header(&mut self, ui: &mut egui::Ui, name: &str, collapsed: bool, acts: &mut Vec<Act>) -> (Rect, bool) {
        let editing = self.renaming_group.as_deref() == Some(name);
        let is_dragging = self.dragging_group.as_deref() == Some(name);
        let count = self.sessions.iter().filter(|s| s.group.as_deref() == Some(name)).count();
        // A touch of top space so each group reads as a new section.
        ui.add_space(4.0);
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::click_and_drag());
        if !ui.is_rect_visible(rect) {
            return (rect, false);
        }
        let drag_started = !editing && resp.drag_started();
        let released = is_dragging && resp.drag_stopped();
        let hovered = ui.rect_contains_pointer(rect);
        let painter = ui.painter();

        // Tinted band + a colored left edge so the header stands out and does
        // not merge into the sidebar on either theme.
        let band = Rect::from_min_max(
            Pos2::new(rect.min.x + 4.0, rect.min.y + 1.0),
            Pos2::new(rect.max.x - 6.0, rect.max.y - 1.0),
        );
        painter.rect_filled(band, CornerRadius::same(5), palette::group_header_bg());
        painter.rect_filled(
            Rect::from_min_size(band.min + Vec2::new(0.0, 2.0), Vec2::new(2.5, band.height() - 4.0)),
            CornerRadius::same(2),
            palette::group_accent(),
        );

        let tri_c = Pos2::new(rect.min.x + 16.0, rect.center().y);
        draw_caret(painter, tri_c, collapsed, palette::group_accent());
        if !editing {
            painter.text(
                Pos2::new(rect.min.x + 27.0, rect.center().y),
                Align2::LEFT_CENTER,
                truncate_end(name, 20),
                FontId::proportional(11.5),
                if hovered { palette::text_strong() } else { palette::text() },
            );
        }
        painter.text(
            Pos2::new(rect.max.x - 14.0, rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{count}"),
            FontId::proportional(10.0),
            palette::text_faint(),
        );

        if resp.clicked() {
            acts.push(Act::ToggleGroup(name.to_string()));
        }
        if resp.double_clicked() {
            acts.push(Act::BeginRenameGroup(name.to_string()));
        }
        resp.context_menu(|ui| {
            if ui.button(tr("Переименовать группу", "Rename group")).clicked() {
                acts.push(Act::BeginRenameGroup(name.to_string()));
                ui.close();
            }
            if ui.button(tr("Расформировать группу", "Ungroup")).clicked() {
                acts.push(Act::DeleteGroup(name.to_string()));
                ui.close();
            }
        });

        if editing {
            let cctx = ui.ctx().clone();
            let width = rect.width() - 30.0;
            let field = egui::Area::new(egui::Id::new(("group-rename", name)))
                .order(egui::Order::Foreground)
                .fixed_pos(Pos2::new(rect.min.x + 22.0, rect.center().y - 13.0))
                .show(&cctx, |ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.rename_buf)
                            .font(FontId::proportional(11.5))
                            .desired_width(width),
                    );
                    if self.rename_focus {
                        r.request_focus();
                        select_all_text(ui.ctx(), r.id, self.rename_buf.chars().count());
                        self.rename_focus = false;
                    }
                    r
                })
                .inner;
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                acts.push(Act::RenameCancel);
            } else if field.lost_focus() {
                acts.push(Act::RenameGroupCommit(name.to_string()));
            }
        }
        if drag_started {
            self.dragging_group = Some(name.to_string());
        }
        (rect, released)
    }

    /// Draws one session row indented by `indent`. Returns the row rect (a drop
    /// target) and whether a drag ended on it.
    fn session_row(&mut self, ui: &mut egui::Ui, idx: usize, indent: f32, acts: &mut Vec<Act>) -> (Rect, bool) {
        let id = self.sessions[idx].id;
        let renaming = self.renaming == Some(id);
        let is_dragging = self.dragging == Some(id);
        let pinned = self.sessions[idx].pinned;
        // Seconds since this row's pin refused something (close, drag, move).
        let shake = self
            .pin_shake
            .filter(|(sid, _)| *sid == id)
            .map(|(_, t)| t.elapsed().as_secs_f32())
            .filter(|t| *t < SHAKE_SECS);
        let s = &self.sessions[idx];
        let selected = self.active == Some(s.id);
        let row_h = 48.0;
        let (rect, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click_and_drag());
        if !ui.is_rect_visible(rect) {
            return (rect, false);
        }
        // Left origin for indented content (dot, name, path).
        let x0 = rect.min.x + indent;
        let drag_started = !renaming && resp.drag_started();
        let released = is_dragging && resp.drag_stopped();
        // Geometric hover: overlapping child widgets (close button, ctx corner)
        // must not make the row highlight and the button flicker.
        let hovered = ui.rect_contains_pointer(rect);
        let painter = ui.painter();

        if selected {
            painter.rect_filled(rect, 0.0, palette::row_sel_bg());
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x0, rect.min.y), Vec2::new(2.0, row_h)),
                0.0,
                palette::accent_bar(),
            );
        } else if hovered {
            painter.rect_filled(rect, 0.0, palette::row_hover_bg());
        }
        if is_dragging {
            painter.rect_stroke(
                rect.shrink(2.0),
                CornerRadius::same(4),
                Stroke::new(1.0, palette::accent_bar()),
                egui::StrokeKind::Inside,
            );
        }

        // Status: green = working (recent output), orange = waiting for input,
        // red = exited with error. A star instead of a dot means claude is running.
        let dot = Pos2::new(x0 + 16.0, rect.center().y);
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
                painter.circle_stroke(dot, 3.0, Stroke::new(1.2, palette::text_dim()));
            },
            Phase::Exited(code) => {
                let color = if code.is_some_and(|c| c != 0) { DOT_EXITED } else { Color32::from_gray(0x6a) };
                painter.circle_filled(dot, 3.0, color);
            },
        }

        // Pin, right above the status dot. A pinned session cannot be dragged,
        // closed by a stray click, or put to sleep by the idle timer. Unpinned
        // rows only show it under the pointer, so the list stays quiet.
        let pin_c = Pos2::new(x0 + 16.0, rect.min.y + 11.0);
        let pin_resp =
            ui.interact(Rect::from_center_size(pin_c, Vec2::splat(18.0)), ui.id().with(("pin", id)), Sense::click());
        if pin_resp.clicked() {
            acts.push(Act::TogglePin(id));
        }
        if pinned || hovered || pin_resp.hovered() {
            // Refusals wobble the pin instead of popping a dialog: a decaying
            // sway that dies out within SHAKE_SECS.
            let dx = match shake {
                Some(t) => {
                    ui.ctx().request_repaint();
                    (t * 55.0).sin() * 4.0 * (1.0 - t / SHAKE_SECS)
                },
                None => 0.0,
            };
            let col = if pinned {
                palette::accent_bar()
            } else if pin_resp.hovered() {
                palette::text()
            } else {
                palette::text_faint()
            };
            paint_pin(ui.painter(), pin_c + Vec2::new(dx, 0.0), pinned, col);
        }
        pin_resp.on_hover_text(if pinned {
            tr("Открепить сессию", "Unpin session")
        } else {
            tr("Закрепить: не закроется, не сдвинется, не уснёт", "Pin: no close, no drag, no auto-suspend")
        });

        // Context badge: pill with the session's context %, top-right.
        // No index entry = no badge (never a fake 0%).
        let ctx_entry = s.claude_session_id.as_deref().and_then(|sid| self.ctx_index.get(sid));
        // Text budgets follow the panel width - widening it is what shows more
        // of a long name. The divisors are the average glyph advance at each
        // size, calibrated so the default 236px panel keeps its old cutoffs.
        let text_w = rect.width() - indent - 28.0;
        let mut name_max = ((text_w - 34.0) / 6.6).max(4.0) as usize;
        if let Some(e) = ctx_entry {
            let pct = e.pct.clamp(1.0, 100.0);
            let lt = palette::light();
            let (bg, fg) = if pct >= 70.0 {
                // Pulse at ~10fps while visible.
                let t = ui.input(|i| i.time);
                let a = ((t * 4.0).sin() * 0.5 + 0.5) as f32;
                let lerp = |lo: u8, hi: u8| (lo as f32 + (hi as f32 - lo as f32) * a) as u8;
                if lt {
                    (
                        Color32::from_rgb(0xf4, lerp(0xce, 0xbc), lerp(0xce, 0xbc)),
                        Color32::from_rgb(0xb0, 0x2b, 0x2b),
                    )
                } else {
                    (
                        Color32::from_rgb(lerp(0x38, 0x5c), lerp(0x1e, 0x22), lerp(0x1e, 0x22)),
                        Color32::from_rgb(0xe2, 0x8f, 0x8f),
                    )
                }
            } else if pct >= 50.0 {
                if lt {
                    (Color32::from_rgb(0xf5, 0xec, 0xc0), Color32::from_rgb(0x8a, 0x6d, 0x0a))
                } else {
                    (Color32::from_rgb(0x33, 0x2f, 0x1a), Color32::from_rgb(0xd4, 0xc4, 0x5a))
                }
            } else if lt {
                (Color32::from_rgb(0xdc, 0xef, 0xcf), Color32::from_rgb(0x3a, 0x7d, 0x2c))
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
            // The badge eats into the name's line.
            name_max = name_max.saturating_sub(5);
        }

        // How long since Claude last answered here. It lives in the bottom-right
        // corner, under the % badge, so a long name keeps its whole line - and
        // it comes from the transcript, so a tab restored from yesterday says
        // yesterday instead of restarting the clock.
        let ago = self
            .settings
            .show_last_msg
            .then(|| s.claude_session_id.as_deref())
            .flatten()
            .and_then(|sid| self.ctx_index.last_msg(sid))
            .map(|t| {
                painter.layout_no_wrap(fmt_ago(t), FontId::proportional(9.0), palette::text_faint())
            });

        let text_x = x0 + 28.0;
        // The corner label takes its width off the path, never off the name.
        let path_w = text_w - 10.0 - ago.as_ref().map_or(0.0, |g| g.size().x + 8.0);
        let path_len = (path_w / 5.6).max(6.0) as usize;
        let name_color = if selected { palette::text_strong() } else { palette::text() };
        // The name is replaced by an inline editor while renaming (drawn below).
        if !renaming {
            painter.text(
                Pos2::new(text_x, rect.center().y - 9.0),
                Align2::LEFT_CENTER,
                truncate_end(&s.display_name(), name_max),
                FontId::proportional(13.0),
                name_color,
            );
        }
        painter.text(
            Pos2::new(text_x, rect.center().y + 8.0),
            Align2::LEFT_CENTER,
            truncate_head(&tilde(&s.cwd), path_len),
            FontId::proportional(10.5),
            palette::text_faint(),
        );
        if let Some(g) = ago {
            let size = g.size();
            painter.galley(
                Pos2::new(rect.max.x - 8.0 - size.x, rect.max.y - 8.0 - size.y / 2.0),
                g,
                palette::text_faint(),
            );
            // Minute granularity: on an idle app nothing else asks for a frame,
            // and the label would sit frozen at whatever it said last.
            ui.ctx().request_repaint_after(Duration::from_secs(20));
        }

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
                (palette::ui_bg_hover(), palette::text())
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
            ui.painter().text(mark, Align2::CENTER_CENTER, "!", FontId::proportional(11.0), palette::text_faint());
        }

        if resp.clicked() {
            acts.push(Act::Select(s.id));
        }
        if resp.double_clicked() {
            acts.push(Act::BeginRename(s.id));
        }
        let sid = s.id;
        let cur_group = s.group.clone();
        let mut all_groups: Vec<String> = Vec::new();
        for x in &self.sessions {
            if let Some(g) = &x.group {
                if !all_groups.contains(g) {
                    all_groups.push(g.clone());
                }
            }
        }
        resp.context_menu(|ui| {
            if ui.button(tr("Переименовать", "Rename")).clicked() {
                acts.push(Act::BeginRename(sid));
                ui.close();
            }
            let pin_label = if pinned {
                tr("Открепить", "Unpin")
            } else {
                tr("Закрепить (защита)", "Pin (protect)")
            };
            if ui.button(pin_label).clicked() {
                acts.push(Act::TogglePin(sid));
                ui.close();
            }
            ui.menu_button(tr("В группу", "Move to group"), |ui| {
                for g in &all_groups {
                    if cur_group.as_deref() != Some(g.as_str())
                        && ui.button(truncate_end(g, 24)).clicked()
                    {
                        acts.push(Act::MoveSession { id: sid, group: Some(g.clone()), before: None });
                        ui.close();
                    }
                }
                if !all_groups.is_empty() {
                    ui.separator();
                }
                if ui.button(tr("Новая группа…", "New group…")).clicked() {
                    acts.push(Act::NewGroup(sid));
                    ui.close();
                }
                if cur_group.is_some() && ui.button(tr("Без группы", "No group")).clicked() {
                    acts.push(Act::MoveSession { id: sid, group: None, before: None });
                    ui.close();
                }
            });
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
                    if ui.button(tr("Продолжить в терминале", "Continue in terminal")).clicked() {
                        acts.push(Act::Resume(s.id, false));
                        ui.close();
                    }
                    if ui.button(tr("Удалить", "Delete")).clicked() {
                        acts.push(Act::Remove(s.id));
                        ui.close();
                    }
                },
            }
        });

        // Inline title editor: a text field laid over the name, committed on
        // Enter or blur, cancelled on Escape. `s` is no longer borrowed here.
        if renaming {
            let cctx = ui.ctx().clone();
            let width = rect.width() - 36.0;
            let field = egui::Area::new(egui::Id::new(("rename", id)))
                .order(egui::Order::Foreground)
                .fixed_pos(Pos2::new(text_x - 4.0, rect.center().y - 18.0))
                .show(&cctx, |ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.rename_buf)
                            .font(FontId::proportional(13.0))
                            .desired_width(width),
                    );
                    if self.rename_focus {
                        r.request_focus();
                        select_all_text(ui.ctx(), r.id, self.rename_buf.chars().count());
                        self.rename_focus = false;
                    }
                    r
                })
                .inner;
            let escaped = ui.input(|i| i.key_pressed(Key::Escape));
            if escaped {
                acts.push(Act::RenameCancel);
            } else if field.lost_focus() {
                acts.push(Act::RenameCommit(id));
            }
        }
        if drag_started {
            if pinned {
                self.pin_shake = Some((id, Instant::now()));
            } else {
                self.dragging = Some(id);
            }
        }
        (rect, released)
    }

    fn bottom_bar(&mut self, ui: &mut egui::Ui) -> Vec<Act> {
        let mut acts = Vec::new();
        let Some(idx) = self.active_idx() else {
            ui.horizontal(|ui| {
                ui.add_space(10.0);
                ui.label(RichText::new(tr("Нет активной сессии", "No active session")).size(11.5).color(palette::text_faint()));
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
                            RichText::new(truncate_head(&path_full, 44)).monospace().size(11.5).color(palette::text()),
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
                        ui.label(RichText::new(&g.branch).size(11.5).color(palette::text_dim()));
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
                            ui.label(RichText::new(tr("чисто", "clean")).size(11.0).color(palette::text_faint()));
                        }
                    },
                    Some(_) => {
                        ui.add_space(6.0);
                        ui.label(RichText::new(tr("не git", "not git")).size(11.0).color(palette::text_faint()));
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
                                if s.fg_is_claude {
                                    ui.label(
                                        RichText::new(short_id(cid)).size(10.5).monospace().color(palette::text_faint()),
                                    )
                                    .on_hover_text(format!("{}: {cid}", tr("Сохранённая сессия Claude", "Saved Claude session")));
                                } else {
                                    // Type the resume into the shell that is already
                                    // running instead of respawning the terminal: a
                                    // respawn drops LiveTerm, which SIGKILLs the whole
                                    // process tree, and this button sits one stray
                                    // click away from a build the user is watching.
                                    // Disabled outright while anything else holds the
                                    // foreground, since the shell would not read it.
                                    let btn = ui
                                        .add_enabled(
                                            !busy_now,
                                            egui::Button::new(
                                                RichText::new(format!("{} {}", tr("Вернуться в сессию", "Return to session"), short_id(cid)))
                                                    .size(11.0)
                                                    .color(Color32::from_rgb(0xd8, 0xe4, 0xd0)),
                                            ),
                                        )
                                        .on_hover_text(format!("claude --resume {cid}"))
                                        .on_disabled_hover_text(tr("Терминал занят", "Terminal busy"));
                                    if btn.clicked() {
                                        acts.push(Act::ResumeInPlace(s.id));
                                    }
                                }
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
                                ui.label(RichText::new(txt).size(11.0).color(palette::text_faint()));
                            } else {
                                ui.label(RichText::new(tr("усыплена", "suspended")).size(11.0).color(palette::text_faint()));
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
            let accept = busy_now
                && !self.settings_open
                && !self.dir_open
                && self.renaming.is_none()
                && self.renaming_group.is_none()
                // Don't steal focus from the explorer's active text field.
                && !(self.explorer.open && self.explorer.mode != ExMode::Browse);
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
                let head_fill = if palette::light() {
                    Color32::from_rgba_unmultiplied(0xf3, 0xf3, 0xf3, 236)
                } else {
                    Color32::from_rgba_unmultiplied(0x15, 0x15, 0x15, 236)
                };
                p.rect_filled(head, 0.0, head_fill);
                p.hline(head.x_range(), head.bottom(), Stroke::new(1.0, palette::border_dim()));
                p.text(
                    head.min + Vec2::new(10.0, 5.0),
                    Align2::LEFT_TOP,
                    format!("{} ({})", tilde(&s.cwd), fmt_dur(dur)),
                    FontId::monospace(10.5),
                    palette::text_dim(),
                );
                p.text(
                    head.min + Vec2::new(10.0, 20.0),
                    Align2::LEFT_TOP,
                    truncate_end(&cmd_text, 110),
                    FontId::monospace(12.0),
                    palette::text(),
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
        let ctx = ui.ctx().clone();
        self.refresh_history(&ctx);
        let interactive = !self.settings_open
            && !self.dir_open
            && self.renaming.is_none()
            && self.renaming_group.is_none()
            // The explorer's search / new-file field owns the keyboard while open.
            && !(self.explorer.open && self.explorer.mode != ExMode::Browse);
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
                // A background history reload can shrink the filtered list under
                // a stale hist_sel, so clamp before stepping up; indexing it raw
                // panics. (The Down branch already guards with i + 1 < show_n.)
                let sel = match self.hist_sel {
                    None => show_n - 1,
                    Some(i) => i.min(show_n - 1).saturating_sub(1),
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
                    Stroke::new(1.0, palette::border_dim()),
                );
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add_space(10.0);
                    ui.label(RichText::new(">").font(font.clone()).color(palette::text_dim()));
                    // Multiline, but egui only inserts a newline on its return_key -
                    // point that at Shift+Enter. Plain Enter is consumed above and
                    // submits, so the editor never sees it.
                    let resp = ui.add(
                        egui::TextEdit::multiline(&mut self.cmd_input)
                            .frame(Frame::new())
                            .font(font.clone())
                            .desired_rows(1)
                            .return_key(egui::KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                            .hint_text(RichText::new(tr("команда...", "command...")).font(font).color(palette::text_faint()))
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
                        .fill(palette::popup_bg())
                        .stroke(Stroke::new(1.0, palette::border()))
                        .corner_radius(CornerRadius::same(8))
                        .inner_margin(Margin::symmetric(10, 8))
                        .show(ui, |ui| {
                            ui.set_width(field_rect.width().min(760.0));
                            ui.label(RichText::new(tr("История", "History")).size(9.5).color(palette::text_faint()));
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
                                    .color(palette::text_faint()),
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
                let bg = if active { palette::surface_hi() } else { palette::surface() };
                ui.painter().rect_filled(rect, CornerRadius::same(5), bg);
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::same(5),
                    Stroke::new(1.0, palette::border()),
                    egui::StrokeKind::Inside,
                );
                let fg = if active { palette::text() } else { palette::text_dim() };
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
            // kip is sampled own-process-only (tree = false); each live session
            // sums its whole shell tree (tree = true). Measuring kip alone keeps
            // its row equal to the kip process itself, never folding in whatever
            // happens to sit under it in the process tree.
            let mut targets: Vec<(String, i32, bool)> =
                vec![("kip".into(), std::process::id() as i32, false)];
            for s in &self.sessions {
                if let Some(live) = s.live() {
                    targets.push((s.display_name(), live.shell_pid, true));
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
                    .fill(palette::popup_bg())
                    .stroke(Stroke::new(1.0, palette::border()))
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
                        ui.label(RichText::new(tr("Ресурсы", "Resources")).size(10.0).strong().color(palette::text_dim()));
                        ui.add_space(6.0);
                        let Some(st) = &self.stats else {
                            ui.label(RichText::new(tr("измеряю...", "measuring...")).size(11.0).color(palette::text_faint()));
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
                                if i == 0 { palette::text_dim() } else { palette::text() },
                            );
                            p.text(
                                rect.right_top() + Vec2::new(0.0, 1.0),
                                Align2::RIGHT_TOP,
                                fmt_mem(*rss),
                                FontId::monospace(11.5),
                                palette::text(),
                            );
                            // Line 2: memory bar + cpu.
                            let bar_w = w - 52.0;
                            let by = rect.top() + 21.0;
                            let track = Rect::from_min_size(
                                Pos2::new(rect.left(), by),
                                Vec2::new(bar_w, 3.5),
                            );
                            p.rect_filled(track, CornerRadius::same(2), palette::border_dim());
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
                                if *cpu >= 50.0 { ORANGE } else { palette::text_faint() },
                            );
                            if i == 0 && st.procs.len() > 1 {
                                ui.add_space(3.0);
                                let sep = ui.available_rect_before_wrap();
                                ui.painter().hline(
                                    sep.left()..=sep.left() + w,
                                    sep.top(),
                                    Stroke::new(1.0, palette::border_dim()),
                                );
                                ui.add_space(5.0);
                            }
                        }
                        ui.add_space(4.0);
                        let sep = ui.available_rect_before_wrap();
                        ui.painter().hline(
                            sep.left()..=sep.left() + w,
                            sep.top(),
                            Stroke::new(1.0, palette::border_dim()),
                        );
                        ui.add_space(5.0);
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 14.0), Sense::hover());
                        let p = ui.painter();
                        p.text(
                            rect.left_top(),
                            Align2::LEFT_TOP,
                            tr("всего", "total"),
                            FontId::proportional(10.5),
                            palette::text_faint(),
                        );
                        p.text(
                            rect.right_top(),
                            Align2::RIGHT_TOP,
                            fmt_mem(total),
                            FontId::monospace(11.5),
                            palette::text(),
                        );
                    });
            });
        self.stats_rect = Some(area.response.rect);
    }

    /// Subscription-limit chip in the bottom-right corner. Hovering opens every
    /// window the account has (5h session, week, per-model week); clicking a row
    /// pins its bar into the chip, so it stays readable with the mouse away.
    fn usage_ui(&mut self, ctx: &egui::Context) {
        let pinned = self
            .settings
            .usage_pin
            .as_ref()
            .and_then(|k| self.usage.iter().find(|l| &l.key == k))
            .cloned();

        let chip = egui::Area::new(egui::Id::new("usage-chip"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-10.0, -42.0))
            .show(ctx, |ui| {
                const BAR_W: f32 = 34.0;
                // The pinned label is a model name on some plans ("Fable"), so the
                // pill is measured, not a fixed width.
                let label = pinned.as_ref().map(|l| {
                    ui.painter().layout_no_wrap(
                        l.short.clone(),
                        FontId::proportional(10.0),
                        palette::text_dim(),
                    )
                });
                let w = match &label {
                    Some(g) => 8.0 + g.size().x + 6.0 + BAR_W + 6.0 + 24.0 + 8.0,
                    None => 30.0,
                };
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::hover());
                let active = resp.hovered() || self.usage_rect.is_some();
                let bg = if active { palette::surface_hi() } else { palette::surface() };
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::same(5), bg);
                p.rect_stroke(
                    rect,
                    CornerRadius::same(5),
                    Stroke::new(1.0, palette::border()),
                    egui::StrokeKind::Inside,
                );
                match (label, &pinned) {
                    (Some(g), Some(l)) => {
                        let gw = g.size().x;
                        p.galley(
                            Pos2::new(rect.left() + 8.0, rect.center().y - g.size().y / 2.0),
                            g,
                            palette::text_dim(),
                        );
                        let track = Rect::from_min_size(
                            Pos2::new(rect.left() + 14.0 + gw, rect.center().y - 2.0),
                            Vec2::new(BAR_W, 4.0),
                        );
                        p.rect_filled(track, CornerRadius::same(2), palette::border_dim());
                        p.rect_filled(
                            Rect::from_min_size(track.min, Vec2::new(bar_fill(BAR_W, l.percent), 4.0)),
                            CornerRadius::same(2),
                            usage_color(l.percent),
                        );
                        p.text(
                            Pos2::new(rect.right() - 8.0, rect.center().y),
                            Align2::RIGHT_CENTER,
                            format!("{:.0}%", l.percent),
                            FontId::monospace(10.0),
                            usage_color(l.percent),
                        );
                    },
                    _ => {
                        // Stacked horizontal bars, so it reads apart from the
                        // vertical cpu glyph in the opposite corner.
                        let fg = if active { palette::text() } else { palette::text_dim() };
                        let left = rect.center().x - 6.0;
                        for (i, len) in [9.0, 5.0, 11.0].iter().enumerate() {
                            let y = rect.center().y - 4.0 + i as f32 * 4.0;
                            p.line_segment(
                                [Pos2::new(left, y), Pos2::new(left + len, y)],
                                Stroke::new(2.0, fg),
                            );
                        }
                    },
                }
                resp
            });

        let pointer = ctx.input(|i| i.pointer.interact_pos());
        let over_popup = self
            .usage_rect
            .is_some_and(|r| pointer.is_some_and(|p| r.expand(6.0).contains(p)));
        let open = chip.inner.hovered() || over_popup;

        // A pinned bar has to stay current with the popup closed; an unpinned one
        // only matters while the panel is open. The pin is read from settings, not
        // from the resolved row: right after a start, and whenever the API drops a
        // window from the array (a session limit that just reset comes back as
        // inactive), there is nothing to resolve - keying the refresh off that
        // would freeze the chip empty until the mouse happens to pass over it.
        if open || self.settings.usage_pin.is_some() || self.usage_at.is_none() {
            let stale = self.usage_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
            if stale && !self.usage_inflight {
                self.usage_inflight = true;
                usage::fetch(self.usage_tx.clone(), ctx.clone());
            }
            ctx.request_repaint_after(Duration::from_secs(30));
        }
        if !open {
            self.usage_rect = None;
            return;
        }

        let mut toggle = None;
        let area = egui::Area::new(egui::Id::new("usage-popup"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, Vec2::new(-10.0, -66.0))
            .show(ctx, |ui| {
                Frame::new()
                    .fill(palette::popup_bg())
                    .stroke(Stroke::new(1.0, palette::border()))
                    .corner_radius(CornerRadius::same(10))
                    .inner_margin(Margin::symmetric(14, 12))
                    .shadow(egui::Shadow {
                        offset: [0, 4],
                        blur: 18,
                        spread: 0,
                        color: Color32::from_black_alpha(120),
                    })
                    .show(ui, |ui| {
                        let w = 220.0;
                        ui.set_min_width(w);
                        ui.label(
                            RichText::new(tr("Лимиты Claude", "Claude limits")).size(10.0).strong().color(palette::text_dim()),
                        );
                        ui.add_space(6.0);
                        if self.usage.is_empty() {
                            let txt = self
                                .usage_err
                                .clone()
                                .unwrap_or_else(|| tr("загружаю...", "loading...").into());
                            ui.label(RichText::new(txt).size(11.0).color(palette::text_faint()));
                            return;
                        }
                        for l in &self.usage {
                            let is_pinned = self.settings.usage_pin.as_deref() == Some(&l.key);
                            let (rect, resp) =
                                ui.allocate_exact_size(Vec2::new(w, 36.0), Sense::click());
                            // The pin button owns the right edge; the bar, the
                            // percent and the countdown stop short of it.
                            let cw = w - 22.0;
                            let pin_rect = Rect::from_center_size(
                                Pos2::new(rect.right() - 8.0, rect.top() + 13.0),
                                Vec2::splat(18.0),
                            );
                            let pin = ui
                                .interact(
                                    pin_rect,
                                    egui::Id::new(("usage-pin", &l.key)),
                                    Sense::click(),
                                )
                                .on_hover_text(if is_pinned {
                                    tr("Открепить", "Unpin")
                                } else {
                                    tr("Закрепить в углу", "Pin to the corner")
                                });
                            let p = ui.painter();
                            if resp.hovered() {
                                p.rect_filled(
                                    rect.expand2(Vec2::new(6.0, 2.0)),
                                    CornerRadius::same(5),
                                    palette::row_hover_bg(),
                                );
                            }
                            if is_pinned {
                                p.rect_filled(
                                    Rect::from_min_size(
                                        Pos2::new(rect.left() - 6.0, rect.top()),
                                        Vec2::new(2.0, 16.0),
                                    ),
                                    CornerRadius::same(1),
                                    palette::accent_bar(),
                                );
                            }
                            p.text(
                                rect.left_top(),
                                Align2::LEFT_TOP,
                                &l.label,
                                FontId::proportional(12.0),
                                if is_pinned { palette::text_strong() } else { palette::text() },
                            );
                            p.text(
                                Pos2::new(rect.left() + cw, rect.top()),
                                Align2::RIGHT_TOP,
                                format!("{:.0}%", l.percent),
                                FontId::monospace(11.5),
                                usage_color(l.percent),
                            );
                            let track = Rect::from_min_size(
                                Pos2::new(rect.left(), rect.top() + 19.0),
                                Vec2::new(cw, 4.0),
                            );
                            p.rect_filled(track, CornerRadius::same(2), palette::border_dim());
                            p.rect_filled(
                                Rect::from_min_size(track.min, Vec2::new(bar_fill(cw, l.percent), 4.0)),
                                CornerRadius::same(2),
                                usage_color(l.percent),
                            );
                            if let Some(ts) = l.resets_at {
                                p.text(
                                    Pos2::new(rect.left() + cw, rect.top() + 26.0),
                                    Align2::RIGHT_TOP,
                                    fmt_until(ts),
                                    FontId::proportional(9.5),
                                    palette::text_faint(),
                                );
                            }
                            if pin.hovered() {
                                p.rect_filled(
                                    pin_rect,
                                    CornerRadius::same(4),
                                    palette::surface_hi(),
                                );
                            }
                            paint_pin(
                                p,
                                pin_rect.center(),
                                is_pinned,
                                if is_pinned {
                                    palette::accent_bar()
                                } else if pin.hovered() {
                                    palette::text()
                                } else {
                                    palette::text_faint()
                                },
                            );
                            if resp.clicked() || pin.clicked() {
                                toggle = Some(l.key.clone());
                            }
                        }
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(tr("закрепи - полоска останется в углу", "pin one - its bar stays in the corner"))
                                .size(9.0)
                                .color(palette::text_faint()),
                        );
                    });
            });

        if let Some(key) = toggle {
            self.settings.usage_pin =
                if self.settings.usage_pin.as_deref() == Some(&key) { None } else { Some(key) };
            self.persist();
        }
        self.usage_rect = Some(area.response.rect);
    }

    /// Directory switcher over the path chip: navigating runs `cd` in the shell.
    fn dir_popup(&mut self, ctx: &egui::Context) {
        if !self.dir_open {
            // Catches every way the popup can close - the path chip, switching
            // session, Escape - so a half-typed folder name never comes back.
            self.new_dir = None;
            self.new_dir_err = None;
            return;
        }
        let Some(chip) = self.chip_rect else { return };

        let (mut esc, mut enter) = (false, false);
        ctx.input_mut(|i| {
            esc = consume_plain(i, Key::Escape);
            enter = consume_plain(i, Key::Enter);
        });
        if esc {
            // Escape backs out of the folder editor first, the popup second.
            if self.new_dir.take().is_none() {
                self.dir_open = false;
            }
            self.new_dir_err = None;
            return;
        }
        if enter && self.new_dir.is_some() {
            self.create_dir_and_enter(ctx);
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
                    .fill(palette::popup_bg())
                    .stroke(Stroke::new(1.0, palette::border()))
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
                        // The search field owns the keyboard except while a new
                        // folder name is being typed.
                        if self.new_dir.is_none() {
                            resp.request_focus();
                        }
                        ui.separator();

                        match self.new_dir.clone() {
                            None => {
                                let label = RichText::new(tr("+ Новая папка", "+ New folder"))
                                    .size(11.5)
                                    .color(palette::text_dim());
                                if ui.add(egui::Button::new(label).frame(false)).clicked() {
                                    self.new_dir =
                                        Some(tr("Новая папка", "New folder").to_string());
                                    self.new_dir_focus = true;
                                    self.new_dir_err = None;
                                }
                            },
                            Some(mut name) => {
                                let out = egui::TextEdit::singleline(&mut name)
                                    .font(FontId::proportional(12.5))
                                    .desired_width(f32::INFINITY)
                                    .show(ui);
                                if self.new_dir_focus {
                                    out.response.request_focus();
                                    let end = CCursor::new(name.chars().count());
                                    let mut state = out.state;
                                    state.cursor.set_char_range(Some(CCursorRange::two(
                                        CCursor::new(0),
                                        end,
                                    )));
                                    state.store(ui.ctx(), out.response.id);
                                    self.new_dir_focus = false;
                                }
                                self.new_dir = Some(name);
                                if let Some(err) = &self.new_dir_err {
                                    ui.label(
                                        RichText::new(truncate_end(err, 60)).size(10.5).color(GIT_DEL),
                                    );
                                }
                            },
                        }
                        ui.separator();
                        ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                            if self.dir_path.parent().is_some()
                                && ui
                                    .selectable_label(false, RichText::new(tr("..  (наверх)", "..  (up)")).size(12.0).color(palette::text_dim()))
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
                                ui.label(RichText::new(tr("нет подпапок", "no subfolders")).size(11.0).color(palette::text_faint()));
                            }
                        });
                        ui.label(
                            RichText::new(truncate_head(&tilde(&self.dir_path), 52))
                                .monospace()
                                .size(9.5)
                                .color(palette::text_faint()),
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

    /// Create the folder named in the inline editor and step into it. A failure
    /// (name taken, read-only parent) keeps the editor open with the reason.
    fn create_dir_and_enter(&mut self, ctx: &egui::Context) {
        let name = self.new_dir.as_deref().unwrap_or_default().trim().to_string();
        if name.is_empty() {
            return; // Enter on an empty field does nothing, the editor stays
        }
        self.new_dir = None;
        match std::fs::create_dir(self.dir_path.join(&name)) {
            Ok(()) => {
                self.new_dir_err = None;
                self.dir_cache_at = None;
                self.dir_navigate(Some(name), ctx);
            },
            Err(e) => {
                self.new_dir_err = Some(e.to_string());
                self.new_dir = Some(name);
                self.new_dir_focus = true;
            },
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
        self.new_dir = None;
        self.new_dir_err = None;
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
                                .color(palette::text_dim()),
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
                    .fill(palette::popup_bg())
                    .stroke(Stroke::new(1.0, palette::border()))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.set_min_width(300.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(title).size(14.0).strong().color(palette::text()));
                            if !hint.is_empty() {
                                ui.label(RichText::new(hint).size(11.0).color(palette::text_dim()));
                            }
                            ui.label(RichText::new(tilde(&s.cwd)).size(11.0).color(palette::text_dim()));
                            if let Some(cid) = &s.claude_session_id {
                                ui.label(
                                    RichText::new(format!("claude: {}", short_id(cid)))
                                        .size(10.5)
                                        .monospace()
                                        .color(palette::text_faint()),
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
                                if ui.button(RichText::new(tr("Продолжить в терминале", "Continue in terminal")).size(12.5)).clicked() {
                                    acts.push(Act::Resume(s.id, false));
                                }
                                if ui.button(RichText::new(tr("Удалить", "Delete")).size(12.5).color(palette::text_dim())).clicked() {
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
            ui.label(RichText::new("kip").size(22.0).color(palette::text_dim()));
            ui.label(RichText::new(tr("Нет открытых сессий", "No open sessions")).size(12.5).color(palette::text_faint()));
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

                    ui.label(tr("Шрифт", "Font"));
                    let choices = available_fonts();
                    let cur_font = choices
                        .iter()
                        .find(|(k, _)| *k == self.settings.font)
                        .map(|(_, l)| *l)
                        .unwrap_or("JetBrains Mono");
                    egui::ComboBox::from_id_salt("term-font")
                        .selected_text(cur_font)
                        .width(160.0)
                        .show_ui(ui, |ui| {
                            for (k, label) in choices {
                                if ui.selectable_label(self.settings.font == k, label).clicked()
                                    && self.settings.font != k
                                {
                                    self.settings.font = k.to_string();
                                    self.font_applied = install_fonts(ctx, k);
                                }
                            }
                        });
                    ui.end_row();

                    // What is really on screen: the pick can fall back silently,
                    // and the cell size in physical pixels shows whether the grid
                    // came out pixel-aligned.
                    let ppp = ctx.pixels_per_point();
                    let fid = FontId::monospace(self.settings.font_size);
                    let (cw, ch) =
                        ctx.fonts_mut(|f| (f.glyph_width(&fid, '0'), f.row_height(&fid)));
                    ui.label("");
                    ui.label(
                        RichText::new(format!(
                            "{}: {} · {:.0}×{:.0} px · ppp {ppp:.2}",
                            tr("рисуется", "rendering"),
                            self.font_applied,
                            (cw * ppp).round(),
                            (ch * ppp).round(),
                        ))
                        .font(FontId::proportional(10.5))
                        .color(palette::text_faint()),
                    );
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
                    &mut self.settings.show_last_msg,
                    tr("Время последнего ответа Claude в списке сессий", "Time since Claude's last reply in the session list"),
                )
                .on_hover_text(tr(
                    "В углу строки, под процентом контекста: сколько прошло с последнего ответа Claude \
                     в этой сессии. Берётся из транскрипта, поэтому у вчерашней сессии там и будет вчера.",
                    "In the row's corner, under the context %: how long since Claude last answered in \
                     that session. Read from the transcript, so yesterday's session says yesterday.",
                ));
                ui.checkbox(
                    &mut self.settings.row_separators,
                    tr("Разделять сессии линией", "Separator line between sessions"),
                );
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
                             работать. Снятие галочки отключает хук и возвращает прежний statusline; \
                             сам скрипт остаётся в ~/.kip/bin.",
                            "Installs a tiny script in ~/.kip/bin and wires it as a Claude Code \
                             statusline hook - the % is exactly what Claude itself shows.\n\
                             An existing statusline is not broken: it gets wrapped and keeps \
                             working. Unchecking unwires the hook and restores the previous \
                             statusline; the script itself stays in ~/.kip/bin.",
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
                            .color(palette::text_dim()),
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
                        ui.label(RichText::new(tr("проверяю...", "checking...")).size(10.5).color(palette::text_faint()));
                    },
                    UpdateState::UpToDate => {
                        ui.label(
                            RichText::new(tr("установлена последняя версия", "you're on the latest version")).size(10.5).color(palette::text_faint()),
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
        self.drain_history();
        // Folders opened from Finder's Services menu ("New kip Window Here").
        #[cfg(target_os = "macos")]
        {
            let pending = mac_service::take_pending();
            if !pending.is_empty() {
                for dir in pending {
                    self.spawn(dir, None, &ctx);
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }
        while let Ok(st) = self.stats_rx.try_recv() {
            self.stats = Some(st);
            self.stats_at = Some(Instant::now());
            self.stats_inflight = false;
        }
        while let Ok(msg) = self.usage_rx.try_recv() {
            match msg {
                usage::UsageMsg::Ok(v) => {
                    self.usage = v;
                    self.usage_err = None;
                },
                // Keep the last good numbers on screen; the panel shows the error.
                usage::UsageMsg::Err(e) => self.usage_err = Some(e),
            }
            self.usage_at = Some(Instant::now());
            self.usage_inflight = false;
        }

        // Paste without text (Finder file, screenshot image) -> insert as a path.
        // Such a pasteboard has no string type, so egui emits no paste event at
        // all; the Cmd+V hook is what tells us it happened. When egui did produce
        // a paste event the text is the paste and the terminal handles it.
        if paste_shortcut() && ctx.input(|i| !i.events.iter().any(|e| matches!(e, egui::Event::Paste(_)))) {
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
                palette::text(),
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

        // Session panel: drag its right edge. Never past half the window (the
        // terminal has to stay the main thing) and never below a width where a
        // row still reads - name, path and the close button.
        let max_w = (ui.max_rect().width() * 0.5).max(SIDEBAR_MIN);
        let panel = egui::Panel::left("sidebar")
            .default_size(self.settings.sidebar_w.clamp(SIDEBAR_MIN, max_w))
            .size_range(SIDEBAR_MIN..=max_w)
            .resizable(true)
            .frame(Frame::new().fill(palette::chrome_sidebar()))
            .show(ui, |ui| self.sidebar(ui));
        // Remember the dragged width; it rides along with the next persist
        // (any session change, and the one on exit) rather than writing state
        // on every pixel of the drag.
        let w = panel.response.rect.width();
        if (w - self.settings.sidebar_w).abs() > 0.5 {
            self.settings.sidebar_w = w;
        }
        self.apply(panel.inner, &ctx);

        if self.explorer.open {
            let acts = egui::Panel::left("explorer")
                .exact_size(240.0)
                .resizable(false)
                .frame(Frame::new().fill(palette::chrome_sidebar()))
                .show(ui, |ui| self.explorer_panel(ui))
                .inner;
            self.apply(acts, &ctx);
        }

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
        self.usage_ui(&ctx);
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

/// Terminal fonts offered in settings, in menu order: (key, label, file to
/// load). `None` means the font already ships inside the binary - "jbmono" is
/// the bundled JetBrains Mono, "Hack" is egui's own default monospace. Which
/// one looks right is a matter of taste (Menlo is thinner than Hack, SF Mono is
/// wider), so this is a setting rather than a guess.
const FONT_CHOICES: &[(&str, &str, Option<(&str, u32)>)] = &[
    // Index 1 of Menlo.ttc is Menlo Bold - a heavier terminal on displays where
    // egui's grayscale antialiasing renders Regular too thin.
    ("menlo", "Menlo", Some(("/System/Library/Fonts/Menlo.ttc", 0))),
    ("menlo-bold", "Menlo Bold", Some(("/System/Library/Fonts/Menlo.ttc", 1))),
    ("sfmono", "SF Mono", Some(("/System/Library/Fonts/SFNSMono.ttf", 0))),
    ("monaco", "Monaco", Some(("/System/Library/Fonts/Monaco.ttf", 0))),
    ("consolas", "Consolas", Some(("C:\\Windows\\Fonts\\consola.ttf", 0))),
    ("cascadia", "Cascadia Mono", Some(("C:\\Windows\\Fonts\\CascadiaMono.ttf", 0))),
    ("dejavu", "DejaVu Sans Mono", Some(("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", 0))),
    ("jetbrains", "JetBrains Mono", None),
    ("hack", "Hack", None),
];

/// The choices whose file is actually present on this machine.
fn available_fonts() -> Vec<(&'static str, &'static str)> {
    FONT_CHOICES
        .iter()
        .filter(|(_, _, src)| src.is_none_or(|(path, _)| std::fs::metadata(path).is_ok()))
        .map(|(key, label, _)| (*key, *label))
        .collect()
}

/// Read a font file, rejecting anything that is not an sfnt container: epaint
/// panics on a font it fails to parse, and this reads whatever the OS ships.
fn load_font_file(path: &str, index: u32) -> Option<FontData> {
    let bytes = std::fs::read(path).ok()?;
    if !matches!(bytes.get(..4)?, b"\x00\x01\x00\x00" | b"true" | b"OTTO" | b"ttcf") {
        return None;
    }
    Some(FontData { font: bytes.into(), index, tweak: Default::default() })
}

/// Installs the picked font and returns the label of what actually got applied -
/// which is not always what was asked for, since a system font can be missing or
/// fail to load, and that difference is exactly what a user cannot see.
fn install_fonts(ctx: &egui::Context, choice: &str) -> &'static str {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "jbmono".into(),
        Arc::new(FontData::from_static(include_bytes!(
            "../resources/JetBrainsMono-Regular.ttf"
        ))),
    );

    // The picked font first, then the bundled JetBrains Mono, so a glyph it
    // misses still lands in a monospace face instead of egui's proportional
    // fallback; egui's own defaults close out the chain (emoji, rare symbols).
    let mut family = Vec::new();
    let mut picked_path = "";
    let mut applied = "JetBrains Mono";
    match FONT_CHOICES.iter().find(|(key, _, _)| *key == choice) {
        Some((_, label, Some((path, index)))) => {
            if let Some(data) = load_font_file(path, *index) {
                fonts.font_data.insert("sys-mono".into(), Arc::new(data));
                family.push("sys-mono".to_owned());
                picked_path = path;
                applied = label;
            }
        },
        Some(("hack", _, _)) => {
            family.push("Hack".to_owned());
            applied = "Hack";
        },
        // "jetbrains", plus an unknown key or a system font that would not load.
        _ => {},
    }
    for name in ["jbmono", "Hack"] {
        if !family.iter().any(|f| f == name) {
            family.push(name.to_owned());
        }
    }

    // Symbol coverage behind the text fonts: Claude Code prints ✳ ✽ (spinner)
    // and ⎿ (tool output). Only Menlo has the first two, only Apple Symbols has
    // the third, so without these they turn into tofu boxes the moment the
    // picked font is something else.
    let symbol_files = [
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Apple Symbols.ttf",
        "C:\\Windows\\Fonts\\seguisym.ttf",
    ];
    for (i, path) in symbol_files.iter().enumerate() {
        if *path == picked_path {
            continue; // already in the family as the picked font
        }
        if let Some(data) = load_font_file(path, 0) {
            let name = format!("sym{i}");
            fonts.font_data.insert(name.clone(), Arc::new(data));
            family.push(name);
        }
    }

    for name in ["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"] {
        if !family.iter().any(|f| f == name) {
            family.push(name.to_owned());
        }
    }
    fonts.families.insert(FontFamily::Monospace, family);
    ctx.set_fonts(fonts);
    applied
}

fn rgb32([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn apply_style(ctx: &egui::Context) {
    // Start from egui's matching base so its many internal defaults (widget
    // text, combobox, sliders) are sane for the mode, then override chrome.
    let mut v = if palette::light() { Visuals::light() } else { Visuals::dark() };
    v.panel_fill = palette::chrome_sidebar();
    v.window_fill = palette::surface();
    v.extreme_bg_color = palette::field_bg();
    v.override_text_color = None;
    v.selection.bg_fill = palette::selection();
    v.widgets.noninteractive.fg_stroke.color = palette::text();
    v.widgets.inactive.bg_fill = palette::ui_bg();
    v.widgets.inactive.weak_bg_fill = palette::ui_bg();
    v.widgets.inactive.fg_stroke.color = palette::text();
    v.widgets.hovered.bg_fill = palette::ui_bg_hover();
    v.widgets.hovered.weak_bg_fill = palette::ui_bg_hover();
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, palette::ui_stroke_hover());
    v.widgets.active.bg_fill = palette::ui_bg_active();
    v.widgets.active.weak_bg_fill = palette::ui_bg_active();
    v.window_stroke = Stroke::new(1.0, palette::border());
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
        // Borrowed key: the dedup allocated a String per line, including the
        // duplicates it then threw away.
        if cmd.is_empty() || !seen.insert(cmd) {
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


/// Group collapse caret: points right when collapsed, down when open.
fn draw_caret(p: &egui::Painter, c: Pos2, collapsed: bool, color: Color32) {
    let r = 3.2;
    let pts = if collapsed {
        vec![
            Pos2::new(c.x - r * 0.5, c.y - r),
            Pos2::new(c.x + r * 0.7, c.y),
            Pos2::new(c.x - r * 0.5, c.y + r),
        ]
    } else {
        vec![
            Pos2::new(c.x - r, c.y - r * 0.5),
            Pos2::new(c.x + r, c.y - r * 0.5),
            Pos2::new(c.x, c.y + r * 0.7),
        ]
    };
    p.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
}

/// Claude marker: an 8-ray star drawn with four crossing lines.
fn draw_star(p: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    for k in 0..4 {
        let a = k as f32 * std::f32::consts::FRAC_PI_4;
        let d = Vec2::new(a.cos(), a.sin()) * r;
        p.line_segment([c - d, c + d], Stroke::new(1.5, color));
    }
}

/// Keep the tail of a long path/string, char-boundary safe. Budgets below the
/// ellipsis itself are clamped: `max - 3` would wrap in release and hand
/// `skip`/`take` a huge count, returning a string LONGER than the input.
fn truncate_head(s: &str, max: usize) -> String {
    let max = max.max(3);
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 3)).collect();
    format!("...{tail}")
}

/// One directory listing, folders first, then case-insensitive by name.
fn read_dir_sorted(dir: &std::path::Path) -> Vec<ExEntry> {
    let mut v: Vec<ExEntry> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| ExEntry {
                is_dir: e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                name: e.file_name().to_string_lossy().into_owned(),
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    v
}

/// Bounded recursive name search: prunes hidden and heavy dirs, caps results
/// and total entries visited so it stays cheap on the UI thread.
fn ex_search(root: &std::path::Path, needle: &str, out: &mut Vec<PathBuf>) {
    const SKIP: &[&str] = &["node_modules", "target", "dist", "build", ".next", ".venv"];
    const MAX_HITS: usize = 400;
    const MAX_VISIT: usize = 20_000;
    let needle = needle.to_lowercase();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            if out.len() >= MAX_HITS || visited >= MAX_VISIT {
                return;
            }
            visited += 1;
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if name.to_lowercase().contains(&needle) {
                out.push(e.path());
            }
            if is_dir && !name.starts_with('.') && !SKIP.contains(&name.as_str()) {
                stack.push(e.path());
            }
        }
    }
}

/// 24px square icon button with hover/active background; `draw` paints the glyph.
fn ex_icon_button(ui: &mut egui::Ui, active: bool, draw: fn(&egui::Painter, Pos2, Color32)) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    if active || resp.hovered() {
        let bg = if active { palette::ui_bg_active() } else { palette::ui_bg_hover() };
        ui.painter().rect_filled(rect, CornerRadius::same(5), bg);
    }
    let col = if active { palette::text() } else { palette::text_dim() };
    draw(ui.painter(), rect.center(), col);
    resp
}

/// Full-width clickable file/dir row with a hover highlight.
fn ex_row(ui: &mut egui::Ui, label: &str, is_dir: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(4), palette::ui_bg_hover());
    }
    let (text, col) =
        if is_dir { (format!("{label}/"), palette::text()) } else { (label.to_string(), palette::text_dim()) };
    ui.painter().text(
        Pos2::new(rect.left() + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        text,
        FontId::monospace(12.5),
        col,
    );
    resp
}

fn draw_search(p: &egui::Painter, c: Pos2, col: Color32) {
    let center = Pos2::new(c.x - 1.0, c.y - 1.0);
    p.circle_stroke(center, 4.0, Stroke::new(1.5, col));
    let a = center + Vec2::angled(std::f32::consts::FRAC_PI_4) * 4.0;
    let b = center + Vec2::angled(std::f32::consts::FRAC_PI_4) * 8.5;
    p.line_segment([a, b], Stroke::new(1.5, col));
}

fn draw_newfile(p: &egui::Painter, c: Pos2, col: Color32) {
    let page = Rect::from_center_size(c, Vec2::new(9.0, 12.0));
    p.rect_stroke(page, CornerRadius::same(1), Stroke::new(1.4, col), StrokeKind::Inside);
    p.line_segment([Pos2::new(c.x - 2.3, c.y), Pos2::new(c.x + 2.3, c.y)], Stroke::new(1.4, col));
    p.line_segment([Pos2::new(c.x, c.y - 2.3), Pos2::new(c.x, c.y + 2.3)], Stroke::new(1.4, col));
}

/// Keep the head, char-boundary safe. See `truncate_head` for the clamp.
fn truncate_end(s: &str, max: usize) -> String {
    let max = max.max(3);
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

/// Select the whole rename buffer so the first keystroke replaces the old
/// name instead of appending. Runs the frame focus is requested; the TextEdit
/// has already stored its state by then, so load-modify-store wins.
fn select_all_text(ctx: &egui::Context, id: egui::Id, char_len: usize) {
    use egui::text::{CCursor, CCursorRange};
    if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) {
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(char_len))));
        state.store(ctx, id);
    }
}

/// Cmd+V pressed since the last frame. Only macOS hooks the shortcut (that is
/// where the file/image pasteboard is read), elsewhere egui's paste event is all
/// there is.
fn paste_shortcut() -> bool {
    #[cfg(target_os = "macos")]
    return mac_service::take_paste();
    #[cfg(not(target_os = "macos"))]
    false
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

/// Pushpin: a head and a needle, filled once the limit is pinned. Drawn rather
/// than typed - the UI font has no pin glyph to rely on.
fn paint_pin(p: &egui::Painter, c: Pos2, filled: bool, col: Color32) {
    let head = Pos2::new(c.x, c.y - 2.0);
    if filled {
        p.circle_filled(head, 3.4, col);
    } else {
        p.circle_stroke(head, 3.0, Stroke::new(1.3, col));
    }
    p.line_segment(
        [Pos2::new(c.x, c.y + 1.4), Pos2::new(c.x, c.y + 5.5)],
        Stroke::new(1.4, col),
    );
}

/// Put the pinned ids back on the slot numbers they held before a move, and let
/// the rest keep their new order in whatever slots are left. A pinned row that
/// someone else was dropped onto stays put; the newcomer takes the next slot.
fn seat_pinned(members: &[u64], anchors: &[(u64, usize)]) -> Vec<u64> {
    let len = members.len();
    if len == 0 {
        return Vec::new();
    }
    let mut slots: Vec<Option<u64>> = vec![None; len];
    let mut want: Vec<(u64, usize)> =
        anchors.iter().filter(|(id, _)| members.contains(id)).copied().collect();
    want.sort_by_key(|&(_, slot)| slot);
    for (id, slot) in want {
        // Two pins claiming one slot (the list got shorter): the later one takes
        // the next free place, wrapping rather than dropping anybody.
        let mut at = slot.min(len - 1);
        while slots[at].is_some() {
            at = (at + 1) % len;
        }
        slots[at] = Some(id);
    }
    let seated: Vec<u64> = slots.iter().flatten().copied().collect();
    let mut rest = members.iter().filter(|id| !seated.contains(id));
    for slot in slots.iter_mut() {
        if slot.is_none() {
            *slot = rest.next().copied();
        }
    }
    slots.into_iter().flatten().collect()
}

/// Filled width of a limit bar. A freshly reset window sits at 1-2%, which is a
/// third of a pixel on the corner chip - too little to tell "just reset" from
/// "not loaded", so anything above zero keeps a visible stub.
fn bar_fill(track_w: f32, pct: f32) -> f32 {
    let w = track_w * (pct / 100.0).clamp(0.0, 1.0);
    if pct > 0.0 { w.max(3.0) } else { 0.0 }
}

/// Usage bar color: green under 60%, amber under 90%, red above.
fn usage_color(pct: f32) -> Color32 {
    if pct < 60.0 {
        GIT_ADD
    } else if pct < 90.0 {
        ORANGE
    } else {
        GIT_DEL
    }
}

/// Countdown to a limit reset, given as unix seconds.
fn fmt_until(target: u64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let left = target.saturating_sub(now);
    let body = if left >= 86400 {
        format!("{}{} {}{}", left / 86400, tr("д", "d"), left % 86400 / 3600, tr("ч", "h"))
    } else if left >= 3600 {
        format!("{}{} {}{}", left / 3600, tr("ч", "h"), left % 3600 / 60, tr("м", "m"))
    } else {
        format!("{}{}", left / 60 + 1, tr("м", "m"))
    };
    format!("{} {body}", tr("сброс через", "resets in"))
}

/// Age of Claude's last reply, in the narrowest form that still reads: the row
/// corner has room for a few glyphs, not for "2 hours 14 minutes ago".
fn fmt_ago(t: SystemTime) -> String {
    let s = SystemTime::now().duration_since(t).map(|d| d.as_secs()).unwrap_or(0);
    if s < 60 {
        tr("сейчас", "now").to_string()
    } else if s < 3600 {
        format!("{}{}", s / 60, tr("м", "m"))
    } else if s < 86400 {
        format!("{}{}", s / 3600, tr("ч", "h"))
    } else {
        format!("{}{}", s / 86400, tr("д", "d"))
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

#[cfg(test)]
mod font_tests {
    /// epaint panics on a font it cannot parse, so every choice the settings
    /// offer has to actually lay out text on this machine.
    #[test]
    fn every_offered_font_renders() {
        let ctx = egui::Context::default();
        for (key, _) in super::available_fonts() {
            super::install_fonts(&ctx, key);
            let _ = ctx.run_ui(Default::default(), |ui| {
                let w = ui.ctx().fonts_mut(|f| f.glyph_width(&egui::FontId::monospace(13.0), '0'));
                assert!(w > 0.0, "{key}: no glyph width");
            });
        }
    }
}

#[cfg(test)]
mod pin_tests {
    use super::seat_pinned;

    #[test]
    fn pinned_row_keeps_its_slot_number() {
        // 1 2 3 4 5 with 3 pinned to slot 2; 4 is dropped above 2. The plain
        // move gives 1 4 2 3 5 - 3 must come back to slot 2, pushing 2 down.
        assert_eq!(seat_pinned(&[1, 4, 2, 3, 5], &[(3, 2)]), vec![1, 4, 3, 2, 5]);
        // 2 pinned to slot 1; 5 dropped on top. Plain move: 5 1 2 3 4.
        assert_eq!(seat_pinned(&[5, 1, 2, 3, 4], &[(2, 1)]), vec![5, 2, 1, 3, 4]);
        // Dropping something straight onto the pinned slot leaves it alone.
        assert_eq!(seat_pinned(&[1, 2, 4, 3, 5], &[(3, 2)]), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn two_pins_and_a_shrunken_list() {
        assert_eq!(seat_pinned(&[4, 1, 2, 3], &[(1, 0), (3, 2)]), vec![1, 4, 3, 2]);
        // The anchor outlives the row count it was taken at: clamp, never drop.
        assert_eq!(seat_pinned(&[1, 2], &[(2, 7)]), vec![1, 2]);
        assert!(seat_pinned(&[], &[(1, 0)]).is_empty());
    }
}

#[cfg(test)]
mod explorer_tests {
    use super::{ex_search, read_dir_sorted};
    use std::fs;

    #[test]
    fn search_finds_nested_and_prunes_heavy_dirs() {
        let base = std::env::temp_dir().join(format!("kip_ex_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("src")).unwrap();
        fs::create_dir_all(base.join("node_modules/pkg")).unwrap();
        fs::write(base.join("src/main.rs"), "").unwrap();
        fs::write(base.join("README.md"), "").unwrap();
        fs::write(base.join("node_modules/pkg/main.rs"), "").unwrap();

        let mut hits = Vec::new();
        ex_search(&base, "main", &mut hits);
        // Finds the nested src/main.rs, skips anything under node_modules.
        assert!(hits.iter().any(|p| p.ends_with("src/main.rs")), "missing src/main.rs: {hits:?}");
        assert!(!hits.iter().any(|p| p.to_string_lossy().contains("node_modules")), "did not prune node_modules: {hits:?}");

        // Case-insensitive.
        let mut hits2 = Vec::new();
        ex_search(&base, "readme", &mut hits2);
        assert!(hits2.iter().any(|p| p.ends_with("README.md")), "case-insensitive miss: {hits2:?}");

        // Listing puts folders before files.
        let list = read_dir_sorted(&base);
        let first_file = list.iter().position(|e| !e.is_dir);
        let last_dir = list.iter().rposition(|e| e.is_dir);
        if let (Some(f), Some(d)) = (first_file, last_dir) {
            assert!(d < f, "folders should sort before files: {:?}", list.iter().map(|e| (&e.name, e.is_dir)).collect::<Vec<_>>());
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn truncate_clamps_budget_below_ellipsis() {
        use super::{truncate_end, truncate_head};
        // max < 3 used to wrap `max - 3` in release and hand take()/skip() a
        // huge count - truncate_end then returned a string LONGER than input.
        for max in 0..=3 {
            assert_eq!(truncate_end("abcdefgh", max), "...");
            assert_eq!(truncate_head("abcdefgh", max), "...");
        }
        // Normal budgets unchanged, multibyte never split.
        assert_eq!(truncate_end("abcdefgh", 5), "ab...");
        assert_eq!(truncate_head("abcdefgh", 5), "...gh");
        assert_eq!(truncate_end("abc", 8), "abc");
        assert_eq!(truncate_end("привет", 5), "пр...");
    }
}
