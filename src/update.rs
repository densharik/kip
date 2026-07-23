//! In-app self-update from GitHub Releases. Network via curl (present on macOS
//! and Windows 10+); no extra crates. Check runs in the background and reports
//! over an mpsc channel. Applying: Windows swaps the exe in place, macOS runs
//! the release .pkg via the system installer (handles the root-owned
//! /Applications case). Both relaunch on success.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Sender;

const LATEST_API: &str = "https://api.github.com/repos/densharik/kip/releases/latest";
const RELEASES_URL: &str = "https://github.com/densharik/kip/releases/latest";

#[cfg(windows)]
const ASSET_NAME: &str = "kip.exe";
#[cfg(target_os = "macos")]
const ASSET_NAME: &str = "kip-installer.pkg";
#[cfg(all(not(windows), not(target_os = "macos")))]
const ASSET_NAME: &str = "";

/// Build identity (git commit), embedded by build.rs. Updates are keyed on this
/// instead of a version number - every released commit is a distinct build.
pub fn current_build() -> &'static str {
    env!("KIP_BUILD")
}

fn short(build: &str) -> String {
    build.trim_start_matches("build-").chars().take(7).collect()
}

/// Short, human-facing form of the running build.
pub fn current_label() -> String {
    let b = current_build();
    if b == "dev" { "dev".into() } else { short(b) }
}

#[derive(Clone)]
pub struct Release {
    pub display: String,
    pub asset_url: String,
}

pub enum UpdateMsg {
    /// Ok(Some) = newer available, Ok(None) = up to date.
    Checked(Result<Option<Release>, String>),
    /// Only ever carries Err: a success relaunches and exits the process.
    Applied(Result<(), String>),
}

fn do_check() -> Result<Option<Release>, String> {
    if ASSET_NAME.is_empty() {
        return Err("автообновление доступно только на macOS и Windows".into());
    }
    let out = Command::new("curl")
        .args([
            "-sL",
            "--max-time",
            "20",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "User-Agent: kip-updater",
            LATEST_API,
        ])
        .output()
        .map_err(|e| format!("curl не запустился: {e}"))?;
    if !out.status.success() {
        return Err("не удалось связаться с GitHub".into());
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|_| "неожиданный ответ GitHub".to_string())?;
    let tag = v["tag_name"].as_str().ok_or("нет tag_name в ответе")?;
    // Same commit as what is running -> nothing to do.
    if tag == current_build() {
        return Ok(None);
    }
    let asset_url = v["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(ASSET_NAME)))
        .and_then(|x| x["browser_download_url"].as_str())
        .ok_or_else(|| format!("в релизе {tag} нет {ASSET_NAME}"))?;
    Ok(Some(Release { display: short(tag), asset_url: asset_url.to_string() }))
}

pub fn check(tx: Sender<UpdateMsg>, egui: egui::Context) {
    std::thread::spawn(move || {
        let res = do_check();
        if tx.send(UpdateMsg::Checked(res)).is_ok() {
            egui.request_repaint();
        }
    });
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let out = Command::new("curl")
        .args(["-sL", "--max-time", "600", "-o"])
        .arg(dest)
        .arg(url)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err("скачивание не удалось".into());
    }
    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if size < 100_000 {
        return Err("скачанный файл повреждён".into());
    }
    Ok(())
}

#[cfg(windows)]
fn do_apply(rel: &Release) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| format!("нет пути к exe: {e}"))?;
    let dir = cur.parent().ok_or("нет каталога exe")?;
    let new = dir.join("kip.update.exe");
    download(&rel.asset_url, &new)?;
    let old = dir.join("kip.old.exe");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&cur, &old).map_err(|e| format!("нет прав на замену: {e}"))?;
    if let Err(e) = std::fs::rename(&new, &cur) {
        let _ = std::fs::rename(&old, &cur);
        return Err(format!("замена не удалась: {e}"));
    }
    Command::new(&cur).spawn().map_err(|e| format!("не удалось перезапустить: {e}"))?;
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
fn do_apply(rel: &Release) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| format!("нет пути к бинарнику: {e}"))?;
    let tmp = std::env::temp_dir().join(format!("kip-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("temp: {e}"))?;
    let pkg = tmp.join("kip.pkg");
    download(&rel.asset_url, &pkg)?;

    // A .pkg-installed app lives in /Applications owned by root, so it cannot be
    // replaced in-process (that is the Permission denied path). Hand the .pkg to
    // the system installer with admin rights: macOS shows one password prompt
    // and swaps the bundle cleanly. `quoted form of` handles shell quoting.
    let script = format!(
        "do shell script \"installer -pkg \" & quoted form of \"{}\" & \" -target /\" \
         with administrator privileges",
        pkg.display()
    );
    let out = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") {
            return Err("обновление отменено".into());
        }
        return Err(format!("установка не удалась: {}", err.trim()));
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // Relaunch the freshly installed bundle.
    let bundle = cur
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .filter(|b| b.extension().and_then(|e| e.to_str()) == Some("app"))
        .map(|b| b.to_path_buf())
        .unwrap_or_else(|| "/Applications/kip.app".into());
    Command::new("open").arg(&bundle).spawn().map_err(|e| format!("перезапуск: {e}"))?;
    std::process::exit(0);
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn do_apply(_rel: &Release) -> Result<(), String> {
    Err("автообновление доступно только на macOS и Windows".into())
}

pub fn apply(rel: Release, tx: Sender<UpdateMsg>, egui: egui::Context) {
    std::thread::spawn(move || {
        // On success do_apply relaunches and exits; only errors return here.
        let err = do_apply(&rel).unwrap_err();
        if tx.send(UpdateMsg::Applied(Err(err))).is_ok() {
            egui.request_repaint();
        }
    });
}

pub fn open_releases() {
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(RELEASES_URL).spawn();
    #[cfg(windows)]
    let _ = Command::new("cmd").args(["/C", "start", "", RELEASES_URL]).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(RELEASES_URL).spawn();
}

/// Remove leftovers from a previous update (old binary/bundle), in the background.
pub fn cleanup() {
    std::thread::spawn(|| {
        let Ok(cur) = std::env::current_exe() else { return };
        #[cfg(windows)]
        if let Some(dir) = cur.parent() {
            let _ = std::fs::remove_file(dir.join("kip.old.exe"));
            let _ = std::fs::remove_file(dir.join("kip.update.exe"));
        }
        #[cfg(target_os = "macos")]
        if let Some(parent) = cur
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|b| b.parent())
        {
            let _ = std::fs::remove_dir_all(parent.join("kip.app.old"));
            let _ = std::fs::remove_dir_all(parent.join("kip.app.new"));
        }
        #[cfg(all(not(windows), not(target_os = "macos")))]
        let _ = &cur;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_build() {
        assert_eq!(short("build-a1b2c3d4e5f6"), "a1b2c3d");
        assert_eq!(short("dev"), "dev");
        assert_eq!(short("build-abc"), "abc");
    }
}
