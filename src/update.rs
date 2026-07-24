//! In-app self-update from GitHub Releases. Network via curl (present on macOS
//! and Windows 10+); no extra crates. Check runs in the background and reports
//! over an mpsc channel. Applying: Windows swaps the exe in place; macOS
//! replaces the .app bundle in place (no password) and falls back to the
//! release .pkg via the system installer (one password) when the app is
//! root-owned. Both relaunch on success.

use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Sender;

use crate::i18n::tr;

const LATEST_API: &str = "https://api.github.com/repos/densharik/kip/releases/latest";
const RELEASES_URL: &str = "https://github.com/densharik/kip/releases/latest";

#[cfg(windows)]
const ASSET_NAME: &str = "kip.exe";
#[cfg(target_os = "macos")]
const ASSET_NAME: &str = "kip-macos.zip";
#[cfg(all(not(windows), not(target_os = "macos")))]
const ASSET_NAME: &str = "";

/// Running version (0.1.<patch>), embedded by build.rs.
pub fn current_version() -> &'static str {
    env!("KIP_VERSION")
}

pub fn current_label() -> String {
    current_version().to_string()
}

/// Parse "v0.1.17" / "0.1.17" into a comparable tuple.
fn parse_ver(s: &str) -> (u32, u32, u32) {
    let mut p = s.trim().trim_start_matches('v').split('.').map(|x| {
        x.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>().unwrap_or(0)
    });
    (p.next().unwrap_or(0), p.next().unwrap_or(0), p.next().unwrap_or(0))
}

#[derive(Clone)]
pub struct Release {
    pub display: String,
    pub asset_url: String,
    /// macOS .pkg installer URL, the admin fallback when the app is root-owned.
    /// None on other platforms.
    #[allow(dead_code)]
    pub pkg_url: Option<String>,
}

pub enum UpdateMsg {
    /// Ok(Some) = newer available, Ok(None) = up to date.
    Checked(Result<Option<Release>, String>),
    /// Only ever carries Err: a success relaunches and exits the process.
    Applied(Result<(), String>),
}

fn do_check() -> Result<Option<Release>, String> {
    if ASSET_NAME.is_empty() {
        return Err(tr("автообновление доступно только на macOS и Windows", "auto-update is only on macOS and Windows").into());
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
        .map_err(|e| format!("{}: {e}", tr("curl не запустился", "curl failed to start")))?;
    if !out.status.success() {
        return Err(tr("не удалось связаться с GitHub", "could not reach GitHub").into());
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|_| tr("неожиданный ответ GitHub", "unexpected GitHub response").to_string())?;
    let tag = v["tag_name"].as_str().ok_or(tr("нет tag_name в ответе", "no tag_name in response"))?;
    // Not newer than what is running -> nothing to do.
    if parse_ver(tag) <= parse_ver(current_version()) {
        return Ok(None);
    }
    let find = |name: &str| -> Option<String> {
        v["assets"]
            .as_array()?
            .iter()
            .find(|x| x["name"].as_str() == Some(name))
            .and_then(|x| x["browser_download_url"].as_str())
            .map(str::to_string)
    };
    let asset_url = find(ASSET_NAME).ok_or_else(|| format!("{} {tag} {} {ASSET_NAME}", tr("в релизе", "release"), tr("нет", "has no")))?;
    let pkg_url = if cfg!(target_os = "macos") { find("kip-installer.pkg") } else { None };
    let display = tag.trim_start_matches('v').to_string();
    Ok(Some(Release { display, asset_url, pkg_url }))
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
        return Err(tr("скачивание не удалось", "download failed").into());
    }
    let size = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if size < 100_000 {
        return Err(tr("скачанный файл повреждён", "downloaded file is corrupt").into());
    }
    Ok(())
}

#[cfg(windows)]
fn do_apply(rel: &Release) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| format!("{}: {e}", tr("нет пути к exe", "no path to exe")))?;
    let dir = cur.parent().ok_or(tr("нет каталога exe", "no exe directory"))?;
    let new = dir.join("kip.update.exe");
    download(&rel.asset_url, &new)?;
    let old = dir.join("kip.old.exe");
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&cur, &old).map_err(|e| format!("{}: {e}", tr("нет прав на замену", "no permission to replace")))?;
    if let Err(e) = std::fs::rename(&new, &cur) {
        let _ = std::fs::rename(&old, &cur);
        return Err(format!("{}: {e}", tr("замена не удалась", "replace failed")));
    }
    Command::new(&cur).spawn().map_err(|e| format!("{}: {e}", tr("не удалось перезапустить", "failed to relaunch")))?;
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
fn bundle_of(cur: &Path) -> Result<std::path::PathBuf, String> {
    // .../kip.app/Contents/MacOS/kip -> kip.app
    let b = cur
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .ok_or(tr("не найден .app бандл", ".app bundle not found"))?;
    if b.extension().and_then(|e| e.to_str()) != Some("app") {
        return Err(tr("запущен не из .app - обнови вручную", "not running from .app - update manually").into());
    }
    Ok(b.to_path_buf())
}

/// Try to replace the bundle in place, no password. Ok(true) = done,
/// Ok(false) = permission denied (caller should use the admin .pkg), Err = a
/// hard failure (download/unpack).
#[cfg(target_os = "macos")]
fn swap_install(bundle: &Path, rel: &Release) -> Result<bool, String> {
    let parent = bundle.parent().ok_or(tr("нет каталога бандла", "no bundle directory"))?;
    let probe = parent.join(".kip-write-test");
    if std::fs::write(&probe, b"x").is_err() {
        return Ok(false);
    }
    let _ = std::fs::remove_file(&probe);

    let tmp = std::env::temp_dir().join(format!("kip-swap-{}", std::process::id()));
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
        return Err(tr("не удалось распаковать архив", "could not unpack archive").into());
    }
    let newapp = tmp.join("kip.app");
    if !newapp.exists() {
        return Err(tr("в архиве нет kip.app", "archive has no kip.app").into());
    }
    let _ = Command::new("xattr").args(["-cr"]).arg(&newapp).status();

    // Copy next to the target (same filesystem) so the final swap is atomic.
    let staged = parent.join("kip.app.new");
    let _ = std::fs::remove_dir_all(&staged);
    let staged_ok = Command::new("ditto")
        .arg(&newapp)
        .arg(&staged)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !staged_ok {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(false);
    }
    let backup = parent.join("kip.app.old");
    let _ = std::fs::remove_dir_all(&backup);
    if let Err(e) = std::fs::rename(bundle, &backup) {
        let _ = std::fs::remove_dir_all(&staged);
        let _ = std::fs::remove_dir_all(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Ok(false);
        }
        return Err(format!("{}: {e}", tr("не сдвинуть старую версию", "could not move the old version")));
    }
    if let Err(e) = std::fs::rename(&staged, bundle) {
        let _ = std::fs::rename(&backup, bundle);
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("{}: {e}", tr("замена бандла", "bundle swap")));
    }
    let _ = std::fs::remove_dir_all(&backup);
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(true)
}

/// Admin fallback: run the release .pkg through the system installer. One
/// password prompt; the .pkg hands the app to the user so the next update can
/// use the passwordless swap path.
#[cfg(target_os = "macos")]
fn pkg_install(rel: &Release) -> Result<(), String> {
    let pkg_url = rel.pkg_url.as_deref().ok_or(tr("в релизе нет .pkg", "release has no .pkg"))?;
    let tmp = std::env::temp_dir().join(format!("kip-pkg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("temp: {e}"))?;
    let pkg = tmp.join("kip.pkg");
    download(pkg_url, &pkg)?;
    // `quoted form of` handles shell quoting inside the AppleScript command.
    let script = format!(
        "do shell script \"installer -pkg \" & quoted form of \"{}\" & \" -target /\" \
         with administrator privileges",
        pkg.display()
    );
    let out = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| format!("osascript: {e}"))?;
    let _ = std::fs::remove_dir_all(&tmp);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") {
            return Err(tr("обновление отменено", "update cancelled").into());
        }
        return Err(format!("{}: {}", tr("установка не удалась", "install failed"), err.trim()));
    }
    Ok(())
}

/// Relaunch the installed bundle. `open` must run AFTER we quit: while this
/// instance is alive macOS just reactivates the old app instead of launching
/// the new one. Detach a helper that waits for us to exit.
#[cfg(target_os = "macos")]
fn relaunch(bundle: &Path) -> Result<(), String> {
    let quoted = format!("'{}'", bundle.to_string_lossy().replace('\'', "'\\''"));
    Command::new("sh")
        .arg("-c")
        .arg(format!("sleep 1; open -n {quoted}"))
        .spawn()
        .map_err(|e| format!("{}: {e}", tr("перезапуск", "relaunch")))?;
    std::process::exit(0);
}

#[cfg(target_os = "macos")]
fn do_apply(rel: &Release) -> Result<(), String> {
    let cur = std::env::current_exe().map_err(|e| format!("{}: {e}", tr("нет пути к бинарнику", "no path to binary")))?;
    let bundle = bundle_of(&cur)?;
    if !swap_install(&bundle, rel)? {
        pkg_install(rel)?;
    }
    relaunch(&bundle)
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn do_apply(_rel: &Release) -> Result<(), String> {
    Err(tr("автообновление доступно только на macOS и Windows", "auto-update is only on macOS and Windows").into())
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
        assert!(parse_ver("v0.1.18") > parse_ver("0.1.17"));
        assert!(parse_ver("0.1.17") == parse_ver("v0.1.17"));
        assert!(parse_ver("v0.2.0") > parse_ver("v0.1.99"));
        assert!(parse_ver("v0.1.5") <= parse_ver("0.1.5"));
    }
}
