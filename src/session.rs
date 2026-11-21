use std::collections::HashMap;
#[cfg(not(windows))]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};

use crate::config::{Agent, SavedSession, Settings};

#[derive(Clone)]
pub struct EventProxy {
    pub id: u64,
    pub tx: Sender<(u64, Event)>,
    pub ctx: egui::Context,
    /// Sessions on screen (one per window); output of the others coalesces repaints.
    pub active: Arc<Mutex<Vec<u64>>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.tx.send((self.id, event));
        if self.active.lock().is_ok_and(|a| a.contains(&self.id)) {
            self.ctx.request_repaint();
        } else {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

pub struct LiveTerm {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub notifier: Notifier,
    pub master_fd: i32,
    pub shell_pid: i32,
    pub cols: u16,
    pub rows: u16,
    /// Size the widget wants and since when; applied once it stops changing.
    pub pending_size: Option<((u16, u16), Instant)>,
    /// Kill the shell tree when this LiveTerm drops. True for intentional
    /// teardown (suspend, close, quit) where the shell is still alive; cleared
    /// by `finalize_exit`, where the shell already exited and its pid may have
    /// been reused - killing it then would target an unrelated process.
    pub kill_on_drop: bool,
}

impl Drop for LiveTerm {
    fn drop(&mut self) {
        // Kill the shell and its whole descendant tree. claude and the node
        // processes it spawns run in their own process groups, so closing the
        // PTY alone (a lone SIGHUP to the shell) leaves them alive as orphans
        // under launchd, still eating RAM. Every intentional teardown - suspend,
        // close tab, quit - drops LiveTerm, so this is the single place that
        // frees them. finalize_exit clears the flag (the shell already exited).
        if self.kill_on_drop {
            crate::plat::kill_tree(self.shell_pid);
        }
        let _ = self.notifier.0.send(Msg::Shutdown);
        // Windows has no master fd; the Pty (dropped with the event loop) owns
        // its own handle cleanup.
        #[cfg(not(windows))]
        unsafe {
            libc::close(self.master_fd)
        };
    }
}

pub enum Phase {
    Live(LiveTerm),
    Suspended,
    Exited(Option<i32>),
}

#[derive(Clone, Default)]
pub struct GitStats {
    pub is_repo: bool,
    pub branch: String,
    pub added: u32,
    pub deleted: u32,
}

pub struct Session {
    pub id: u64,
    /// Window this session is shown in (`SavedWindow::id`).
    pub window: u64,
    pub cwd: PathBuf,
    pub phase: Phase,
    pub title: String,
    /// Session name from Claude Code (~/.claude/sessions metadata).
    pub claude_title: Option<String>,
    /// User-set name (double-click rename). Overrides claude_title and dir name.
    pub custom_name: Option<String>,
    /// Manual group (None = ungrouped). Reorder/move-between via drag & drop.
    pub group: Option<String>,
    pub last_activity: Instant,
    pub spawned_at: SystemTime,
    pub claude_session_id: Option<String>,
    /// Which CLI agent owns `claude_session_id` (None on legacy saves = Claude).
    pub session_agent: Option<Agent>,
    pub skip_permissions: bool,
    pub keep_awake: bool,
    /// Pinned = protected: cannot be dragged, closed by mistake, or auto-suspended.
    pub pinned: bool,
    pub snapshot: Option<String>,
    pub unread: bool,
    /// The agent finished while nobody was looking; the row glows until seen.
    pub done: bool,
    /// When Claude's current turn started (its metadata status went "busy").
    pub turn_since: Option<Instant>,
    pub busy: bool,
    pub busy_since: Option<Instant>,
    pub git: Option<GitStats>,
    pub last_git_poll: Option<Instant>,
    pub git_inflight: bool,
    pub scroll_accum: f32,
    /// Last selection and whether it was on the alt screen. Claude repaints its
    /// live area with erase-line/erase-below, and alacritty drops any selection
    /// those touch; we put it back so a drag or a copy survives the repaint.
    pub sel_keep: Option<(alacritty_terminal::selection::Selection, bool)>,
    /// Ctrl+F matches in this terminal (grid points) and the one jumped to.
    pub find_hits: Vec<alacritty_terminal::term::search::Match>,
    pub find_cur: Option<usize>,
    /// (cwd, project root) last worked out for it; see `project_root`. The
    /// lookup stats folders, so it runs off the UI thread: `proj_asked` is the
    /// cwd a lookup is out for, `proj_seq` sorts a row that changed project
    /// after the ones already there.
    pub project: Option<(PathBuf, PathBuf)>,
    pub proj_asked: Option<PathBuf>,
    pub proj_seq: u64,
    /// Auto-repeat streak of a held Backspace (drives word-wise erase).
    pub bs_repeat: u8,
    /// Name of the PTY foreground process while busy (e.g. "claude").
    pub fg_name: Option<String>,
    /// The foreground process is claude (checked by name and executable path).
    pub fg_is_claude: bool,
    /// Pid of the claude process itself - the PTY leader or its descendant
    /// when claude runs behind a wrapper (session pickers etc).
    pub claude_pid: Option<i32>,
    /// claude ran in this terminal at least once - gates the newest-jsonl fallback.
    pub saw_claude: bool,
    /// Command we sent that has not started yet (picked up on the idle->busy edge).
    pub pending_cmd: Option<String>,
    /// Command line of the currently running job, for the sticky header.
    pub running_cmd: Option<String>,
    pub last_ctx_poll: Option<Instant>,
    /// Throttle for the fast foreground probe in the Wakeup handler.
    pub last_fg_probe: Option<Instant>,
    /// Post-start window of 300ms context polling (claude just launched/resumed).
    pub burst_until: Option<Instant>,
    /// Context poller bookkeeping (file stat results between ticks).
    pub ctx_stat: crate::ctx_index::CtxStat,
}

impl Session {
    pub fn name(&self) -> String {
        self.cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned())
    }

    /// User name if set, else Claude's session name, else the directory name.
    pub fn display_name(&self) -> String {
        self.custom_name
            .clone()
            .or_else(|| self.claude_title.clone())
            .unwrap_or_else(|| self.name())
    }

    pub fn live(&self) -> Option<&LiveTerm> {
        match &self.phase {
            Phase::Live(l) => Some(l),
            _ => None,
        }
    }

    /// Which agent this session belongs to: its own captured agent, else Claude
    /// for legacy sessions that already carry an id, else the global setting.
    pub fn effective_agent(&self, settings: &Settings) -> Agent {
        self.session_agent.unwrap_or(if self.claude_session_id.is_some() {
            Agent::Claude
        } else {
            settings.agent
        })
    }

    pub fn resume_command(&self, settings: &Settings) -> Option<String> {
        let id = self.claude_session_id.as_ref().filter(|s| valid_sid(s))?;
        let agent = self.effective_agent(settings);
        let mut cmd = match agent {
            Agent::Claude => format!("{} --resume {}", settings.claude_cmd, id),
            Agent::Agy => format!("{} --conversation {}", settings.agy_cmd, id),
        };
        if self.skip_permissions {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        Some(cmd)
    }

    /// Remember the agent session of THIS terminal. Only a pid-exact match
    /// (the agent process in this PTY's foreground) may overwrite an already
    /// captured id - several sessions in the same directory must never converge
    /// on whichever transcript happens to be newest.
    pub fn save_claude_session(&mut self, agent: Agent) {
        // Only a foreground process that is NOT the shell itself can be the agent.
        // Without this filter an idle terminal still scanned the whole sessions
        // directory looking for a pid-exact hit that cannot exist - wasted work
        // on the UI thread, which runs this for every session on suspend and on
        // exit.
        let fg_pid = self.live().and_then(|l| {
            crate::plat::foreground_pgid(l.master_fd, l.shell_pid).filter(|pg| *pg != l.shell_pid)
        });
        match agent {
            Agent::Claude => {
                let hit = fg_pid
                    .and_then(|_| find_claude_meta(&self.cwd, None, fg_pid, self.spawned_at))
                    .filter(|m| m.score >= 3);
                if let Some(m) = hit {
                    self.claude_session_id = Some(m.session_id);
                    self.session_agent = Some(Agent::Claude);
                    if m.name.is_some() {
                        self.claude_title = m.name;
                    }
                } else if self.claude_session_id.is_none() && self.saw_claude {
                    // Newest-jsonl fallback only when claude actually ran here - otherwise
                    // it would happily adopt a session started from some other terminal.
                    if let Some(id) = detect_claude_session(&self.cwd, Some(self.spawned_at)) {
                        self.claude_session_id = Some(id);
                        self.session_agent = Some(Agent::Claude);
                    }
                }
            },
            Agent::Agy => {
                let pid_sid = self
                    .claude_pid
                    .or(fg_pid)
                    .and_then(crate::plat::agy_pid_conversation);
                if let Some(sid) = pid_sid {
                    self.claude_session_id = Some(sid.clone());
                    self.session_agent = Some(Agent::Agy);
                    if let Some(t) = agy_title_for_sid(&sid) {
                        self.claude_title = Some(t);
                    }
                } else if self.claude_session_id.is_none()
                    && self.saw_claude
                    && let Some(id) = detect_agy_session(&self.cwd, Some(self.spawned_at))
                {
                    self.claude_session_id = Some(id);
                    self.session_agent = Some(Agent::Agy);
                }
            },
        }
    }

    /// Kill the child and free terminal memory, keeping a text snapshot.
    pub fn suspend(&mut self, agent: Agent) {
        if let Phase::Live(live) = &self.phase {
            self.snapshot = Some(grab_snapshot(&live.term));
            self.save_claude_session(agent);
            self.phase = Phase::Suspended;
        }
        self.busy = false;
        self.busy_since = None;
    }

    pub fn finalize_exit(&mut self, code: Option<i32>, agent: Agent) {
        // The shell exited on its own and alacritty already reaped it, so its
        // pid may now belong to an unrelated process; disarm the kill in
        // LiveTerm::Drop (its children have reparented to launchd anyway).
        if let Phase::Live(live) = &mut self.phase {
            live.kill_on_drop = false;
        }
        if let Phase::Live(live) = &self.phase {
            self.snapshot = Some(grab_snapshot(&live.term));
            self.save_claude_session(agent);
            self.phase = Phase::Exited(code);
        }
        self.busy = false;
    }

    pub fn to_saved(&self) -> SavedSession {
        let mut snapshot = self.snapshot.clone();
        if let Phase::Live(live) = &self.phase {
            snapshot = Some(grab_snapshot(&live.term));
        }
        SavedSession {
            cwd: self.cwd.clone(),
            claude_session_id: self.claude_session_id.clone(),
            session_agent: self.session_agent,
            claude_title: self.claude_title.clone(),
            custom_name: self.custom_name.clone(),
            group: self.group.clone(),
            skip_permissions: self.skip_permissions,
            keep_awake: self.keep_awake,
            pinned: self.pinned,
            window: self.window,
            snapshot: snapshot.map(|mut s| {
                if s.len() > 32 * 1024 {
                    let cut = s.len() - 32 * 1024;
                    let cut = s.char_indices().map(|(i, _)| i).find(|&i| i >= cut).unwrap_or(0);
                    s = s.split_off(cut);
                }
                s
            }),
        }
    }

    pub fn from_saved(saved: SavedSession, id: u64) -> Self {
        let fresh_title = saved
            .claude_session_id
            .as_deref()
            .filter(|s| valid_sid(s))
            .and_then(|sid| match saved.session_agent {
                Some(Agent::Agy) => agy_title_for_sid(sid),
                _ => transcript_title(&project_dir(&saved.cwd)?.join(format!("{sid}.jsonl"))),
            });
        Session {
            id,
            window: saved.window,
            cwd: saved.cwd,
            phase: Phase::Suspended,
            title: String::new(),
            claude_title: fresh_title.or(saved.claude_title),
            custom_name: saved.custom_name,
            group: saved.group,
            last_activity: Instant::now(),
            spawned_at: SystemTime::now(),
            claude_session_id: saved.claude_session_id,
            session_agent: saved.session_agent,
            skip_permissions: saved.skip_permissions,
            keep_awake: saved.keep_awake,
            pinned: saved.pinned,
            snapshot: saved.snapshot,
            unread: false,
            done: false,
            turn_since: None,
            busy: false,
            busy_since: None,
            git: None,
            last_git_poll: None,
            git_inflight: false,
            scroll_accum: 0.0,
            sel_keep: None,
            find_hits: Vec::new(),
            find_cur: None,
            project: None,
            proj_asked: None,
            proj_seq: 0,
            bs_repeat: 0,
            fg_name: None,
            fg_is_claude: false,
            claude_pid: None,
            saw_claude: false,
            pending_cmd: None,
            running_cmd: None,
            last_ctx_poll: None,
            last_fg_probe: None,
            burst_until: None,
            ctx_stat: Default::default(),
        }
    }
}

pub(crate) fn grab_snapshot(term: &FairMutex<Term<EventProxy>>) -> String {
    let term = term.lock();
    let grid = term.grid();
    let top = grid.topmost_line().0.max(-200);
    let start = Point::new(Line(top), Column(0));
    let end = Point::new(grid.bottommost_line(), Column(grid.columns().saturating_sub(1)));
    let text = term.bounds_to_string(start, end);
    text.trim_end().to_string()
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_live(
    id: u64,
    cwd: &Path,
    command: Option<String>,
    settings: &Settings,
    proxy: EventProxy,
    cols: u16,
    rows: u16,
    cell_size: (u16, u16),
    seed: Option<&str>,
) -> std::io::Result<LiveTerm> {
    #[cfg(not(windows))]
    let shell = {
        let prog = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        match command {
            Some(cmd) => Shell::new(prog, vec!["-i".into(), "-l".into(), "-c".into(), cmd]),
            None => Shell::new(prog, vec!["-i".into(), "-l".into()]),
        }
    };
    #[cfg(windows)]
    let shell = {
        let prog = "powershell.exe".to_string();
        match command {
            // PowerShell's -Command consumes the rest of the line, so the
            // command's own spaces need no extra escaping.
            Some(cmd) => Shell::new(prog, vec!["-NoLogo".into(), "-Command".into(), cmd]),
            None => Shell::new(prog, vec!["-NoLogo".into()]),
        }
    };

    let mut env = HashMap::new();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env.insert("TERM_PROGRAM".into(), "kip".into());

    // Built via Default so the windows-only `escape_args` field is set too.
    #[allow(clippy::field_reassign_with_default)]
    let opts = {
        let mut o = PtyOptions::default();
        o.shell = Some(shell);
        o.working_directory = Some(cwd.to_path_buf());
        o.drain_on_exit = false;
        o.env = env;
        o
    };

    let window_size = WindowSize {
        num_lines: rows,
        num_cols: cols,
        cell_width: cell_size.0,
        cell_height: cell_size.1,
    };

    let pty = tty::new(&opts, window_size, id)?;
    #[cfg(not(windows))]
    let (master_fd, shell_pid) =
        (unsafe { libc::dup(pty.file().as_raw_fd()) }, pty.child().id() as i32);
    #[cfg(windows)]
    let (master_fd, shell_pid) =
        (-1, pty.child_watcher().pid().map(|p| p.get() as i32).unwrap_or(0));

    let term_config = TermConfig {
        scrolling_history: settings.scrollback,
        kitty_keyboard: true,
        ..Default::default()
    };
    let term = Term::new(term_config, &TermSize::new(cols as usize, rows as usize), proxy.clone());
    let term = Arc::new(FairMutex::new(term));

    // Preload scrollback (e.g. a suspended session's snapshot) before the shell
    // runs, so the on-screen history survives "reopen as terminal". Done before
    // the event loop starts, so the shell's prompt lands after the history.
    if let Some(seed) = seed {
        let mut parser =
            alacritty_terminal::vte::ansi::Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        parser.advance(&mut *term.lock(), seed.as_bytes());
    }

    let event_loop = match EventLoop::new(term.clone(), proxy, pty, false, false) {
        Ok(el) => el,
        Err(e) => {
            #[cfg(not(windows))]
            unsafe {
                libc::close(master_fd)
            };
            return Err(e);
        },
    };
    let notifier = Notifier(event_loop.channel());
    event_loop.spawn();

    Ok(LiveTerm { term, notifier, master_fd, shell_pid, cols, rows, pending_size: None, kill_on_drop: true })
}

/// Claude session ids are UUIDs. Everything else (filenames, external JSON)
/// is rejected: the id ends up interpolated into a shell command on resume.
pub(crate) fn valid_sid(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// What a claude invocation resumes, recovered from its command line.
pub enum ResumeHint {
    Sid(String),
    /// `--continue`: the newest session of the cwd.
    Latest,
}

/// Strict UUID form. `--resume` also accepts a search term for its picker -
/// that (or a clipped argv) must never be mistaken for a session id.
fn valid_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Parse a typed command or a process argv for an agent resume target. The id
/// is known the moment the user hits Enter - no need to wait for the agent.
pub fn parse_resume_hint(cmd: &str, agent: Agent) -> Option<ResumeHint> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    // Only a command that actually launches the agent counts (not `echo claude ...`).
    let first = tokens.first()?;
    let base = first.rsplit(['/', '\\']).next().unwrap_or(first).to_ascii_lowercase();
    let ok = match agent {
        Agent::Claude => base.contains("claude"),
        Agent::Agy => base == "agy" || base == "agy.exe",
    };
    if !ok {
        return None;
    }
    resume_from_tokens(&tokens, agent)
}

/// argv of a VERIFIED agent process: no first-token check - the binary's
/// basename may be a bare version number (".../claude/versions/2.1.217"), and
/// wrappers may exec it under any argv0.
pub fn parse_resume_hint_argv(cmd: &str, agent: Agent) -> Option<ResumeHint> {
    resume_from_tokens(&cmd.split_whitespace().collect::<Vec<_>>(), agent)
}

fn resume_from_tokens(tokens: &[&str], agent: Agent) -> Option<ResumeHint> {
    for (i, t) in tokens.iter().enumerate() {
        match *t {
            "--resume" | "-r" => {
                let sid = tokens.get(i + 1).filter(|s| valid_uuid(s))?;
                return Some(ResumeHint::Sid(sid.to_string()));
            },
            "--conversation" if agent == Agent::Agy => {
                let sid = tokens.get(i + 1).filter(|s| valid_uuid(s))?;
                return Some(ResumeHint::Sid(sid.to_string()));
            },
            "--continue" | "-c" => return Some(ResumeHint::Latest),
            _ => {
                let sid = t
                    .strip_prefix("--resume=")
                    .or_else(|| (agent == Agent::Agy).then(|| t.strip_prefix("--conversation=")).flatten())
                    .or_else(|| t.strip_prefix("-r"))
                    .filter(|s| valid_uuid(s));
                if let Some(sid) = sid {
                    return Some(ResumeHint::Sid(sid.to_string()));
                }
            },
        }
    }
    None
}

/// Claude Code config dir: CLAUDE_CONFIG_DIR when set, ~/.claude otherwise.
pub(crate) fn claude_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
}

/// Antigravity CLI data dir: ~/.gemini/antigravity-cli.
pub(crate) fn agy_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("antigravity-cli"))
}

/// Antigravity shared config dir: ~/.gemini/config (mcp_config.json, hooks.json).
pub(crate) fn agy_config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".gemini").join("config"))
}

/// Transcript file of one Antigravity CLI conversation.
pub(crate) fn agy_transcript_path(sid: &str) -> Option<PathBuf> {
    valid_sid(sid).then(|| {
        agy_dir()
            .map(|d| d.join("brain").join(sid).join(".system_generated").join("logs").join("transcript.jsonl"))
    })?
}

/// Decode a `file:///path/to/dir` workspace URI from agy metadata into a PathBuf.
pub(crate) fn agy_uri_to_path(uri: &str) -> PathBuf {
    let raw = uri.strip_prefix("file://").unwrap_or(uri);
    let mut bytes = Vec::with_capacity(raw.len());
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(hex) = std::str::from_utf8(&b[i + 1..i + 3])
            && let Ok(val) = u8::from_str_radix(hex, 16)
        {
            bytes.push(val);
            i += 3;
            continue;
        }
        bytes.push(b[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&bytes);
    // Windows file URIs look like file:///C:/work -> strip leading slash before drive letter.
    let s = if decoded.len() >= 3
        && decoded.as_bytes()[0] == b'/'
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        &decoded[1..]
    } else {
        decoded.as_ref()
    };
    PathBuf::from(s)
}

/// Strip Antigravity's `<USER_REQUEST>...</USER_REQUEST>` prompt envelope from
/// a transcript `USER_INPUT` content string.
pub(crate) fn agy_strip_user_request(content: &str) -> String {
    if let Some(start) = content.find("<USER_REQUEST>") {
        let rest = &content[start + "<USER_REQUEST>".len()..];
        let body = match rest.find("</USER_REQUEST>") {
            Some(end) => &rest[..end],
            None => rest,
        };
        return body.trim().to_string();
    }
    content.trim().to_string()
}

pub(crate) struct AgyConvMeta {
    pub title: Option<String>,
    pub preview: Option<String>,
    pub cwd: Option<PathBuf>,
}

/// Cached conversation summaries written by agy (`cache/conversation_metadata.json`).
pub(crate) fn agy_meta_map() -> HashMap<String, AgyConvMeta> {
    let mut out = HashMap::new();
    let Some(path) = agy_dir().map(|d| d.join("cache").join("conversation_metadata.json")) else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(path) else { return out };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return out };
    let Some(convs) = v["conversations"].as_object() else { return out };
    for (sid, entry) in convs {
        if !valid_sid(sid) {
            continue;
        }
        let sum = &entry["summary"];
        let title = sum["Title"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let preview = sum["Preview"].as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from);
        let cwd = sum["WorkspaceURIs"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|u| u.as_str())
            .filter(|s| !s.is_empty())
            .map(agy_uri_to_path);
        out.insert(sid.clone(), AgyConvMeta { title, preview, cwd });
    }
    out
}

/// Workspace map from agy's append-only `history.jsonl` (covers brand-new
/// conversations not yet flushed to `conversation_metadata.json`).
pub(crate) fn agy_history_cwds() -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let Some(path) = agy_dir().map(|d| d.join("history.jsonl")) else { return out };
    let Ok(text) = std::fs::read_to_string(path) else { return out };
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(sid) = v["conversationId"].as_str().filter(|s| valid_sid(s)) else { continue };
        let Some(ws) = v["workspace"].as_str().filter(|s| !s.is_empty()) else { continue };
        out.insert(sid.to_string(), PathBuf::from(ws));
    }
    out
}

fn agy_transcript_first_prompt(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 32 * 1024];
    let n = f.read(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]);
    for line in text.lines() {
        if !line.contains("\"USER_INPUT\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"].as_str() != Some("USER_INPUT") {
            continue;
        }
        let raw = v["content"].as_str()?;
        let clean = agy_strip_user_request(raw);
        let first = clean.lines().next().unwrap_or("").trim();
        if !first.is_empty() {
            return Some(first.chars().take(80).collect());
        }
    }
    None
}

/// Best available title for an agy conversation: live statusline hook title
/// (when matching sid), else metadata Title/Preview, else first user prompt.
pub(crate) fn agy_title_for_sid(sid: &str) -> Option<String> {
    if let Some(p) = crate::ctx_index::agy_status_path()
        && let Ok(text) = std::fs::read_to_string(p)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && v["session_id"].as_str() == Some(sid)
        && let Some(t) = v["conversation_title"].as_str().map(str::trim).filter(|s| !s.is_empty())
    {
        return Some(t.to_string());
    }
    if let Some(m) = agy_meta_map().remove(sid)
        && let Some(t) = m.title.or(m.preview)
    {
        return Some(t);
    }
    agy_transcript_path(sid).and_then(|p| agy_transcript_first_prompt(&p))
}

/// Derive agy's turn status ("busy" vs "idle") from the tail of transcript.jsonl:
/// a final PLANNER_RESPONSE without tool_calls means the turn finished.
pub(crate) fn agy_transcript_status(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 64 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let kind = v["type"].as_str()?;
        let done = v["status"].as_str() == Some("DONE");
        let has_tools = v["tool_calls"].as_array().is_some_and(|a| !a.is_empty());
        let has_content = v["content"].as_str().is_some_and(|c| !c.trim().is_empty());
        return if kind == "PLANNER_RESPONSE" && done && !has_tools && has_content {
            Some("idle".into())
        } else {
            Some("busy".into())
        };
    }
    None
}

/// Detect the newest Antigravity CLI conversation for `cwd`.
pub fn detect_agy_session(cwd: &Path, only_after: Option<SystemTime>) -> Option<String> {
    let cwd_str = cwd.to_string_lossy();
    let fresh = |sid: &str| -> bool {
        let Some(after) = only_after else { return true };
        let t_ok = agy_transcript_path(sid)
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
            .is_some_and(|mt| mt >= after);
        if t_ok {
            return true;
        }
        agy_dir()
            .map(|d| d.join("presence").join(format!("{sid}.lock")))
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
            .is_some_and(|mt| mt >= after)
    };
    if let Some(path) = agy_dir().map(|d| d.join("cache").join("last_conversations.json"))
        && let Ok(text) = std::fs::read_to_string(path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(sid) = v[cwd_str.as_ref()].as_str().filter(|s| valid_sid(s))
        && fresh(sid)
    {
        return Some(sid.to_string());
    }
    let after_ms = only_after
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    if let Some(path) = agy_dir().map(|d| d.join("history.jsonl"))
        && let Ok(text) = std::fs::read_to_string(path)
    {
        for line in text.lines().rev() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            let Some(sid) = v["conversationId"].as_str().filter(|s| valid_sid(s)) else { continue };
            if v["workspace"].as_str() != Some(cwd_str.as_ref()) {
                continue;
            }
            if let Some(min_ms) = after_ms
                && v["timestamp"].as_u64().is_some_and(|ts| ts < min_ms)
            {
                continue;
            }
            return Some(sid.to_string());
        }
    }
    None
}

pub fn detect_agent_session(cwd: &Path, only_after: Option<SystemTime>, agent: Agent) -> Option<String> {
    match agent {
        Agent::Claude => detect_claude_session(cwd, only_after),
        Agent::Agy => detect_agy_session(cwd, only_after),
    }
}

/// ~/.claude/projects directory for a cwd (Claude Code path encoding).
pub(crate) fn project_dir(cwd: &Path) -> Option<PathBuf> {
    let enc: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(claude_dir()?.join("projects").join(enc))
}

/// Newest Claude Code session jsonl for a directory,
/// optionally only among files modified after `only_after`.
fn newest_jsonl(cwd: &Path, only_after: Option<SystemTime>) -> Option<PathBuf> {
    let dir = project_dir(cwd)?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        if only_after.is_some_and(|t| mtime < t) {
            continue;
        }
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

pub fn detect_claude_session(cwd: &Path, only_after: Option<SystemTime>) -> Option<String> {
    newest_jsonl(cwd, only_after)?
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| valid_sid(s))
        .map(String::from)
}

pub struct ClaudeInfo {
    pub session_id: Option<String>,
    pub name: Option<String>,
    /// The actual claude process (the PTY leader itself, or its descendant
    /// when claude runs behind a wrapper tool).
    pub claude_pid: Option<i32>,
    /// Claude's own turn status from its metadata ("busy", "idle", ...).
    pub status: Option<String>,
}

/// Resolve the active agent session running in a terminal and read its name,
/// id, and turn status. `fg_pid` is the PTY foreground leader; the real agent
/// may be its descendant (wrappers) - resolved here, in the background.
#[allow(clippy::too_many_arguments)]
pub fn poll_claude(
    id: u64,
    cwd: PathBuf,
    spawned_at: SystemTime,
    known_id: Option<String>,
    fg_pid: Option<i32>,
    agent: Agent,
    tx: Sender<(u64, ClaudeInfo)>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let info = match agent {
            Agent::Claude => {
                let cpid = fg_pid.and_then(|p| crate::plat::find_agent_desc(p, Agent::Claude));
                let meta = find_claude_meta(&cwd, known_id.as_deref(), cpid, spawned_at);
                // A cwd-only guess (score 1) may be another terminal's session:
                // good enough to display, never good enough to overwrite the saved id.
                let (mut store_sid, name) = match meta {
                    Some(m) => ((m.score >= 2).then(|| m.session_id.clone()), m.name),
                    None => (None, None),
                };
                // The claude argv may carry the resumed id (alias/scripts/wrappers), pid-exact.
                match cpid
                    .and_then(crate::plat::process_args)
                    .and_then(|a| parse_resume_hint_argv(&a, Agent::Claude))
                {
                    Some(ResumeHint::Sid(sid)) => store_sid = Some(sid),
                    Some(ResumeHint::Latest) => {
                        if let Some(sid) = detect_claude_session(&cwd, None) {
                            store_sid = Some(sid);
                        }
                    },
                    None => {},
                }
                ClaudeInfo { session_id: store_sid, name, claude_pid: cpid, status: None }
            },
            Agent::Agy => {
                let cpid = fg_pid.and_then(|p| crate::plat::find_agent_desc(p, Agent::Agy));
                let mut store_sid = cpid.and_then(crate::plat::agy_pid_conversation);
                if store_sid.is_none() {
                    match cpid
                        .and_then(crate::plat::process_args)
                        .and_then(|a| parse_resume_hint_argv(&a, Agent::Agy))
                    {
                        Some(ResumeHint::Sid(sid)) => store_sid = Some(sid),
                        Some(ResumeHint::Latest) => store_sid = detect_agy_session(&cwd, None),
                        None => {},
                    }
                }
                if store_sid.is_none() && cpid.is_some() {
                    store_sid = detect_agy_session(&cwd, Some(spawned_at));
                }
                let lookup_sid = store_sid.as_deref().or(known_id.as_deref());
                let name = lookup_sid.and_then(agy_title_for_sid);
                let status = if cpid.is_some() {
                    lookup_sid
                        .and_then(agy_transcript_path)
                        .and_then(|p| agy_transcript_status(&p))
                } else {
                    None
                };
                ClaudeInfo { session_id: store_sid, name, claude_pid: cpid, status }
            },
        };
        if tx.send((id, info)).is_ok() {
            ctx.request_repaint();
        }
    });
}

/// claude's own per-pid metadata file - THE universal binding source: written
/// by claude itself for any launch method (picker, --continue, wrappers).
pub fn meta_path(pid: i32) -> Option<PathBuf> {
    claude_dir().map(|d| d.join("sessions").join(format!("{pid}.json")))
}

/// Read one metadata file and report its sessionId/name. `min_mtime` guards
/// against a stale leftover of a dead claude that had the same pid.
pub fn spawn_meta_read(
    id: u64,
    pid: i32,
    min_mtime: Option<SystemTime>,
    tx: Sender<(u64, ClaudeInfo)>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let Some(path) = meta_path(pid) else { return };
        let Ok(meta) = std::fs::metadata(&path) else { return };
        if meta.len() > 16 * 1024 {
            return;
        }
        if let (Some(min), Ok(mt)) = (min_mtime, meta.modified())
            && mt < min - Duration::from_secs(10) {
                return;
            }
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        if v["pid"].as_i64() != Some(pid as i64) {
            return;
        }
        let Some(sid) = v["sessionId"].as_str().filter(|s| valid_sid(s)) else { return };
        let info = ClaudeInfo {
            session_id: Some(sid.to_string()),
            name: meta_name(&v, sid),
            claude_pid: Some(pid),
            status: v["status"].as_str().map(String::from),
        };
        if tx.send((id, info)).is_ok() {
            ctx.request_repaint();
        }
    });
}

/// Session title: a name the user gave (/rename), else the title Claude
/// generated into the transcript, else Claude's derived "dir-xx" name.
fn meta_name(v: &serde_json::Value, sid: &str) -> Option<String> {
    let name = v["name"].as_str().map(String::from);
    if v["nameSource"].as_str().is_some_and(|s| s != "derived" && s != "collision") {
        return name;
    }
    let cwd = Path::new(v["cwd"].as_str()?);
    transcript_title(&project_dir(cwd)?.join(format!("{sid}.jsonl"))).or(name)
}

/// Last custom/ai title in the transcript tail (Claude re-appends it every turn).
pub(crate) fn transcript_title(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 512 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    let (mut custom, mut ai) = (None, None);
    for line in text.lines() {
        if !line.contains("-title\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match v["type"].as_str() {
            Some("custom-title") => custom = v["customTitle"].as_str().map(String::from),
            Some("ai-title") => ai = v["aiTitle"].as_str().map(String::from),
            _ => {},
        }
    }
    custom.or(ai).filter(|t| !t.trim().is_empty())
}

struct MetaHit {
    session_id: String,
    name: Option<String>,
    score: i32,
    updated: u64,
}

fn find_claude_meta(
    cwd: &Path,
    known_id: Option<&str>,
    fg_pid: Option<i32>,
    spawned_at: SystemTime,
) -> Option<MetaHit> {
    let dir = claude_dir()?.join("sessions");
    let spawned_ms = spawned_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut best: Option<MetaHit> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if entry.metadata().is_ok_and(|m| m.len() > 16 * 1024) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(sid) = v["sessionId"].as_str().filter(|s| valid_sid(s)) else { continue };
        let pid = v["pid"].as_i64();
        let mcwd = v["cwd"].as_str();
        let updated = v["updatedAt"].as_u64().unwrap_or(0);
        let score = if fg_pid.is_some() && pid == fg_pid.map(i64::from) {
            3
        } else if known_id == Some(sid) {
            2
        } else if mcwd == Some(cwd.to_string_lossy().as_ref()) && updated >= spawned_ms {
            1
        } else {
            continue;
        };
        let better = best
            .as_ref()
            .is_none_or(|b| score > b.score || (score == b.score && updated > b.updated));
        if better {
            best = Some(MetaHit {
                session_id: sid.to_string(),
                name: meta_name(&v, sid),
                score,
                updated,
            });
        }
    }
    best
}

pub fn poll_git(id: u64, cwd: PathBuf, tx: Sender<(u64, PathBuf, GitStats)>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let stats = git_stats(&cwd);
        if tx.send((id, cwd, stats)).is_ok() {
            ctx.request_repaint();
        }
    });
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        // Never let a poll hang on a credential prompt or fight over index locks.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_stats(cwd: &Path) -> GitStats {
    // `branch --show-current` fails outside a repo (doubles as the repo check)
    // and works in repos without commits; empty output = detached HEAD.
    let Some(raw_branch) = git(cwd, &["branch", "--show-current"]) else {
        return GitStats::default();
    };
    let branch = Some(raw_branch)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| git(cwd, &["rev-parse", "--short", "HEAD"]).map(|s| s.trim().to_string()))
        .unwrap_or_default();
    let mut stats = GitStats { is_repo: true, branch, ..Default::default() };
    let numstat = git(cwd, &["diff", "HEAD", "--numstat"])
        .or_else(|| git(cwd, &["diff", "--numstat"]))
        .unwrap_or_default();
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let a = parts.next().unwrap_or("-");
        let d = parts.next().unwrap_or("-");
        stats.added += a.parse::<u32>().unwrap_or(0);
        stats.deleted += d.parse::<u32>().unwrap_or(0);
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    fn sid_of(cmd: &str) -> Option<String> {
        match parse_resume_hint(cmd, Agent::Claude) {
            Some(ResumeHint::Sid(s)) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn resume_hint_forms() {
        assert_eq!(sid_of(&format!("claude --resume {SID}")).as_deref(), Some(SID));
        assert_eq!(sid_of(&format!("claude -r{SID}")).as_deref(), Some(SID));
        assert_eq!(sid_of(&format!("claude --resume={SID} -x")).as_deref(), Some(SID));
        assert!(matches!(parse_resume_hint("claude --continue", Agent::Claude), Some(ResumeHint::Latest)));
    }

    #[test]
    fn resume_hint_agy_forms() {
        assert!(matches!(
            parse_resume_hint(&format!("agy --conversation {SID}"), Agent::Agy),
            Some(ResumeHint::Sid(s)) if s == SID
        ));
        assert!(matches!(
            parse_resume_hint(&format!("/Users/x/.local/bin/agy --conversation={SID}"), Agent::Agy),
            Some(ResumeHint::Sid(s)) if s == SID
        ));
        assert!(matches!(parse_resume_hint("agy -c", Agent::Agy), Some(ResumeHint::Latest)));
        assert!(parse_resume_hint(&format!("echo agy --conversation {SID}"), Agent::Agy).is_none());
        assert!(parse_resume_hint(&format!("claude --resume {SID}"), Agent::Agy).is_none());
    }

    #[test]
    fn resume_hint_rejects_search_term() {
        // --resume also takes a picker search term - never a session id.
        assert!(parse_resume_hint("claude --resume fix-the-bug", Agent::Claude).is_none());
        // Clipped argv: a truncated uuid must not pass.
        assert!(parse_resume_hint(&format!("claude --resume {}", &SID[..20]), Agent::Claude).is_none());
    }

    #[test]
    fn resume_hint_requires_claude_command() {
        assert!(parse_resume_hint(&format!("echo claude --resume {SID}"), Agent::Claude).is_none());
        assert_eq!(sid_of(&format!("/usr/local/bin/claude -r {SID}")).as_deref(), Some(SID));
    }

    #[test]
    fn resume_hint_argv_accepts_version_binary() {
        // The claude binary's basename is a bare version number; the typed-command
        // parser rejects it, the argv parser (verified claude process) must not.
        let argv = format!("/Users/x/.local/share/claude/versions/2.1.217 --resume {SID}");
        assert!(parse_resume_hint(&argv, Agent::Claude).is_none());
        assert!(matches!(parse_resume_hint_argv(&argv, Agent::Claude), Some(ResumeHint::Sid(s)) if s == SID));
    }

    #[test]
    fn agy_helpers_parse_uris_and_requests() {
        assert_eq!(agy_uri_to_path("file:///Users/a/my%20repo"), PathBuf::from("/Users/a/my repo"));
        assert_eq!(agy_uri_to_path("file:///C:/work/app"), PathBuf::from("C:/work/app"));
        assert_eq!(
            agy_strip_user_request("<USER_REQUEST>\nПривет\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"),
            "Привет"
        );
        let p = std::env::temp_dir().join(format!("kip-agy-st-{}.jsonl", std::process::id()));
        std::fs::write(&p, "{\"type\":\"USER_INPUT\",\"status\":\"DONE\",\"content\":\"hi\"}\n").unwrap();
        assert_eq!(agy_transcript_status(&p).as_deref(), Some("busy"));
        std::fs::write(
            &p,
            "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"tool_calls\":[{\"name\":\"run_command\"}]}\n",
        )
        .unwrap();
        assert_eq!(agy_transcript_status(&p).as_deref(), Some("busy"));
        std::fs::write(
            &p,
            "{\"type\":\"PLANNER_RESPONSE\",\"status\":\"DONE\",\"content\":\"Done!\"}\n",
        )
        .unwrap();
        assert_eq!(agy_transcript_status(&p).as_deref(), Some("idle"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn resume_command_uses_session_agent() {
        let settings = Settings::default();
        let mut s = Session::from_saved(
            SavedSession {
                claude_session_id: Some(SID.into()),
                session_agent: Some(Agent::Agy),
                skip_permissions: true,
                ..Default::default()
            },
            1,
        );
        assert_eq!(
            s.resume_command(&settings).as_deref(),
            Some(format!("agy --conversation {SID} --dangerously-skip-permissions").as_str())
        );
        s.session_agent = None;
        assert_eq!(
            s.resume_command(&settings).as_deref(),
            Some(format!("claude --resume {SID} --dangerously-skip-permissions").as_str())
        );
    }

    #[test]
    fn seed_lands_in_scrollback() {
        // The "reopen as terminal" seed must be parsed into the grid before the
        // shell runs, so the suspended snapshot stays on screen. Feed the same
        // bytes spawn_live feeds and read them back.
        use alacritty_terminal::event::VoidListener;
        use alacritty_terminal::term::{Config, Term};
        use alacritty_terminal::index::{Column, Point};
        use alacritty_terminal::grid::Dimensions;

        let size = TermSize::new(80, 24);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let seed = "line one\r\nline two\r\n";
        let mut parser =
            alacritty_terminal::vte::ansi::Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        parser.advance(&mut term, seed.as_bytes());

        let grid = term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), Column(grid.columns() - 1));
        let text = term.bounds_to_string(start, end);
        assert!(text.contains("line one") && text.contains("line two"), "seed missing: {text:?}");
    }

    #[test]
    fn sid_validation() {
        assert!(valid_uuid(SID) && valid_sid(SID));
        assert!(!valid_uuid("not-a-uuid") && !valid_sid("../etc/passwd"));
        assert!(valid_sid("shortid") && !valid_uuid("shortid"));
    }

    #[test]
    fn transcript_title_prefers_custom() {
        let p = std::env::temp_dir().join(format!("kip-title-{}.jsonl", std::process::id()));
        std::fs::write(
            &p,
            "{\"type\":\"ai-title\",\"aiTitle\":\"old\"}\n{\"type\":\"user\"}\n\
             {\"type\":\"ai-title\",\"aiTitle\":\"Чистка ADB\"}\n",
        )
        .unwrap();
        assert_eq!(transcript_title(&p).as_deref(), Some("Чистка ADB"));
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, b"{\"type\":\"custom-title\",\"customTitle\":\"mine\"}\n").unwrap();
        assert_eq!(transcript_title(&p).as_deref(), Some("mine"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn pin_survives_a_restart() {
        // A state file written before pinning existed loads as unpinned...
        let old: SavedSession = serde_json::from_str(r#"{"cwd":"/tmp"}"#).unwrap();
        assert!(!old.pinned);
        // ...and a pin set in this run is still there after save/load.
        let s = Session::from_saved(SavedSession { pinned: true, ..old }, 1);
        assert!(s.pinned && s.to_saved().pinned);
    }
}
