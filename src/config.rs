use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
    /// Terminal font key (see FONT_CHOICES in main.rs).
    pub font: String,
    /// Global UI zoom, 1.0 = 100%.
    pub ui_scale: f32,
    pub scrollback: usize,
    /// Minutes of inactivity before a session is suspended. 0 = disabled.
    pub idle_suspend_min: u32,
    pub notify_bell: bool,
    pub notify_job_done: bool,
    pub notify_sound: bool,
    pub skip_permissions_default: bool,
    pub claude_cmd: String,
    /// Selected terminal text is copied to the clipboard immediately.
    pub copy_on_select: bool,
    /// The statusline hook is installed (exact context % for any user).
    pub ctx_hook: bool,
    /// Serialized original statusLine value for rollback ("" = key was absent).
    pub prev_statusline: Option<String>,
    /// Terminal color preset key (see palette::PRESETS).
    pub theme: String,
    /// Optional selection-highlight override; None uses the preset's color.
    pub accent: Option<[u8; 3]>,
    /// UI language: "auto" | "ru" | "en".
    pub lang: String,
    /// Key of the usage limit pinned to the corner chip (see usage::Limit::key).
    pub usage_pin: Option<String>,
    /// Settings schema version, so a changed default can reach existing installs
    /// (see `migrate`). The field-level `default` is what makes that work: the
    /// struct-level one would hand an old file the current version and skip the
    /// migration entirely.
    #[serde(default)]
    pub version: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // Bundled Hack at a size that lands on ~14.4 physical pixels with the
            // default UI scale - the same on-screen size other terminals use, and
            // identical on every machine since the font ships in the binary.
            font_size: 12.0,
            font: "hack".into(),
            ui_scale: 1.2,
            scrollback: 5000,
            idle_suspend_min: 10,
            notify_bell: true,
            notify_job_done: true,
            notify_sound: true,
            skip_permissions_default: true,
            claude_cmd: "claude".into(),
            copy_on_select: true,
            ctx_hook: false,
            prev_statusline: None,
            theme: "tomorrow".into(),
            accent: None,
            lang: "auto".into(),
            usage_pin: None,
            version: SETTINGS_VERSION,
        }
    }
}

/// Bump when a default changes in a way that existing installs should pick up.
const SETTINGS_VERSION: u32 = 1;

/// v1: terminal text is the bundled Hack at 12pt. Before this the font came from
/// the system (Menlo, or whatever was picked while chasing the "text looks
/// wrong" reports) at a size that rendered noticeably larger than other
/// terminals. Those picks were troubleshooting, not preference, so they are
/// replaced once instead of carried forward.
fn migrate(s: &mut Settings) {
    if s.version < 1 {
        let d = Settings::default();
        s.font = d.font;
        s.font_size = d.font_size;
    }
    s.version = SETTINGS_VERSION;
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSession {
    pub cwd: PathBuf,
    pub claude_session_id: Option<String>,
    pub claude_title: Option<String>,
    /// User-set name (double-click on the session title). Wins over claude_title.
    pub custom_name: Option<String>,
    /// Manual group this session belongs to (None = ungrouped, shown at the top).
    pub group: Option<String>,
    pub skip_permissions: bool,
    pub keep_awake: bool,
    pub snapshot: Option<String>,
}

impl Default for SavedSession {
    fn default() -> Self {
        Self {
            cwd: dirs::home_dir().unwrap_or_else(|| "/".into()),
            claude_session_id: None,
            claude_title: None,
            custom_name: None,
            group: None,
            skip_permissions: true,
            keep_awake: false,
            snapshot: None,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    pub settings: Settings,
    pub sessions: Vec<SavedSession>,
    /// Names of collapsed session groups (persisted across restarts).
    pub collapsed_groups: Vec<String>,
}

fn state_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| "/tmp".into()).join("kip").join("state.json")
}

pub fn load_state() -> AppState {
    let path = state_path();
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 16 * 1024 * 1024) {
        return AppState::default();
    }
    let Ok(bytes) = std::fs::read(&path) else { return AppState::default() };
    match serde_json::from_slice::<AppState>(&bytes) {
        Ok(mut state) => {
            migrate(&mut state.settings);
            state
        },
        Err(_) => {
            // Keep the unparseable file around instead of silently overwriting it.
            let _ = std::fs::copy(&path, path.with_extension("json.bad"));
            AppState::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_font_default_reaches_existing_installs_once() {
        let json = r#"{"font":"sfmono","font_size":13.5,"ui_scale":1.2,"theme":"tomorrow"}"#;
        let mut s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(s.version, 0, "a file written before versioning must read as v0");

        migrate(&mut s);
        assert_eq!(s.font, "hack");
        assert_eq!(s.font_size, 12.0);
        assert_eq!(s.ui_scale, 1.2, "migration only touches the font");
        assert_eq!(s.version, SETTINGS_VERSION);

        // A later pick survives: the reset happens once, not on every launch.
        s.font = "menlo".into();
        s.font_size = 15.0;
        migrate(&mut s);
        assert_eq!(s.font, "menlo");
        assert_eq!(s.font_size, 15.0);
    }
}

pub fn save_state(state: &AppState) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write-then-rename so a crash mid-write cannot truncate the existing state.
    if let Ok(json) = serde_json::to_vec_pretty(state) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}
