use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use portable_pty::{CommandBuilder, MasterPty, PtySize};

use crate::config::{SavedSession, Settings};
use crate::vt;

/// What a session's terminal reports to the UI thread.
pub enum TermEvent {
    /// New output was parsed.
    Wakeup,
    Bell,
    Title(String),
    /// OSC 52: the program put text on the clipboard.
    Clipboard(String),
    /// The shell exited (None: killed by a signal).
    ChildExit(Option<i32>),
}

#[derive(Clone)]
pub struct EventProxy {
    pub id: u64,
    pub tx: Sender<(u64, TermEvent)>,
    pub ctx: egui::Context,
    /// Sessions on screen (one per window); output of the others coalesces repaints.
    pub active: Arc<Mutex<Vec<u64>>>,
}

impl EventProxy {
    pub fn send_event(&self, event: TermEvent) {
        let _ = self.tx.send((self.id, event));
        if self.active.lock().is_ok_and(|a| a.contains(&self.id)) {
            self.ctx.request_repaint();
        } else {
            self.ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// The terminal core shared by the PTY reader thread and the UI thread.
pub struct TermLock(Mutex<vt::Term>);

impl TermLock {
    pub fn lock(&self) -> MutexGuard<'_, vt::Term> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Bytes for the PTY, written by a thread of its own so a program that stops
/// reading its input never blocks the UI.
pub struct Notifier(Sender<Vec<u8>>);

impl Notifier {
    pub fn notify(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.0.send(bytes);
        }
    }
}

pub struct LiveTerm {
    pub term: Arc<TermLock>,
    pub notifier: Notifier,
    master: Box<dyn MasterPty>,
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
    /// Set on drop: the reader stops, the exit watcher stays quiet.
    closed: Arc<AtomicBool>,
    /// Theme last handed to the terminal core (palette, default colors).
    pub theme: Option<crate::palette::Theme>,
}

impl LiveTerm {
    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
        self.term.lock().resize(cols, rows, cell_w as u32, cell_h as u32);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: cols.saturating_mul(cell_w),
            pixel_height: rows.saturating_mul(cell_h),
        });
        self.cols = cols;
        self.rows = rows;
    }

    /// Follow the app theme: palette colors and OSC 10/11 answers.
    pub fn sync_theme(&mut self) {
        let th = crate::palette::theme();
        if self.theme.as_ref() == Some(&th) {
            return;
        }
        let c = |c: egui::Color32| [c.r(), c.g(), c.b()];
        self.term.lock().set_colors(c(th.fg), c(th.bg), c(th.cursor), &crate::palette::term_palette(), crate::palette::light());
        self.theme = Some(th);
    }
}

impl Drop for LiveTerm {
    fn drop(&mut self) {
        // Kill the shell and its whole descendant tree. claude and the node
        // processes it spawns run in their own process groups, so closing the
        // PTY alone (a lone SIGHUP to the shell) leaves them alive as orphans
        // under launchd, still eating RAM. Every intentional teardown - suspend,
        // close tab, quit - drops LiveTerm, so this is the single place that
        // frees them. finalize_exit clears the flag (the shell already exited).
        self.closed.store(true, Ordering::SeqCst);
        if self.kill_on_drop {
            crate::plat::kill_tree(self.shell_pid);
        }
        // The master closes with `master`; the reader and writer threads hold
        // their own handles and exit on `closed` / the dropped notifier.
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
    /// Mouse selection in progress: what it selects and where it started.
    pub sel_drag: Option<crate::term_view::SelDrag>,
    /// Ctrl+F matches in this terminal (buffer positions) and the one jumped to.
    pub find_hits: Vec<crate::term_view::Match>,
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

    pub fn resume_command(&self, settings: &Settings) -> Option<String> {
        let id = self.claude_session_id.as_ref().filter(|s| valid_sid(s))?;
        let mut cmd = format!("{} --resume {}", settings.claude_cmd, id);
        if self.skip_permissions {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        Some(cmd)
    }

    /// Remember the Claude session of THIS terminal. Only a pid-exact match
    /// (the claude process in this PTY's foreground) may overwrite an already
    /// captured id - several sessions in the same directory must never converge
    /// on whichever transcript happens to be newest.
    pub fn save_claude_session(&mut self) {
        // Only a foreground process that is NOT the shell itself can be claude.
        // Without this filter an idle terminal still scanned the whole sessions
        // directory looking for a pid-exact hit that cannot exist - wasted work
        // on the UI thread, which runs this for every session on suspend and on
        // exit.
        let fg_pid = self.live().and_then(|l| {
            crate::plat::foreground_pgid(l.master_fd, l.shell_pid).filter(|pg| *pg != l.shell_pid)
        });
        let hit = fg_pid
            .and_then(|_| find_claude_meta(&self.cwd, None, fg_pid, self.spawned_at))
            .filter(|m| m.score >= 3);
        if let Some(m) = hit {
            self.claude_session_id = Some(m.session_id);
            if m.name.is_some() {
                self.claude_title = m.name;
            }
        } else if self.claude_session_id.is_none() && self.saw_claude {
            // Newest-jsonl fallback only when claude actually ran here - otherwise
            // it would happily adopt a session started from some other terminal.
            if let Some(id) = detect_claude_session(&self.cwd, Some(self.spawned_at)) {
                self.claude_session_id = Some(id);
            }
        }
    }

    /// Kill the child and free terminal memory, keeping a text snapshot.
    pub fn suspend(&mut self) {
        if let Phase::Live(live) = &self.phase {
            self.snapshot = Some(grab_snapshot(&live.term));
            self.save_claude_session();
            self.phase = Phase::Suspended;
        }
        self.busy = false;
        self.busy_since = None;
    }

    pub fn finalize_exit(&mut self, code: Option<i32>) {
        // The shell exited on its own and the exit watcher already reaped it, so its
        // pid may now belong to an unrelated process; disarm the kill in
        // LiveTerm::Drop (its children have reparented to launchd anyway).
        if let Phase::Live(live) = &mut self.phase {
            live.kill_on_drop = false;
        }
        if let Phase::Live(live) = &self.phase {
            self.snapshot = Some(grab_snapshot(&live.term));
            self.save_claude_session();
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
            .and_then(|sid| transcript_title(&project_dir(&saved.cwd)?.join(format!("{sid}.jsonl"))));
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
            sel_drag: None,
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

pub(crate) fn grab_snapshot(term: &TermLock) -> String {
    let term = term.lock();
    // The screen plus up to 200 rows of scrollback above it.
    let from = term.history().saturating_sub(200) as u32;
    term.text_from(from).trim_end().to_string()
}

fn io_err(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
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
    let mut cmd = {
        let prog = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let mut c = CommandBuilder::new(prog);
        c.args(["-i", "-l"]);
        if let Some(cmd) = command {
            c.args(["-c", cmd.as_str()]);
        }
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        let mut c = CommandBuilder::new("powershell.exe");
        c.arg("-NoLogo");
        // PowerShell's -Command consumes the rest of the line, so the
        // command's own spaces need no extra escaping.
        if let Some(cmd) = command {
            c.args(["-Command", cmd.as_str()]);
        }
        c
    };
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "kip");

    let size = PtySize {
        rows,
        cols,
        pixel_width: cols.saturating_mul(cell_size.0),
        pixel_height: rows.saturating_mul(cell_size.1),
    };
    let pair = portable_pty::native_pty_system().openpty(size).map_err(io_err)?;

    let mut term = vt::Term::new(cols, rows, settings.scrollback);
    term.resize(cols, rows, cell_size.0 as u32, cell_size.1 as u32);
    // Preload scrollback (e.g. a suspended session's snapshot) before the shell
    // runs, so the on-screen history survives "reopen as terminal" and the
    // shell's prompt lands after it.
    if let Some(seed) = seed {
        term.write(seed.as_bytes());
        term.take_pty_out();
        term.take_events();
    }
    let term = Arc::new(TermLock(Mutex::new(term)));

    let mut child = pair.slave.spawn_command(cmd).map_err(io_err)?;
    drop(pair.slave);
    let master = pair.master;
    let shell_pid = child.process_id().map(|p| p as i32).unwrap_or(0);
    let closed = Arc::new(AtomicBool::new(false));

    #[cfg(not(windows))]
    let (master_fd, reader, mut writer) = {
        use std::os::fd::{AsRawFd, FromRawFd};
        let fd = master.as_raw_fd().unwrap_or(-1);
        // Own handles: the reader polls so it can notice `closed`, the writer
        // must not send EOF on drop (portable-pty's does). Non-blocking (the
        // flag is shared by both dups): the reader drains the PTY one read()
        // per chunk, as alacritty did.
        let dup = |fd| unsafe {
            let d = libc::dup(fd);
            if d < 0 { Err(std::io::Error::last_os_error()) } else { Ok(std::fs::File::from_raw_fd(d)) }
        };
        let file = dup(fd)?;
        unsafe {
            let fl = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let r: Box<dyn Read + Send> = Box::new(PollReader { file, closed: closed.clone() });
        let w: Box<dyn Write + Send> = Box::new(PollWriter(dup(fd)?));
        (fd, r, w)
    };
    #[cfg(windows)]
    let (master_fd, reader, mut writer) =
        (-1, master.try_clone_reader().map_err(io_err)?, master.take_writer().map_err(io_err)?);

    let (wtx, wrx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new().name(format!("pty-write-{id}")).spawn(move || {
        for bytes in wrx {
            if writer.write_all(&bytes).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    })?;

    {
        let (term, proxy, wtx, mut reader) = (term.clone(), proxy.clone(), wtx.clone(), reader);
        std::thread::Builder::new().name(format!("pty-read-{id}")).spawn(move || {
            let mut buf = vec![0u8; 1024 * 1024];
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                let (reply, events) = {
                    let mut t = term.lock();
                    t.write(&buf[..n]);
                    (t.take_pty_out(), t.take_events())
                };
                if !reply.is_empty() {
                    let _ = wtx.send(reply);
                }
                for e in events {
                    proxy.send_event(match e {
                        vt::Event::Bell => TermEvent::Bell,
                        vt::Event::Title(t) => TermEvent::Title(t),
                        vt::Event::Clipboard(t) => TermEvent::Clipboard(t),
                    });
                }
                proxy.send_event(TermEvent::Wakeup);
            }
        })?;
    }

    {
        let closed = closed.clone();
        std::thread::Builder::new().name(format!("pty-wait-{id}")).spawn(move || {
            let code = child.wait().ok().and_then(|s| s.signal().is_none().then_some(s.exit_code() as i32));
            if !closed.load(Ordering::SeqCst) {
                proxy.send_event(TermEvent::ChildExit(code));
            }
        })?;
    }

    let mut live = LiveTerm {
        term,
        notifier: Notifier(wtx),
        master,
        master_fd,
        shell_pid,
        cols,
        rows,
        pending_size: None,
        kill_on_drop: true,
        closed,
        theme: None,
    };
    live.sync_theme();
    Ok(live)
}

/// Blocking PTY reads that give up once the session is torn down: the master
/// has other open handles, so a plain read would wait for the last process on
/// the terminal to exit.
#[cfg(not(windows))]
struct PollReader {
    file: std::fs::File,
    closed: Arc<AtomicBool>,
}

#[cfg(not(windows))]
fn wait_fd(fd: i32, events: i16, timeout_ms: i32) -> std::io::Result<bool> {
    let mut p = libc::pollfd { fd, events, revents: 0 };
    match unsafe { libc::poll(&mut p, 1, timeout_ms) } {
        n if n < 0 => {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted { Ok(false) } else { Err(e) }
        },
        n => Ok(n > 0),
    }
}

#[cfg(not(windows))]
impl Read for PollReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::ErrorKind::{Interrupted, WouldBlock};
        use std::os::fd::AsRawFd;
        let mut n = 0;
        loop {
            // Drain what is there so the parser lock and the UI wakeup happen
            // once per batch, not once per ~1KB the PTY hands out.
            while n < buf.len() {
                match self.file.read(&mut buf[n..]) {
                    Ok(0) => return Ok(n),
                    Ok(m) => n += m,
                    Err(e) if e.kind() == WouldBlock => break,
                    Err(e) if e.kind() == Interrupted => {},
                    Err(_) if n > 0 => return Ok(n),
                    Err(e) => return Err(e),
                }
            }
            if n > 0 {
                return Ok(n);
            }
            if self.closed.load(Ordering::SeqCst) {
                return Ok(0);
            }
            wait_fd(self.file.as_raw_fd(), libc::POLLIN, 200)?;
        }
    }
}

/// Writes to the non-blocking master, waiting while the PTY is full.
#[cfg(not(windows))]
struct PollWriter(std::fs::File);

#[cfg(not(windows))]
impl Write for PollWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        loop {
            match self.0.write(buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    wait_fd(self.0.as_raw_fd(), libc::POLLOUT, 1000)?;
                },
                r => return r,
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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

/// Parse a typed command or a process argv for a claude resume target. The id
/// is known the moment the user hits Enter - no need to wait for claude.
pub fn parse_resume_hint(cmd: &str) -> Option<ResumeHint> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    // Only a command that actually launches claude counts (not `echo claude ...`).
    let first = tokens.first()?;
    if !first.rsplit('/').next().unwrap_or(first).contains("claude") {
        return None;
    }
    resume_from_tokens(&tokens)
}

/// argv of a VERIFIED claude process: no first-token check - the binary's
/// basename is a bare version number (".../claude/versions/2.1.217"), and
/// wrappers may exec it under any argv0.
pub fn parse_resume_hint_argv(cmd: &str) -> Option<ResumeHint> {
    resume_from_tokens(&cmd.split_whitespace().collect::<Vec<_>>())
}

fn resume_from_tokens(tokens: &[&str]) -> Option<ResumeHint> {
    for (i, t) in tokens.iter().enumerate() {
        match *t {
            "--resume" | "-r" => {
                let sid = tokens.get(i + 1).filter(|s| valid_uuid(s))?;
                return Some(ResumeHint::Sid(sid.to_string()));
            },
            "--continue" | "-c" => return Some(ResumeHint::Latest),
            _ => {
                let sid = t
                    .strip_prefix("--resume=")
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

/// Resolve the Claude session running in a terminal (via ~/.claude/sessions
/// metadata, matched by claude pid, then saved id, then cwd) and read its
/// name and id. `fg_pid` is the PTY foreground leader; the real claude may be
/// its descendant (wrappers like cchb) - resolved here, in the background.
pub fn poll_claude(
    id: u64,
    cwd: PathBuf,
    spawned_at: SystemTime,
    known_id: Option<String>,
    fg_pid: Option<i32>,
    tx: Sender<(u64, ClaudeInfo)>,
    ctx: egui::Context,
) {
    std::thread::spawn(move || {
        let cpid = fg_pid.and_then(crate::plat::find_claude_desc);
        let meta = find_claude_meta(&cwd, known_id.as_deref(), cpid, spawned_at);
        // A cwd-only guess (score 1) may be another terminal's session:
        // good enough to display, never good enough to overwrite the saved id.
        let (mut store_sid, name) = match meta {
            Some(m) => ((m.score >= 2).then(|| m.session_id.clone()), m.name),
            None => (None, None),
        };
        // The claude argv may carry the resumed id (alias/scripts/wrappers), pid-exact.
        match cpid.and_then(crate::plat::process_args).and_then(|a| parse_resume_hint_argv(&a)) {
            Some(ResumeHint::Sid(sid)) => store_sid = Some(sid),
            Some(ResumeHint::Latest) => {
                if let Some(sid) = detect_claude_session(&cwd, None) {
                    store_sid = Some(sid);
                }
            },
            None => {},
        }
        if tx.send((id, ClaudeInfo { session_id: store_sid, name, claude_pid: cpid, status: None })).is_ok() {
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
        match parse_resume_hint(cmd) {
            Some(ResumeHint::Sid(s)) => Some(s),
            _ => None,
        }
    }

    #[test]
    fn resume_hint_forms() {
        assert_eq!(sid_of(&format!("claude --resume {SID}")).as_deref(), Some(SID));
        assert_eq!(sid_of(&format!("claude -r{SID}")).as_deref(), Some(SID));
        assert_eq!(sid_of(&format!("claude --resume={SID} -x")).as_deref(), Some(SID));
        assert!(matches!(parse_resume_hint("claude --continue"), Some(ResumeHint::Latest)));
    }

    #[test]
    fn resume_hint_rejects_search_term() {
        // --resume also takes a picker search term - never a session id.
        assert!(parse_resume_hint("claude --resume fix-the-bug").is_none());
        // Clipped argv: a truncated uuid must not pass.
        assert!(parse_resume_hint(&format!("claude --resume {}", &SID[..20])).is_none());
    }

    #[test]
    fn resume_hint_requires_claude_command() {
        assert!(parse_resume_hint(&format!("echo claude --resume {SID}")).is_none());
        assert_eq!(sid_of(&format!("/usr/local/bin/claude -r {SID}")).as_deref(), Some(SID));
    }

    #[test]
    fn resume_hint_argv_accepts_version_binary() {
        // The claude binary's basename is a bare version number; the typed-command
        // parser rejects it, the argv parser (verified claude process) must not.
        let argv = format!("/Users/x/.local/share/claude/versions/2.1.217 --resume {SID}");
        assert!(parse_resume_hint(&argv).is_none());
        assert!(matches!(parse_resume_hint_argv(&argv), Some(ResumeHint::Sid(s)) if s == SID));
    }

    #[test]
    fn seed_lands_in_scrollback() {
        // The "reopen as terminal" seed must be parsed into the grid before the
        // shell runs, so the suspended snapshot stays on screen. Feed the same
        // bytes spawn_live feeds and read them back.
        let mut term = vt::Term::new(80, 24, 1000);
        term.write(b"line one\r\nline two\r\n");
        let text = grab_snapshot(&TermLock(Mutex::new(term)));
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
