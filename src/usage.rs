//! Claude.ai subscription limits - the same numbers `/usage` shows inside
//! Claude Code. Source: GET /api/oauth/usage with the local Claude Code OAuth
//! token (macOS Keychain, `.credentials.json` elsewhere). Network via curl,
//! like update.rs - no extra crates.
//!
//! The response's `limits` array already matches the plan: Plus gets a session
//! and a weekly row, Max gets a third model-scoped one (Fable). Nothing here
//! knows about plans - it renders whatever the array carries.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use crate::i18n::tr;

const USAGE_API: &str = "https://api.anthropic.com/api/oauth/usage";

#[derive(Clone)]
pub struct Limit {
    /// Stable id for pinning; survives restarts, so it goes into settings.
    pub key: String,
    pub label: String,
    /// Short form for the pinned chip.
    pub short: String,
    pub percent: f32,
    /// Unix seconds when this window resets.
    pub resets_at: Option<u64>,
}

pub enum UsageMsg {
    Ok(Vec<Limit>),
    Err(String),
}

pub fn fetch(tx: Sender<UsageMsg>, egui: egui::Context) {
    std::thread::spawn(move || {
        let msg = match do_fetch() {
            Ok(v) => UsageMsg::Ok(v),
            Err(e) => UsageMsg::Err(e),
        };
        if tx.send(msg).is_ok() {
            egui.request_repaint();
        }
    });
}

fn do_fetch() -> Result<Vec<Limit>, String> {
    let token = token().ok_or_else(|| {
        tr("нет токена Claude - войди через claude /login", "no Claude token - sign in with claude /login").to_string()
    })?;
    // curl options arrive on stdin, so the token never shows up in `ps`.
    let cfg = format!(
        "silent\nmax-time = 20\nheader = \"Authorization: Bearer {token}\"\nheader = \"anthropic-beta: oauth-2025-04-20\"\nurl = \"{USAGE_API}\"\n"
    );
    let mut child = Command::new("curl")
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "curl stdin".to_string())?
        .write_all(cfg.as_bytes())
        .map_err(|e| format!("curl: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(tr("не удалось связаться с api.anthropic.com", "could not reach api.anthropic.com").into());
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .map_err(|_| tr("неожиданный ответ API", "unexpected API response").to_string())?;
    if v["error"].is_object() {
        return Err(tr(
            "нет доступа к лимитам - запусти claude, чтобы обновить токен",
            "no access to limits - run claude to refresh the token",
        )
        .into());
    }
    let rows = v["limits"].as_array().ok_or_else(|| {
        tr("в ответе нет лимитов (подписки нет?)", "no limits in the response (no subscription?)").to_string()
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (Some(kind), Some(percent)) = (r["kind"].as_str(), r["percent"].as_f64()) else {
            continue;
        };
        let model = r["scope"]["model"]["display_name"].as_str();
        let (label, short) = match (kind, model) {
            ("session", _) => (tr("Сессия 5ч", "Session 5h").to_string(), tr("5ч", "5h").to_string()),
            ("weekly_all", _) => (tr("Неделя", "Week").to_string(), tr("7д", "7d").to_string()),
            ("weekly_scoped", Some(m)) => {
                (format!("{}: {m}", tr("Неделя", "Week")), m.to_string())
            },
            _ => (kind.to_string(), kind.to_string()),
        };
        out.push(Limit {
            key: match model {
                Some(m) => format!("{kind}:{m}"),
                None => kind.to_string(),
            },
            label,
            short,
            percent: percent as f32,
            resets_at: r["resets_at"].as_str().and_then(parse_iso),
        });
    }
    Ok(out)
}

fn token() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        // Claude Code itself stores the item through this same tool, so reading
        // it back does not raise a new keychain prompt.
        if let Ok(out) = Command::new("security")
            .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
            .output()
        {
            if out.status.success() {
                if let Some(t) = token_from_json(&out.stdout) {
                    return Some(t);
                }
            }
        }
    }
    let path = crate::session::claude_dir()?.join(".credentials.json");
    token_from_json(&std::fs::read(path).ok()?)
}

fn token_from_json(bytes: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let t = v["claudeAiOauth"]["accessToken"].as_str()?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// "2026-07-25T04:40:00.502120+00:00" -> unix seconds. The API answers in UTC.
fn parse_iso(s: &str) -> Option<u64> {
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
    let days = days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?);
    let secs = days * 86400 + num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19)?;
    u64::try_from(secs).ok()
}

/// Days since 1970-01-01 (Howard Hinnant's civil calendar algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_to_epoch() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-07-25T04:40:00.502120+00:00"), Some(1784954400));
        // Leap day, and a plain "Z" suffix instead of an offset.
        assert_eq!(parse_iso("2024-02-29T12:00:00Z"), Some(1709208000));
        assert_eq!(parse_iso("nope"), None);
    }

    #[test]
    fn token_shape() {
        let js = br#"{"claudeAiOauth":{"accessToken":"tok-1","refreshToken":"r"}}"#;
        assert_eq!(token_from_json(js).as_deref(), Some("tok-1"));
        assert_eq!(token_from_json(br#"{"claudeAiOauth":{}}"#), None);
        assert_eq!(token_from_json(b"garbage"), None);
    }
}
