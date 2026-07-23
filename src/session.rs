use std::collections::HashMap;
#[cfg(not(windows))]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

use crate::config::{SavedSession, Settings};

#[derive(Clone)]
pub struct EventProxy {
    pub id: u64,
    pub tx: Sender<(u64, Event)>,
    pub ctx: egui::Context,
    /// Id of the currently active session; output of background sessions coalesces repaints.
    pub active: Arc<AtomicU64>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.tx.send((self.id, event));
        if self.active.load(Ordering::Relaxed) == self.id {
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
}

impl Drop for LiveTerm {
    fn drop(&mut self) {
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
    pub cwd: PathBuf,
    pub phase: Phase,
    pub title: String,
    /// Session name from Claude Code (~/.claude/sessions metadata).
    pub claude_title: Option<String>,
    pub last_activity: Instant,
    pub spawned_at: SystemTime,
    pub claude_session_id: Option<String>,
    pub skip_permissions: bool,
    pub keep_awake: bool,
    pub snapshot: Option<String>,
    pub unread: bool,
    pub busy: bool,
    pub busy_since: Option<Instant>,
    pub git: Option<GitStats>,
    pub last_git_poll: Option<Instant>,
    pub git_inflight: bool,
    pub scroll_accum: f32,
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

    /// Claude's session name when known, directory name otherwise.
    pub fn display_name(&self) -> String {
        self.claude_title.clone().unwrap_or_else(|| self.name())
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
        let fg_pid = self.live().and_then(|l| crate::plat::foreground_pgid(l.master_fd, l.shell_pid));
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
            skip_permissions: self.skip_permissions,
            keep_awake: self.keep_awake,
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
        Session {
            id,
            cwd: saved.cwd,
            phase: Phase::Suspended,
            title: String::new(),
            claude_title: saved.claude_title,
            last_activity: Instant::now(),
            spawned_at: SystemTime::now(),
            claude_session_id: saved.claude_session_id,
            skip_permissions: saved.skip_permissions,
            keep_awake: saved.keep_awake,
            snapshot: saved.snapshot,
            unread: false,
            busy: false,
            busy_since: None,
            git: None,
            last_git_poll: None,
            git_inflight: false,
            scroll_accum: 0.0,
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

fn grab_snapshot(term: &FairMutex<Term<EventProxy>>) -> String {
    let term = term.lock();
    let grid = term.grid();
    let top = grid.topmost_line().0.max(-200);
    let start = Point::new(Line(top), Column(0));
    let end = Point::new(grid.bottommost_line(), Column(grid.columns().saturating_sub(1)));
    let text = term.bounds_to_string(start, end);
    text.trim_end().to_string()
}

pub fn spawn_live(
    id: u64,
    cwd: &Path,
    command: Option<String>,
    settings: &Settings,
    proxy: EventProxy,
    cols: u16,
    rows: u16,
    cell_size: (u16, u16),
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
    env.insert("TERM_PROGRAM".into(), "rwarp".into());

    // Built via Default so the windows-only `escape_args` field is set too.
    let mut opts = PtyOptions::default();
    opts.shell = Some(shell);
    opts.working_directory = Some(cwd.to_path_buf());
    opts.drain_on_exit = false;
    opts.env = env;

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

    let event_loop = match EventLoop::new(term.clone(), proxy, pty, false, false) {
        Ok(el) => el,
        Err(e) => {
            #[cfg(not(windows))]
            unsafe {
                libc::close(master_fd)
            };
            return Err(e.into());
        },
    };
    let notifier = Notifier(event_loop.channel());
    event_loop.spawn();

    Ok(LiveTerm { term, notifier, master_fd, shell_pid, cols, rows })
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
        if tx.send((id, ClaudeInfo { session_id: store_sid, name, claude_pid: cpid })).is_ok() {
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
        if let (Some(min), Ok(mt)) = (min_mtime, meta.modified()) {
            if mt < min - Duration::from_secs(10) {
                return;
            }
        }
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        if v["pid"].as_i64() != Some(pid as i64) {
            return;
        }
        let Some(sid) = v["sessionId"].as_str().filter(|s| valid_sid(s)) else { return };
        let info = ClaudeInfo {
            session_id: Some(sid.to_string()),
            name: v["name"].as_str().map(String::from),
            claude_pid: Some(pid),
        };
        if tx.send((id, info)).is_ok() {
            ctx.request_repaint();
        }
    });
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
                name: v["name"].as_str().map(String::from),
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
    fn sid_validation() {
        assert!(valid_uuid(SID) && valid_sid(SID));
        assert!(!valid_uuid("not-a-uuid") && !valid_sid("../etc/passwd"));
        assert!(valid_sid("shortid") && !valid_uuid("shortid"));
    }
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
