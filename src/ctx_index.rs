//! Context-% pipeline: hook snapshots + jsonl estimates merged by source time.
//!
//! Data flow: background threads read `~/.kip/ctx` snapshots (written by the
//! statusline hook) and estimate from `~/.claude/projects/**.jsonl` transcripts,
//! then send `CtxMsg` over mpsc; the UI drains the channel into `CtxIndex` and
//! renders badges with a synchronous lookup. The UI thread never reads files.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::config::Settings;
use crate::session::{claude_dir, project_dir, valid_sid};

pub const HOOK_SCRIPT: &str = include_str!("../resources/kip-ctx-hook.sh");

const SNAPSHOT_MAX: u64 = 64 * 1024;
/// Progressive transcript tail: one jsonl line can exceed 64KB (huge tool
/// results), which would otherwise read as "no usage at all".
const TAILS: [u64; 2] = [64 * 1024, 1024 * 1024];
/// A by-pid snapshot older than this may belong to a dead claude (pid reuse).
const BYPID_FRESH: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
pub struct CtxEntry {
    pub pct: f32,
    pub exact: bool,
    pub source_ts: SystemTime,
}

pub struct CtxUpdate {
    pub sid: String,
    pub pct: f32,
    pub exact: bool,
    pub source_ts: SystemTime,
}

pub enum CtxMsg {
    Update(CtxUpdate),
    /// A by-pid snapshot revealed the tab runs a different session now
    /// (/resume inside claude).
    Rebind { session: u64, update: CtxUpdate },
}

/// Single source of truth for badges. Owned by the UI, fed only via mpsc.
#[derive(Default)]
pub struct CtxIndex {
    map: HashMap<String, CtxEntry>,
}

impl CtxIndex {
    pub fn get(&self, sid: &str) -> Option<CtxEntry> {
        self.map.get(sid).copied()
    }

    /// Merge by SOURCE time, never read time: a late background scan must not
    /// overwrite fresher data. Tie (±2s) prefers exact. pct<=0 = no data.
    pub fn apply(&mut self, u: CtxUpdate) {
        if u.pct <= 0.0 || !u.pct.is_finite() {
            return;
        }
        let entry = CtxEntry { pct: u.pct.min(100.0), exact: u.exact, source_ts: u.source_ts };
        let replace = match self.map.get(&u.sid) {
            None => true,
            Some(cur) => {
                let diff = entry
                    .source_ts
                    .duration_since(cur.source_ts)
                    .unwrap_or_else(|e| e.duration());
                if diff <= Duration::from_secs(2) {
                    if entry.exact != cur.exact {
                        entry.exact
                    } else {
                        entry.source_ts >= cur.source_ts
                    }
                } else {
                    entry.source_ts > cur.source_ts
                }
            },
        };
        if replace {
            self.map.insert(u.sid, entry);
        }
    }
}

/// Per-session poller bookkeeping (stat results between ticks). Not persisted.
#[derive(Default)]
pub struct CtxStat {
    pub snap_mtime: Option<SystemTime>,
    pub bypid_mtime: Option<SystemTime>,
    pub jsonl_state: Option<(SystemTime, u64)>,
    pub jsonl_path: Option<PathBuf>,
    /// sid `jsonl_path` was resolved for.
    pub path_sid: Option<String>,
    pub last_resolve: Option<Instant>,
    /// mtime of claude's own ~/.claude/sessions/<pid>.json metadata.
    pub meta_mtime: Option<SystemTime>,
    /// Throttle for the claude-behind-wrapper finder.
    pub last_finder: Option<Instant>,
}

// ---- sid -> jsonl map ----

#[derive(Default)]
pub struct JsonlMap {
    pub map: HashMap<String, PathBuf>,
    pub last_scan: Option<Instant>,
}

pub type SharedMap = Arc<Mutex<JsonlMap>>;

/// Walk project dirs collecting jsonl paths by file stem (names only, no
/// content reads). On a duplicate sid the newer file wins.
fn scan_projects_in(root: &Path) -> HashMap<String, PathBuf> {
    let mut out: HashMap<String, (SystemTime, PathBuf)> = HashMap::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for proj in rd.flatten() {
            let Ok(files) = std::fs::read_dir(proj.path()) else { continue };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()).filter(|s| valid_sid(s))
                else {
                    continue;
                };
                let mtime = f
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                match out.get(stem) {
                    Some((t, _)) if *t >= mtime => {},
                    _ => {
                        out.insert(stem.to_string(), (mtime, p));
                    },
                }
            }
        }
    }
    out.into_iter().map(|(k, (_, p))| (k, p)).collect()
}

fn scan_projects() -> HashMap<String, PathBuf> {
    match claude_dir() {
        Some(d) => scan_projects_in(&d.join("projects")),
        None => HashMap::new(),
    }
}

pub fn spawn_initial_scan(map: SharedMap) {
    std::thread::spawn(move || {
        let scanned = scan_projects();
        if let Ok(mut m) = map.lock() {
            for (k, v) in scanned {
                m.map.entry(k).or_insert(v);
            }
            m.last_scan = Some(Instant::now());
        }
    });
}

/// Background-thread path resolution: map hit, then the cwd candidate (new
/// bindings land here instantly), then a full rescan throttled to 1/30s.
pub fn resolve_jsonl(map: &SharedMap, sid: &str, cwd_hint: Option<&Path>) -> Option<PathBuf> {
    if let Ok(m) = map.lock() {
        if let Some(p) = m.map.get(sid) {
            if p.exists() {
                return Some(p.clone());
            }
        }
    }
    if let Some(cwd) = cwd_hint {
        if let Some(dir) = project_dir(cwd) {
            let cand = dir.join(format!("{sid}.jsonl"));
            if cand.exists() {
                if let Ok(mut m) = map.lock() {
                    m.map.insert(sid.to_string(), cand.clone());
                }
                return Some(cand);
            }
        }
    }
    let due = {
        let mut m = map.lock().ok()?;
        if m.last_scan.is_none_or(|t| t.elapsed() >= Duration::from_secs(30)) {
            m.last_scan = Some(Instant::now());
            true
        } else {
            false
        }
    };
    if due {
        // Scan without holding the lock; the UI try_locks this map every tick.
        let scanned = scan_projects();
        let mut m = map.lock().ok()?;
        for (k, v) in scanned {
            m.map.insert(k, v);
        }
        return m.map.get(sid).cloned();
    }
    None
}

// ---- hook snapshots ----

fn ctx_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".kip").join("ctx"))
}

pub fn by_sid_path(sid: &str) -> Option<PathBuf> {
    ctx_dir().map(|d| d.join("by-sid").join(format!("{sid}.json")))
}

pub fn by_pid_path(pid: i32) -> Option<PathBuf> {
    ctx_dir().map(|d| d.join("by-pid").join(format!("{pid}.json")))
}

fn parse_snapshot(text: &str) -> Option<CtxUpdate> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let sid = v["session_id"].as_str().filter(|s| valid_sid(s))?;
    let pct = v["ctx"].as_f64().filter(|p| *p > 0.0)? as f32;
    let ts = v["ts"].as_u64()?;
    Some(CtxUpdate {
        sid: sid.to_string(),
        pct: pct.min(100.0),
        exact: true,
        source_ts: SystemTime::UNIX_EPOCH + Duration::from_secs(ts),
    })
}

fn read_snapshot(path: &Path) -> Option<CtxUpdate> {
    if std::fs::metadata(path).ok()?.len() > SNAPSHOT_MAX {
        return None;
    }
    parse_snapshot(&std::fs::read_to_string(path).ok()?)
}

// ---- jsonl estimate ----

/// "2026-07-23T12:38:25.388Z" -> SystemTime (days-from-civil, no deps).
fn parse_iso_ts(s: &str) -> Option<SystemTime> {
    if s.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + sec;
    (secs > 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

fn model_window(model: &str, pref_1m: bool) -> u64 {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") || m.contains("[1m]") || pref_1m {
        1_000_000
    } else {
        200_000
    }
}

/// The user's `"model"` preference in claude settings carries the [1m] window
/// choice that the API model id does not.
fn settings_model_is_1m() -> bool {
    let Some(p) = claude_dir().map(|d| d.join("settings.json")) else { return false };
    let Ok(text) = std::fs::read_to_string(p) else { return false };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["model"].as_str().map(|m| m.contains("[1m]")))
        .unwrap_or(false)
}

/// Reverse-scan transcript text for the last main-chain API usage. Returns
/// (pct, source time of that usage entry). A torn last line fails the JSON
/// parse and is skipped; sidechain (subagent) usage is not the main context.
fn scan_usage(text: &str, pref_1m: bool) -> Option<(f32, Option<SystemTime>)> {
    for line in text.lines().rev() {
        if !line.contains("\"usage\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        let u = &v["message"]["usage"];
        let Some(inp) = u["input_tokens"].as_u64() else { continue };
        let used = inp
            + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
            + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        if used == 0 {
            continue;
        }
        let mut window = model_window(v["message"]["model"].as_str().unwrap_or(""), pref_1m);
        // Self-calibration: more tokens than the assumed window = wrong assumption.
        if used > window {
            window = 1_000_000;
        }
        let pct = (used as f64 / window as f64 * 100.0).clamp(1.0, 100.0) as f32;
        let ts = v["timestamp"].as_str().and_then(parse_iso_ts);
        return Some((pct, ts));
    }
    None
}

/// Estimate from a transcript. source_ts is the usage entry's own timestamp
/// (NOT file mtime - appended user messages must not outrank an exact hook
/// snapshot), falling back to mtime for malformed timestamps.
pub fn estimate_from_jsonl(path: &Path, sid: &str) -> Option<CtxUpdate> {
    let pref_1m = settings_model_is_1m();
    let mut f = std::fs::File::open(path).ok()?;
    let meta = f.metadata().ok()?;
    let len = meta.len();
    let mtime = meta.modified().ok();
    for tail in TAILS {
        let start = len.saturating_sub(tail);
        if f.seek(SeekFrom::Start(start)).is_err() {
            return None;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return None;
        }
        // A cut mid-UTF8 first line becomes replacement chars and fails JSON parse.
        let text = String::from_utf8_lossy(&buf);
        if let Some((pct, ts)) = scan_usage(&text, pref_1m) {
            return Some(CtxUpdate {
                sid: sid.to_string(),
                pct,
                exact: false,
                source_ts: ts.or(mtime).unwrap_or_else(SystemTime::now),
            });
        }
        if start == 0 {
            break;
        }
    }
    None
}

// ---- async entry points (all spawn a thread, send CtxMsg, request repaint) ----

/// Full lookup for a known sid: BOTH the hook snapshot and the jsonl estimate
/// go to the channel - merge by source_ts picks the truth, so a stale snapshot
/// (session continued elsewhere / compacted) loses to a fresher transcript.
pub fn lookup(
    sid: String,
    cwd: PathBuf,
    map: SharedMap,
    tx: Sender<CtxMsg>,
    egui: egui::Context,
) {
    std::thread::spawn(move || {
        let mut sent = false;
        if let Some(u) = by_sid_path(&sid).as_deref().and_then(read_snapshot) {
            if u.sid == sid {
                sent |= tx.send(CtxMsg::Update(u)).is_ok();
            }
        }
        if let Some(path) = resolve_jsonl(&map, &sid, Some(&cwd)) {
            if let Some(u) = estimate_from_jsonl(&path, &sid) {
                sent |= tx.send(CtxMsg::Update(u)).is_ok();
            }
        }
        if sent {
            egui.request_repaint();
        }
    });
}

pub fn spawn_sid_read(sid: String, tx: Sender<CtxMsg>, egui: egui::Context) {
    std::thread::spawn(move || {
        let Some(u) = by_sid_path(&sid).as_deref().and_then(read_snapshot) else { return };
        if u.sid == sid && tx.send(CtxMsg::Update(u)).is_ok() {
            egui.request_repaint();
        }
    });
}

/// by-pid snapshot: the tab's own claude wrote it, so it both carries the
/// exact % (two claudes on one sid diverge - by-sid is last-writer-wins,
/// by-pid is per-process) and reveals in-claude /resume rebinding.
pub fn spawn_bypid_read(
    session: u64,
    pid: i32,
    cur_sid: Option<String>,
    tx: Sender<CtxMsg>,
    egui: egui::Context,
) {
    std::thread::spawn(move || {
        let Some(u) = by_pid_path(pid).as_deref().and_then(read_snapshot) else { return };
        // Stale file = possibly a dead claude's pid, reused. Never rebind on it.
        let fresh = SystemTime::now()
            .duration_since(u.source_ts)
            .is_ok_and(|d| d <= BYPID_FRESH);
        if !fresh {
            return;
        }
        let msg = if cur_sid.as_deref() != Some(u.sid.as_str()) {
            CtxMsg::Rebind { session, update: u }
        } else {
            CtxMsg::Update(u)
        };
        if tx.send(msg).is_ok() {
            egui.request_repaint();
        }
    });
}

pub fn spawn_estimate(sid: String, path: PathBuf, tx: Sender<CtxMsg>, egui: egui::Context) {
    std::thread::spawn(move || {
        let Some(u) = estimate_from_jsonl(&path, &sid) else { return };
        if tx.send(CtxMsg::Update(u)).is_ok() {
            egui.request_repaint();
        }
    });
}

/// Startup GC of the snapshot dirs: by-pid files outlive their claudes within
/// a day, by-sid keeps a month of history, in.* are stdin buffers of killed hooks.
pub fn sweep() {
    std::thread::spawn(|| {
        let Some(dir) = ctx_dir() else { return };
        let clean = |sub: &str, max_age: Duration, prefix: Option<&str>| {
            let Ok(rd) = std::fs::read_dir(if sub.is_empty() { dir.clone() } else { dir.join(sub) })
            else {
                return;
            };
            for e in rd.flatten() {
                if let Some(pref) = prefix {
                    if !e.file_name().to_string_lossy().starts_with(pref) {
                        continue;
                    }
                }
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|d| d > max_age));
                if old && e.file_type().is_ok_and(|t| t.is_file()) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        };
        clean("by-pid", Duration::from_secs(24 * 3600), None);
        clean("by-sid", Duration::from_secs(30 * 24 * 3600), None);
        clean("", Duration::from_secs(3600), Some("in."));
    });
}

// ---- hook install / uninstall ----

fn claude_settings_path() -> Option<PathBuf> {
    claude_dir().map(|d| d.join("settings.json"))
}

/// $HOME stays literal in settings.json so the entry survives dotfiles sync
/// to another machine/user.
const HOOK_CMD: &str = "\"$HOME\"/.kip/bin/kip-ctx-hook.sh";

fn is_ours(cmd: &str) -> bool {
    cmd.contains(".kip/bin/kip-ctx-hook.sh")
}

fn build_cmd(prev_cmd: Option<&str>) -> String {
    match prev_cmd {
        Some(p) if !p.trim().is_empty() && !is_ours(p) => {
            format!("{HOOK_CMD} '{}'", p.replace('\'', "'\\''"))
        },
        _ => HOOK_CMD.to_string(),
    }
}

/// Wrap statusLine in a settings root, touching nothing else. Returns the
/// serialized previous value ("" if the key was absent), None if already ours.
fn wrap_root(root: &mut serde_json::Value) -> Option<String> {
    let cur = root.get("statusLine").cloned();
    let cur_cmd = cur.as_ref().and_then(|v| v["command"].as_str()).map(String::from);
    if cur_cmd.as_deref().is_some_and(is_ours) {
        return None;
    }
    let prev_ser = cur.as_ref().map(|v| v.to_string()).unwrap_or_default();
    let cmd = build_cmd(cur_cmd.as_deref());
    root["statusLine"] = serde_json::json!({ "type": "command", "command": cmd });
    Some(prev_ser)
}

/// Restore statusLine ONLY if it is still our hook - a manually changed
/// command must never be clobbered by a rollback. Returns whether it wrote.
fn restore_root(root: &mut serde_json::Value, prev_ser: &str) -> bool {
    let cur_ours =
        root.get("statusLine").and_then(|v| v["command"].as_str()).is_some_and(is_ours);
    if !cur_ours {
        return false;
    }
    match serde_json::from_str::<serde_json::Value>(prev_ser) {
        Ok(prev) if !prev_ser.is_empty() => root["statusLine"] = prev,
        _ => {
            if let Some(o) = root.as_object_mut() {
                o.remove("statusLine");
            }
        },
    }
    true
}

fn read_settings_root(path: &Path) -> Result<serde_json::Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|_| "settings.json Claude не парсится - не трогаю его".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(format!("чтение settings.json: {e}")),
    }
}

fn write_settings_root(path: &Path, root: &serde_json::Value) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let json = serde_json::to_vec_pretty(root).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.kip-tmp");
    std::fs::write(&tmp, json).map_err(|e| format!("запись settings.json: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("замена settings.json: {e}"))
}

/// Copy the embedded script to ~/.kip/bin (the .app path is not stable -
/// moves, updates, duplicates; this one is). Idempotent, run on every start
/// while the hook is enabled so updates propagate.
pub fn write_hook_script() -> Result<PathBuf, String> {
    let home = dirs::home_dir().ok_or("нет домашней директории")?;
    let bin = home.join(".kip").join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| format!("mkdir {}: {e}", bin.display()))?;
    let p = bin.join("kip-ctx-hook.sh");
    std::fs::write(&p, HOOK_SCRIPT).map_err(|e| format!("запись хука: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755));
    }
    Ok(p)
}

/// The wrapped command carries the previous one as its quoted argument -
/// recover it when kip's own state was lost (fresh machine, reset state.json)
/// so a later rollback still restores the user's statusline.
fn recover_prev(cmd: &str) -> String {
    let Some(rest) = cmd.strip_prefix(HOOK_CMD).map(str::trim) else { return String::new() };
    if rest.len() < 2 || !rest.starts_with('\'') || !rest.ends_with('\'') {
        return String::new();
    }
    let inner = rest[1..rest.len() - 1].replace("'\\''", "'");
    serde_json::json!({ "type": "command", "command": inner }).to_string()
}

pub fn install_hook(settings: &mut Settings) -> Result<(), String> {
    write_hook_script()?;
    let path = claude_settings_path().ok_or("нет домашней директории")?;
    let mut root = read_settings_root(&path)?;
    if let Some(prev) = wrap_root(&mut root) {
        settings.prev_statusline = Some(prev);
        write_settings_root(&path, &root)?;
    } else if settings.prev_statusline.is_none() {
        // Already wrapped, but this kip never saw the original.
        let cmd = root["statusLine"]["command"].as_str().unwrap_or_default();
        settings.prev_statusline = Some(recover_prev(cmd));
    }
    Ok(())
}

pub fn uninstall_hook(settings: &mut Settings) -> Result<(), String> {
    let path = claude_settings_path().ok_or("нет домашней директории")?;
    let mut root = read_settings_root(&path)?;
    let prev = settings.prev_statusline.clone().unwrap_or_default();
    if restore_root(&mut root, &prev) {
        write_settings_root(&path, &root)?;
    }
    settings.prev_statusline = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn ts(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn upd(sid: &str, pct: f32, exact: bool, t: u64) -> CtxUpdate {
        CtxUpdate { sid: sid.into(), pct, exact, source_ts: ts(t) }
    }

    fn tdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("kip-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    fn usage_line(used_input: u64, cache: u64, model: &str, iso: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{iso}\",\"message\":{{\"model\":\"{model}\",\"usage\":{{\"input_tokens\":{used_input},\"cache_read_input_tokens\":{cache},\"cache_creation_input_tokens\":0,\"output_tokens\":10}}}}}}"
        )
    }

    // -- merge --

    #[test]
    fn merge_newer_wins() {
        let mut idx = CtxIndex::default();
        idx.apply(upd(SID, 50.0, false, 1000));
        idx.apply(upd(SID, 60.0, false, 2000));
        assert_eq!(idx.get(SID).unwrap().pct, 60.0);
        // Late background result with an older source must lose.
        idx.apply(upd(SID, 10.0, false, 1500));
        assert_eq!(idx.get(SID).unwrap().pct, 60.0);
    }

    #[test]
    fn merge_tie_prefers_exact() {
        let mut idx = CtxIndex::default();
        idx.apply(upd(SID, 40.0, true, 1000));
        idx.apply(upd(SID, 90.0, false, 1001));
        let e = idx.get(SID).unwrap();
        assert!(e.exact);
        assert_eq!(e.pct, 40.0);
    }

    #[test]
    fn merge_zero_and_nan_ignored() {
        let mut idx = CtxIndex::default();
        idx.apply(upd(SID, 0.0, true, 1000));
        idx.apply(upd(SID, f32::NAN, true, 1000));
        assert!(idx.get(SID).is_none());
    }

    #[test]
    fn merge_stale_snapshot_loses_to_fresh_estimate() {
        // Session compacted/continued elsewhere: old exact 90%, fresh estimate 15%.
        let mut idx = CtxIndex::default();
        idx.apply(upd(SID, 90.0, true, 1000));
        idx.apply(upd(SID, 15.0, false, 5000));
        assert_eq!(idx.get(SID).unwrap().pct, 15.0);
    }

    #[test]
    fn merge_old_estimate_keeps_exact() {
        // A user message re-triggers an estimate whose usage entry is OLDER
        // than the hook snapshot - exact must survive.
        let mut idx = CtxIndex::default();
        idx.apply(upd(SID, 40.0, true, 5000));
        idx.apply(upd(SID, 90.0, false, 4000));
        let e = idx.get(SID).unwrap();
        assert!(e.exact && e.pct == 40.0);
    }

    // -- window heuristic --

    #[test]
    fn window_model_families() {
        assert_eq!(model_window("claude-fable-5", false), 1_000_000);
        assert_eq!(model_window("claude-mythos-5", false), 1_000_000);
        assert_eq!(model_window("claude-opus-4-8", false), 200_000);
        assert_eq!(model_window("claude-opus-4-8", true), 1_000_000);
    }

    #[test]
    fn window_self_calibration() {
        // 500k tokens on an "opus 200k" line: false-red regression guard.
        let line = usage_line(1000, 499_000, "claude-opus-4-8", "2026-07-23T10:00:00.000Z");
        let (pct, _) = scan_usage(&line, false).unwrap();
        assert!((pct - 50.0).abs() < 0.1, "pct={pct}");
    }

    // -- jsonl scan --

    #[test]
    fn scan_torn_last_line_skipped() {
        let good = usage_line(1000, 150_000, "claude-fable-5", "2026-07-23T10:00:00.000Z");
        let torn = &good[..good.len() / 2];
        let text = format!("{good}\n{torn}");
        let (pct, ts_) = scan_usage(&text, false).unwrap();
        assert!((pct - 15.1).abs() < 0.2, "pct={pct}");
        assert!(ts_.is_some());
    }

    #[test]
    fn scan_sidechain_and_no_usage_skipped() {
        let side = format!(
            "{{\"isSidechain\":true,\"message\":{{\"usage\":{{\"input_tokens\":5,\"cache_read_input_tokens\":900000}}}}}}"
        );
        let main = usage_line(1000, 99_000, "claude-fable-5", "2026-07-23T10:00:00.000Z");
        let text = format!("{main}\n{side}");
        let (pct, _) = scan_usage(&text, false).unwrap();
        assert!((pct - 10.0).abs() < 0.1, "pct={pct}");
        assert!(scan_usage("{\"type\":\"user\"}\nnot json\n", false).is_none());
    }

    #[test]
    fn estimate_progressive_tail_past_giant_line() {
        // usage entry, then one 100KB line: the 64KB tail sees no usage,
        // the 1MB pass must find it.
        let dir = tdir("progressive");
        let path = dir.join(format!("{SID}.jsonl"));
        let good = usage_line(1000, 150_000, "claude-fable-5", "2026-07-23T10:00:00.000Z");
        let giant = format!("{{\"type\":\"user\",\"blob\":\"{}\"}}", "x".repeat(100 * 1024));
        std::fs::write(&path, format!("{good}\n{giant}\n")).unwrap();
        let u = estimate_from_jsonl(&path, SID).expect("progressive tail must find usage");
        assert!(!u.exact && (u.pct - 15.1).abs() < 0.2);
    }

    #[test]
    fn estimate_utf8_boundary_tail() {
        // The 64KB cut lands mid-multibyte char; lossy decode must not panic
        // and the scan still finds the trailing usage line.
        let dir = tdir("utf8");
        let path = dir.join(format!("{SID}.jsonl"));
        let filler = format!("{{\"t\":\"{}\"}}", "я".repeat(40 * 1024));
        let good = usage_line(1000, 150_000, "claude-fable-5", "2026-07-23T10:00:00.000Z");
        std::fs::write(&path, format!("{filler}\n{good}\n")).unwrap();
        let u = estimate_from_jsonl(&path, SID).unwrap();
        assert!((u.pct - 15.1).abs() < 0.2);
    }

    #[test]
    fn estimate_missing_file_none() {
        assert!(estimate_from_jsonl(Path::new("/nonexistent/x.jsonl"), SID).is_none());
    }

    // -- snapshots --

    #[test]
    fn snapshot_parse_and_rejects() {
        let u = parse_snapshot(&format!(
            "{{\"session_id\":\"{SID}\",\"ctx\":42.5,\"ts\":1700000000}}"
        ))
        .unwrap();
        assert!(u.exact && (u.pct - 42.5).abs() < 0.01);
        assert_eq!(u.source_ts, ts(1_700_000_000));
        assert!(parse_snapshot("not json").is_none());
        assert!(parse_snapshot(&format!("{{\"session_id\":\"{SID}\",\"ctx\":0,\"ts\":1}}")).is_none());
        assert!(parse_snapshot("{\"session_id\":\"../evil\",\"ctx\":5,\"ts\":1}").is_none());
    }

    #[test]
    fn snapshot_oversized_skipped() {
        let dir = tdir("oversize");
        let p = dir.join("s.json");
        std::fs::write(&p, " ".repeat((SNAPSHOT_MAX + 1) as usize)).unwrap();
        assert!(read_snapshot(&p).is_none());
    }

    // -- iso timestamps --

    #[test]
    fn iso_ts_roundtrip() {
        // 2026-07-23T12:38:25Z = 1784810305 (verified against date and python).
        let t = parse_iso_ts("2026-07-23T12:38:25.388Z").unwrap();
        assert_eq!(t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(), 1_784_810_305);
        assert!(parse_iso_ts("garbage").is_none());
        assert!(parse_iso_ts("2026-99-99T00:00:00Z").is_none());
    }

    // -- sid -> jsonl map (cross-project regression: "5 seconds" bug) --

    #[test]
    fn map_finds_session_of_foreign_project() {
        let root = tdir("projects");
        let proj = root.join("-Users-x-other-project");
        std::fs::create_dir_all(&proj).unwrap();
        let path = proj.join(format!("{SID}.jsonl"));
        std::fs::write(&path, usage_line(1000, 99_000, "claude-fable-5", "2026-07-23T10:00:00.000Z")).unwrap();
        let scanned = scan_projects_in(&root);
        assert_eq!(scanned.get(SID), Some(&path));
    }

    #[test]
    fn map_rescan_throttled() {
        let map: SharedMap = Default::default();
        map.lock().unwrap().last_scan = Some(Instant::now());
        // Fresh scan mark + unknown sid + no cwd hint -> throttled, no result.
        assert!(resolve_jsonl(&map, SID, None).is_none());
        // Manual insert = "new binding adds the path immediately".
        map.lock().unwrap().map.insert(SID.into(), PathBuf::from("/tmp/x.jsonl"));
        assert!(map.lock().unwrap().map.contains_key(SID));
    }

    // -- settings wrap / restore --

    #[test]
    fn wrap_preserves_other_keys_and_chains_prev() {
        let mut root: serde_json::Value = serde_json::from_str(
            "{\"model\":\"opus[1m]\",\"statusLine\":{\"type\":\"command\",\"command\":\"node /x/heartbeat.js\"}}",
        )
        .unwrap();
        let prev = wrap_root(&mut root).unwrap();
        assert!(prev.contains("heartbeat.js"));
        assert_eq!(root["model"], "opus[1m]");
        let cmd = root["statusLine"]["command"].as_str().unwrap();
        assert!(cmd.starts_with(HOOK_CMD) && cmd.contains("'node /x/heartbeat.js'"));
        // Second install: already ours, no double wrap.
        assert!(wrap_root(&mut root).is_none());
    }

    #[test]
    fn wrap_absent_statusline() {
        let mut root = serde_json::json!({});
        let prev = wrap_root(&mut root).unwrap();
        assert_eq!(prev, "");
        assert_eq!(root["statusLine"]["command"].as_str().unwrap(), HOOK_CMD);
    }

    #[test]
    fn restore_foreign_statusline_noop() {
        let mut root = serde_json::json!({"statusLine": {"type":"command","command":"my-own-thing"}});
        assert!(!restore_root(&mut root, "{\"type\":\"command\",\"command\":\"old\"}"));
        assert_eq!(root["statusLine"]["command"], "my-own-thing");
    }

    #[test]
    fn restore_ours_brings_back_prev_or_removes() {
        let mut root = serde_json::json!({"statusLine": {"type":"command","command": HOOK_CMD}});
        assert!(restore_root(&mut root, "{\"type\":\"command\",\"command\":\"node /x/heartbeat.js\"}"));
        assert_eq!(root["statusLine"]["command"], "node /x/heartbeat.js");
        let mut root = serde_json::json!({"statusLine": {"type":"command","command": HOOK_CMD}});
        assert!(restore_root(&mut root, ""));
        assert!(root.get("statusLine").is_none());
    }

    #[test]
    fn build_cmd_escapes_quotes() {
        let cmd = build_cmd(Some("echo 'hi'"));
        assert_eq!(cmd, format!("{HOOK_CMD} 'echo '\\''hi'\\'''"));
    }

    #[test]
    fn recover_prev_roundtrip() {
        // build_cmd -> recover_prev must give back the original command,
        // including embedded quotes.
        for orig in ["node /x/heartbeat.js", "echo 'hi'"] {
            let ser = recover_prev(&build_cmd(Some(orig)));
            let v: serde_json::Value = serde_json::from_str(&ser).unwrap();
            assert_eq!(v["command"], orig);
        }
        assert_eq!(recover_prev(HOOK_CMD), "");
        assert_eq!(recover_prev("something-else"), "");
    }

    // -- hook script smoke tests (real /bin/sh, unix only) --

    #[cfg(unix)]
    fn run_hook(home: &Path, stdin: &str, arg: Option<&str>) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("resources/kip-ctx-hook.sh");
        let mut cmd = Command::new("/bin/sh");
        cmd.arg(&script);
        if let Some(a) = arg {
            cmd.arg(a);
        }
        let mut child = cmd
            .env("HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[cfg(unix)]    #[test]
    fn hook_writes_snapshots() {
        let home = tdir("hook-ok");
        let stdin = format!(
            "{{\"session_id\":\"{SID}\",\"model\":{{\"id\":\"claude-fable-5\"}},\"context_window\":{{\"used_percentage\":37.5}}}}"
        );
        run_hook(&home, &stdin, None);
        let by_sid = home.join(".kip/ctx/by-sid").join(format!("{SID}.json"));
        let text = std::fs::read_to_string(by_sid).expect("by-sid snapshot written");
        let u = parse_snapshot(&text).unwrap();
        assert_eq!(u.sid, SID);
        assert!((u.pct - 37.5).abs() < 0.01);
        // by-pid keyed by the hook's parent (this test process).
        let by_pid = home.join(".kip/ctx/by-pid");
        assert!(std::fs::read_dir(by_pid).unwrap().flatten().count() >= 1);
    }

    #[cfg(unix)]    #[test]
    fn hook_empty_or_zero_writes_nothing() {
        let home = tdir("hook-empty");
        run_hook(&home, "", None);
        run_hook(&home, "{\"no_fields\":true}", None);
        run_hook(
            &home,
            &format!("{{\"session_id\":\"{SID}\",\"context_window\":{{\"used_percentage\":0}}}}"),
            None,
        );
        let by_sid = home.join(".kip/ctx/by-sid");
        let n = std::fs::read_dir(by_sid).map(|r| r.flatten().count()).unwrap_or(0);
        assert_eq!(n, 0);
    }

    #[cfg(unix)]    #[test]
    fn hook_chains_prev_with_full_stdin() {
        let home = tdir("hook-chain");
        let out = home.join("prev-out.txt");
        let stdin = format!(
            "{{\"session_id\":\"{SID}\",\"context_window\":{{\"used_percentage\":12}}}}"
        );
        run_hook(&home, &stdin, Some(&format!("cat > '{}'", out.display())));
        let echoed = std::fs::read_to_string(&out).expect("prev command ran");
        assert_eq!(echoed, stdin, "prev must receive the exact stdin");
        // No stdin-buffer litter left behind.
        let leftovers = std::fs::read_dir(home.join(".kip/ctx"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("in."))
            .count();
        assert_eq!(leftovers, 0);
    }
}
