use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub font_size: f32,
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_size: 13.0,
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
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSession {
    pub cwd: PathBuf,
    pub claude_session_id: Option<String>,
    pub claude_title: Option<String>,
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
    match serde_json::from_slice(&bytes) {
        Ok(state) => state,
        Err(_) => {
            // Keep the unparseable file around instead of silently overwriting it.
            let _ = std::fs::copy(&path, path.with_extension("json.bad"));
            AppState::default()
        },
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
