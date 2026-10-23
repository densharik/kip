//! Past Claude Code conversations (~/.claude/projects/*/*.jsonl): an index
//! cached in ~/.kip/history.json, a full-text search over them, and the
//! message list for the preview.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
pub struct Entry {
    pub sid: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
    pub branch: String,
    /// Custom or AI title from the transcript.
    pub title: Option<String>,
    /// First thing the user typed.
    pub first: String,
    /// Seconds since the epoch.
    pub mtime: u64,
    pub size: u64,
}

impl Entry {
    pub fn name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.first)
    }
}

fn cache_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".kip").join("history-2.json"))
}

/// The user's or Claude's text in one transcript line. None for tool calls and
/// results, meta lines, subagents, and the tagged echoes of slash commands.
pub fn msg_text(v: &Value) -> Option<(bool, String)> {
    let user = match v["type"].as_str() {
        Some("user") => true,
        Some("assistant") => false,
        _ => return None,
    };
    if v["isMeta"].as_bool() == Some(true) || v["isSidechain"].as_bool() == Some(true) {
        return None;
    }
    // Injected reminders come as separate text parts next to what was typed.
    let keep = |t: &&str| !(user && is_tagged(t.trim_start()));
    let text = match &v["message"]["content"] {
        Value::String(t) => Some(t.as_str()).filter(keep).unwrap_or_default().to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .filter(keep)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some((user, text.to_string()))
}

/// `<command-name>`, `<local-command-stdout>`, `<system-reminder>`...
fn is_tagged(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else { return false };
    let tag: String = rest.chars().take_while(|c| *c != '>').collect();
    tag.len() < 40 && tag.contains('-') && tag.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Plain conversation text per session, kept next to the index so a search
/// reads ~1% of what the transcripts weigh (tool output and snapshots are the rest).
fn text_dir() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".kip").join("history-text"))
}

/// One pass over a transcript: cwd, branch, first prompt, title, and the
/// user/Claude text. Only lines that can hold one of those get parsed.
fn scan(path: &Path) -> (PathBuf, String, String, Option<String>, String) {
    let (mut cwd, mut branch, mut first, mut text) = (None, String::new(), String::new(), String::new());
    let (mut custom, mut ai) = (None, None);
    for_lines(path, |line| {
        let has = |pat: &[u8]| line.windows(pat.len()).any(|w| w == pat);
        let talk = has(b"\"type\":\"user\"") || has(b"\"type\":\"assistant\"");
        let titled = has(b"-title\"");
        if !talk && !titled && cwd.is_some() {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return };
        if cwd.is_none()
            && let Some(c) = v["cwd"].as_str() {
                cwd = Some(PathBuf::from(c));
                branch = v["gitBranch"].as_str().unwrap_or_default().to_string();
            }
        match v["type"].as_str() {
            Some("custom-title") => custom = v["customTitle"].as_str().map(String::from),
            Some("ai-title") => ai = v["aiTitle"].as_str().map(String::from),
            _ => {},
        }
        if let Some((user, t)) = msg_text(&v) {
            if user && first.is_empty() {
                first = t.lines().next().unwrap_or_default().chars().take(200).collect();
            }
            text.push_str(&t);
            text.push_str("\n\n");
        }
    });
    let title = custom.or(ai).filter(|t| !t.trim().is_empty());
    (cwd.unwrap_or_default(), branch, first, title, text)
}

fn read_entry(sid: String, path: PathBuf, mtime: u64, size: u64) -> Option<Entry> {
    let (cwd, branch, first, title, text) = scan(&path);
    // A session that never got a prompt is noise in the list.
    if first.is_empty() && title.is_none() {
        return None;
    }
    if let Some(dir) = text_dir() {
        let _ = std::fs::write(dir.join(format!("{sid}.txt")), text);
    }
    Some(Entry { sid, path, cwd, branch, title, first, mtime, size })
}

/// All past conversations, newest first. Only files changed since the cached
/// index are read again; the rest come from ~/.kip/history.json.
pub fn load_index() -> Vec<Entry> {
    let cached: HashMap<PathBuf, Entry> = cache_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Vec<Entry>>(&b).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let mut todo = Vec::new();
    let mut out = Vec::new();
    let texts = text_dir();
    if let Some(d) = &texts {
        let _ = std::fs::create_dir_all(d);
    }
    for (sid, path) in crate::ctx_index::scan_projects() {
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
        let size = meta.len();
        match cached.get(&path) {
            Some(e)
                if e.mtime == mtime
                    && e.size == size
                    && texts.as_ref().is_some_and(|d| d.join(format!("{sid}.txt")).exists()) =>
            {
                out.push(e.clone())
            },
            _ => todo.push((sid, path, mtime, size)),
        }
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let chunk = todo.len().div_ceil(threads).max(1);
    std::thread::scope(|sc| {
        let parts: Vec<_> = todo
            .chunks(chunk)
            .map(|part| {
                sc.spawn(move || {
                    part.iter()
                        .filter_map(|(s, p, m, z)| read_entry(s.clone(), p.clone(), *m, *z))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in parts {
            out.extend(h.join().unwrap_or_default());
        }
    });
    out.sort_by_key(|e| std::cmp::Reverse(e.mtime));
    // Text of transcripts that are gone.
    if let Some(d) = &texts {
        let keep: std::collections::HashSet<String> = out.iter().map(|e| format!("{}.txt", e.sid)).collect();
        for f in std::fs::read_dir(d).into_iter().flatten().flatten() {
            if !keep.contains(f.file_name().to_string_lossy().as_ref()) {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
    if let Some(p) = cache_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_vec(&out) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
    out
}

/// Case-insensitive literal matcher for `query`.
pub fn matcher(query: &str) -> Option<regex::Regex> {
    regex::RegexBuilder::new(&regex::escape(query)).case_insensitive(true).build().ok()
}

/// How many times the query occurs in a session's conversation text.
pub fn count_hits(sid: &str, re: &regex::Regex) -> usize {
    text_dir()
        .and_then(|d| std::fs::read_to_string(d.join(format!("{sid}.txt"))).ok())
        .map_or(0, |t| re.find_iter(&t).count())
}

/// Each line of a file, one at a time: transcripts run to tens of MB and the
/// search reads several at once, so they are never loaded whole.
fn for_lines(path: &Path, mut f: impl FnMut(&[u8])) {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else { return };
    let mut r = std::io::BufReader::with_capacity(256 * 1024, file);
    let mut line = Vec::new();
    while r.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        f(&line);
        line.clear();
    }
}

/// The conversation for the preview, oldest first, at most the last `max` messages.
pub fn messages(path: &Path, max: usize) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    for_lines(path, |l| {
        let talk = |pat: &[u8]| l.windows(pat.len()).any(|w| w == pat);
        if !talk(b"\"type\":\"user\"") && !talk(b"\"type\":\"assistant\"") {
            return;
        }
        if let Some(m) = serde_json::from_slice::<Value>(l).ok().and_then(|v| msg_text(&v)) {
            out.push(m);
        }
    });
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn msg_text_keeps_only_conversation() {
        let u = json!({"type": "user", "message": {"content": "привет"}});
        assert_eq!(msg_text(&u), Some((true, "привет".into())));
        let a = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "hi"}, {"type": "tool_use", "name": "Bash"}]}});
        assert_eq!(msg_text(&a), Some((false, "hi".into())));
        let tool = json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "x"}]}});
        assert_eq!(msg_text(&tool), None);
        let cmd = json!({"type": "user", "message": {"content": "<command-name>/clear</command-name>"}});
        assert_eq!(msg_text(&cmd), None);
        let mixed = json!({"type": "user", "message": {"content": [
            {"type": "text", "text": "<system-reminder>x</system-reminder>"}, {"type": "text", "text": "real"}]}});
        assert_eq!(msg_text(&mixed), Some((true, "real".into())));
        let html = json!({"type": "user", "message": {"content": "<div> fix this"}});
        assert!(msg_text(&html).is_some());
        let meta = json!({"type": "user", "isMeta": true, "message": {"content": "x"}});
        assert_eq!(msg_text(&meta), None);
    }

    #[test]
    fn scan_keeps_conversation_text_and_meta() {
        let dir = std::env::temp_dir().join(format!("kip-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.jsonl");
        let lines = [
            json!({"type": "user", "cwd": "/w", "gitBranch": "dev", "message": {"content": "Найди Деплой"}}),
            json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "деплой и \"use client\" C:\\work"}]}}),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "деплой"}]}}),
            json!({"type": "ai-title", "aiTitle": "Деплой"}),
        ];
        std::fs::write(&p, lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let (cwd, branch, first, title, text) = scan(&p);
        assert_eq!((cwd, branch.as_str(), first.as_str(), title.as_deref()), (PathBuf::from("/w"), "dev", "Найди Деплой", Some("Деплой")));
        assert_eq!(matcher("деплой").unwrap().find_iter(&text).count(), 2);
        assert_eq!(matcher("\"use client\"").unwrap().find_iter(&text).count(), 1);
        assert_eq!(matcher("c:\\work").unwrap().find_iter(&text).count(), 1);
        assert_eq!(matcher("a.b").unwrap().find_iter(&text).count(), 0);
        assert_eq!(messages(&p, 1).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
