//! What Claude Code mods show in kip. A mod running inside a claude session
//! writes `~/.kip/mods/<mod>/<sid>.json`; kip draws its badge in the session
//! row and its card over the terminal, with no kip code of its own:
//!
//! ```json
//! { "updatedAt": 1760000000000, "ttlMs": 5000,
//!   "badge": { "text": "VPN", "tone": "ok", "hint": "api.anthropic.com via NL 11" },
//!   "card":  { "title": "Running bash: 1", "tone": "busy",
//!              "rows": [{ "text": "npm test", "style": "strong" }, { "text": "ok 12", "style": "mono" }] } }
//! ```
//!
//! `ttlMs` drops a view the mod stopped refreshing (its claude died); without
//! it the view stays until the file changes. Tones: info, ok, warn, error,
//! busy, dim. Row styles: text, strong, mono, dim, faint. A background thread
//! polls the files for the sessions in the list and sends `ModsMsg` over mpsc;
//! the UI only draws.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

const POLL: Duration = Duration::from_millis(400);
const FILE_MAX: u64 = 256 * 1024;
const ROWS_MAX: usize = 60;

#[derive(Deserialize, Clone, Copy, PartialEq, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Ok,
    Warn,
    Error,
    Busy,
    Dim,
    #[default]
    #[serde(other)]
    Info,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Strong,
    Mono,
    Dim,
    Faint,
    #[default]
    #[serde(other)]
    Text,
}

#[derive(Deserialize, Clone, PartialEq, Debug)]
pub struct Badge {
    pub text: String,
    #[serde(default)]
    pub tone: Tone,
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Deserialize, Clone, PartialEq, Debug)]
pub struct Row {
    pub text: String,
    #[serde(default)]
    pub style: Style,
}

#[derive(Deserialize, Clone, PartialEq, Debug)]
pub struct Card {
    pub title: String,
    #[serde(default)]
    pub tone: Tone,
    #[serde(default)]
    pub rows: Vec<Row>,
}

#[derive(Deserialize, Clone, PartialEq, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ModView {
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub ttl_ms: Option<u64>,
    #[serde(default)]
    pub badge: Option<Badge>,
    #[serde(default)]
    pub card: Option<Card>,
}

impl ModView {
    /// When the view expires, in epoch ms; None = never.
    pub fn expires(&self) -> Option<u64> {
        self.ttl_ms.map(|t| self.updated_at.saturating_add(t))
    }

    pub fn alive(&self, now: u64) -> bool {
        self.expires().is_none_or(|t| now <= t)
    }
}

/// Every mod's view for one claude session, sorted by mod name.
pub struct ModsMsg {
    pub sid: String,
    pub views: Vec<(String, ModView)>,
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn mods_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".kip").join("mods"))
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

fn parse(text: &str) -> Option<ModView> {
    let mut view: ModView = serde_json::from_str(text).ok()?;
    if let Some(card) = &mut view.card {
        card.rows.truncate(ROWS_MAX);
        for r in &mut card.rows {
            r.text = clean(&r.text);
        }
        card.title = clean(&card.title);
    }
    if let Some(b) = &mut view.badge {
        b.text = clean(&b.text);
    }
    Some(view)
}

fn read_view(path: &PathBuf) -> Option<ModView> {
    if std::fs::metadata(path).ok()?.len() > FILE_MAX {
        return None;
    }
    parse(&std::fs::read_to_string(path).ok()?)
}

/// Starts the poller. `watch` holds the Claude session ids in the list; the
/// thread rereads a mod's file only when its mtime moved and sends a
/// session's views whenever they changed.
pub fn start(watch: Arc<Mutex<Vec<String>>>, tx: Sender<ModsMsg>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let mut seen: HashMap<(String, String), (SystemTime, ModView)> = HashMap::new();
        let mut sent: HashMap<String, Vec<(String, ModView)>> = HashMap::new();
        loop {
            std::thread::sleep(POLL);
            let sids = watch.lock().map(|w| w.clone()).unwrap_or_default();
            let Some(root) = mods_dir() else { continue };
            let mut mods: Vec<String> = std::fs::read_dir(&root)
                .map(|rd| {
                    rd.flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            mods.sort();
            seen.retain(|(_, sid), _| sids.contains(sid));
            sent.retain(|sid, _| sids.contains(sid));
            let mut changed = false;
            for sid in &sids {
                let mut views = Vec::new();
                for m in &mods {
                    let path = root.join(m).join(format!("{sid}.json"));
                    let key = (m.clone(), sid.clone());
                    let Some(mtime) = std::fs::metadata(&path).ok().and_then(|md| md.modified().ok()) else {
                        seen.remove(&key);
                        continue;
                    };
                    if seen.get(&key).is_none_or(|(t, _)| *t != mtime) {
                        // A half-written file keeps the last good view.
                        if let Some(v) = read_view(&path) {
                            seen.insert(key.clone(), (mtime, v));
                        }
                    }
                    if let Some((_, v)) = seen.get(&key) {
                        views.push((m.clone(), v.clone()));
                    }
                }
                if sent.get(sid) == Some(&views) {
                    continue;
                }
                sent.insert(sid.clone(), views.clone());
                if tx.send(ModsMsg { sid: sid.clone(), views }).is_err() {
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
    fn view_parses_and_tolerates_unknown_tones() {
        let json = r#"{"updatedAt":10,"ttlMs":5,
            "badge":{"text":"VPN","tone":"ok"},
            "card":{"title":"Running","tone":"sparkly","rows":[{"text":"\u001b[31mfail","style":"mono"},{"text":"x"}]}}"#;
        let v = parse(json).unwrap();
        assert_eq!(v.badge.as_ref().unwrap().tone, Tone::Ok);
        let card = v.card.as_ref().unwrap();
        assert_eq!(card.tone, Tone::Info);
        assert_eq!(card.rows[0].text, "fail");
        assert_eq!(card.rows[0].style, Style::Mono);
        assert_eq!(card.rows[1].style, Style::Text);
        assert!(v.alive(15));
        assert!(!v.alive(16));
        assert!(parse(r#"{"badge":{"text":"x"}}"#).unwrap().alive(u64::MAX));
    }
}
