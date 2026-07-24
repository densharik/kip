//! Tiny two-language i18n. `tr(ru, en)` returns the string for the active
//! language, which is set once at startup (auto-detected or from settings) and
//! whenever the user switches. Single-threaded UI, so a relaxed atomic is enough.
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Ru,
    En,
}

static LANG: AtomicU8 = AtomicU8::new(0); // 0 = Ru, 1 = En

pub fn set(lang: Lang) {
    LANG.store(if lang == Lang::En { 1 } else { 0 }, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 1 { Lang::En } else { Lang::Ru }
}

/// The string for the active language.
pub fn tr(ru: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::En => en,
        Lang::Ru => ru,
    }
}

/// Resolve a settings value ("auto" | "ru" | "en") to a language, detecting from
/// the system locale when "auto".
pub fn resolve(setting: &str) -> Lang {
    match setting {
        "ru" => Lang::Ru,
        "en" => Lang::En,
        _ => detect(),
    }
}

fn from_locale(s: &str) -> Lang {
    if s.trim().to_lowercase().starts_with("ru") { Lang::Ru } else { Lang::En }
}

fn detect() -> Lang {
    // macOS GUI apps often have no LANG set, so ask the system locale directly.
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("defaults")
        .args(["read", "-g", "AppleLocale"])
        .output()
    {
        let s = String::from_utf8_lossy(&out.stdout);
        if !s.trim().is_empty() {
            return from_locale(&s);
        }
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return from_locale(&v);
            }
        }
    }
    Lang::En
}
