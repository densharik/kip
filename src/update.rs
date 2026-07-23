//! In-app self-update from GitHub Releases. Network via curl (present on macOS
//! and Windows 10+), archive handling via ditto (macOS); no extra crates.
//! Check runs in the background and reports over an mpsc channel; applying
//! swaps the running binary/bundle in place and relaunches.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Sender;

const LATEST_API: &str = "https://api.github.com/repos/densharik/kip/releases/latest";
const RELEASES_URL: &str = "https://github.com/densharik/kip/releases/latest";

#[cfg(windows)]
const ASSET_NAME: &str = "kip.exe";
#[cfg(target_os = "macos")]
const ASSET_NAME: &str = "kip-macos.zip";
#[cfg(all(not(windows), not(target_os = "macos")))]
const ASSET_NAME: &str = "";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub asset_url: String,
}

pub enum UpdateMsg {
    /// Ok(Some) = newer available, Ok(None) = up to date.
    Checked(Result<Option<Release>, String>),
    /// Only ever carries Err: a success relaunches and exits the process.
    Applied(Result<(), String>),
}

fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split(['.', '-', '+']);
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next().unwrap_or("0").parse().ok()?;
    Some((a, b, c))
}

fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_ver(latest), parse_ver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => latest.trim_start_matches('v') != current.trim_start_matches('v'),
    }
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
    if !is_newer(tag, current_version()) {
        return Ok(None);
    }
    let asset_url = v["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(ASSET_NAME)))
        .and_then(|x| x["browser_download_url"].as_str())
        .ok_or_else(|| format!("в релизе {tag} нет {ASSET_NAME}"))?;
    Ok(Some(Release {
        version: tag.trim_start_matches('v').to_string(),
        asset_url: asset_url.to_string(),
    }))
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
    // .../kip.app/Contents/MacOS/kip -> kip.app
    let bundle = cur
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or("не найден .app бандл")?;
    if bundle.extension().and_then(|e| e.to_str()) != Some("app") {
        return Err("запущен не из .app - обнови вручную".into());
    }
    let parent = bundle.parent().ok_or("нет каталога бандла")?;
    let probe = parent.join(".kip-write-test");
    if std::fs::write(&probe, b"x").is_err() {
        return Err(format!("нет прав на запись в {}", parent.display()));
    }
    let _ = std::fs::remove_file(&probe);

    let tmp = std::env::temp_dir().join(format!("kip-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("temp: {e}"))?;
    let zip = tmp.join("kip.zip");
    download(&rel.asset_url, &zip)?;
    let ok = Command::new("ditto")
        .args(["-x", "-k"])
        .arg(&zip)
        .arg(&tmp)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err("не удалось распаковать архив".into());
    }
    let newapp = tmp.join("kip.app");
    if !newapp.exists() {
        return Err("в архиве нет kip.app".into());
    }
    let _ = Command::new("xattr").args(["-cr"]).arg(&newapp).status();

    // Copy into the install dir (same filesystem) so the final swap is atomic.
    let staged = parent.join("kip.app.new");
    let _ = std::fs::remove_dir_all(&staged);
    let ok = Command::new("ditto")
        .arg(&newapp)
        .arg(&staged)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err("не удалось подготовить новую версию".into());
    }
    let backup = parent.join("kip.app.old");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(bundle, &backup).map_err(|e| format!("не сдвинуть старую версию: {e}"))?;
    if let Err(e) = std::fs::rename(&staged, bundle) {
        let _ = std::fs::rename(&backup, bundle);
        return Err(format!("замена бандла: {e}"));
    }
    let _ = std::fs::remove_dir_all(&backup);
    let _ = std::fs::remove_dir_all(&tmp);
    Command::new("open").arg(bundle).spawn().map_err(|e| format!("перезапуск: {e}"))?;
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
    fn version_compare() {
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.0", "0.1.2"));
        // unparseable falls back to string inequality
        assert!(is_newer("weird", "0.1.1"));
        assert!(!is_newer("v0.1.1", "v0.1.1"));
    }
}
