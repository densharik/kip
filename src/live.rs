//! Live runs of a Claude session: what its Bash calls print while they run and
//! which subagents are working, so a long turn is not a blank wait.
//!
//! The `live-runs` Claude Code mod tees every foreground Bash call into
//! `~/.kip/live/<sid>/NNNN.log` and keeps `~/.kip/live/<sid>/index.json` with
//! the runs and subagents. A background thread polls those files for the
//! sessions on screen and sends `LiveMsg` over mpsc; the UI only draws.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

const POLL: Duration = Duration::from_millis(400);
const TAIL_BYTES: u64 = 8 * 1024;
pub const TAIL_LINES: usize = 14;
const INDEX_MAX: u64 = 1024 * 1024;

#[derive(Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    pub label: String,
    pub command: String,
    pub log: String,
    pub started_at: u64,
    #[serde(default)]
    pub ended_at: Option<u64>,
    pub status: String,
}

#[derive(Deserialize, Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub started_at: u64,
    #[serde(default)]
    pub ended_at: Option<u64>,
    pub status: String,
    #[serde(default)]
    pub last_tool: Option<String>,
}

#[derive(Deserialize, Default)]
struct Index {
    #[serde(default)]
    runs: Vec<Run>,
    #[serde(default)]
    agents: Vec<Agent>,
}

#[derive(Clone, PartialEq, Default, Debug)]
pub struct LiveView {
    pub runs: Vec<Run>,
    pub agents: Vec<Agent>,
    /// Last lines of each running run's log, by run id.
    pub tails: HashMap<String, Vec<String>>,
}

impl LiveView {
    pub fn running(&self) -> impl Iterator<Item = &Run> {
        self.runs.iter().filter(|r| r.status == "running")
    }

    pub fn agents_running(&self) -> impl Iterator<Item = &Agent> {
        self.agents.iter().filter(|a| a.status == "running")
    }

    pub fn is_busy(&self) -> bool {
        self.running().next().is_some() || self.agents_running().next().is_some()
    }

    /// When the last run or agent ended, in epoch ms.
    pub fn last_end(&self) -> Option<u64> {
        let runs = self.runs.iter().filter_map(|r| r.ended_at);
        let agents = self.agents.iter().filter_map(|a| a.ended_at);
        runs.chain(agents).max()
    }
}

pub struct LiveMsg {
    pub sid: String,
    pub view: LiveView,
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn live_dir(sid: &str) -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".kip").join("live").join(sid))
}

/// Strip CSI/OSC escapes and keep the text after the last carriage return,
/// the way a terminal would show a progress line.
fn clean(line: &str) -> String {
    let line = line.rsplit('\r').find(|p| !p.is_empty()).unwrap_or("");
    let mut out = String::with_capacity(line.len());
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            match it.peek() {
                Some('[') => {
                    it.next();
                    while let Some(&n) = it.peek() {
                        it.next();
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                },
                Some(']') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' && it.peek() == Some(&'\\') {
                            it.next();
                            break;
                        }
                    }
                },
                _ => {
                    it.next();
                },
            }
        } else if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn tail(path: &str) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.split('\n').collect();
    // A cut in the middle of a line: drop the partial first one.
    if from > 0 && lines.len() > 1 {
        lines.remove(0);
    }
    let lines: Vec<String> = lines.iter().map(|l| clean(l)).filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].to_vec()
}

fn read_index(sid: &str) -> Option<(Index, SystemTime)> {
    let path = live_dir(sid)?.join("index.json");
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > INDEX_MAX {
        return None;
    }
    let mtime = meta.modified().ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    Some((serde_json::from_str(&text).ok()?, mtime))
}

/// Starts the poller. `watch` holds the Claude session ids on screen; the
/// thread reads only those and sends a view whenever it changed.
pub fn start(watch: Arc<Mutex<Vec<String>>>, tx: Sender<LiveMsg>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let mut seen: HashMap<String, (SystemTime, LiveView)> = HashMap::new();
        loop {
            std::thread::sleep(POLL);
            let sids = watch.lock().map(|w| w.clone()).unwrap_or_default();
            seen.retain(|sid, _| sids.contains(sid));
            let mut changed = false;
            for sid in sids {
                let prev = seen.get(&sid);
                let busy = prev.is_some_and(|(_, v)| v.is_busy());
                let mtime = live_dir(&sid)
                    .and_then(|d| std::fs::metadata(d.join("index.json")).ok())
                    .and_then(|m| m.modified().ok());
                let Some(mtime) = mtime else { continue };
                if !busy && prev.is_some_and(|(t, _)| *t == mtime) {
                    continue;
                }
                let Some((index, mtime)) = read_index(&sid) else { continue };
                let tails = index.runs.iter().filter(|r| r.status == "running").map(|r| (r.id.clone(), tail(&r.log))).collect();
                let view = LiveView { runs: index.runs, agents: index.agents, tails };
                if prev.is_some_and(|(_, v)| *v == view) {
                    seen.insert(sid, (mtime, view));
                    continue;
                }
                seen.insert(sid.clone(), (mtime, view.clone()));
                if tx.send(LiveMsg { sid, view }).is_err() {
                    return;
                }
                changed = true;
            }
            if changed {
                ctx.request_repaint();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_escapes_and_keeps_last_progress_frame() {
        assert_eq!(clean("\u{1b}[32mok\u{1b}[0m done"), "ok done");
        assert_eq!(clean("10%\r50%\r100%"), "100%");
        assert_eq!(clean("\u{1b}]0;title\u{7}text"), "text");
    }

    #[test]
    fn index_parses_mod_output() {
        let json = r#"{"sessionId":"s","cwd":"/w","updatedAt":1,
            "runs":[{"id":"t1","label":"Run tests","command":"npm test","log":"/x/0001.log","startedAt":5,"status":"running"}],
            "agents":[{"id":"a","agentId":"x","label":"Find","type":"Explore","startedAt":3,"status":"ok","endedAt":9}]}"#;
        let index: Index = serde_json::from_str(json).unwrap();
        assert_eq!(index.runs[0].command, "npm test");
        assert_eq!(index.agents[0].kind, "Explore");
        assert_eq!(index.agents[0].ended_at, Some(9));
    }
}
