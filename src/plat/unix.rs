use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::SysStats;

/// One `ps` pass; each target's cpu/rss is summed over its whole process tree.
pub fn sample_stats(targets: &[(String, i32)]) -> SysStats {
    let mut rows: Vec<(i32, i32, u64, f32)> = Vec::new();
    if let Ok(out) = Command::new("ps").args(["-axo", "pid=,ppid=,rss=,pcpu="]).output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut it = line.split_whitespace();
            let (Some(pid), Some(ppid), Some(rss), Some(cpu)) =
                (it.next(), it.next(), it.next(), it.next())
            else {
                continue;
            };
            let (Ok(pid), Ok(ppid), Ok(rss), Ok(cpu)) =
                (pid.parse(), ppid.parse(), rss.parse(), cpu.parse())
            else {
                continue;
            };
            rows.push((pid, ppid, rss, cpu));
        }
    }
    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    let mut by_pid: HashMap<i32, usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        children.entry(r.1).or_default().push(i);
        by_pid.insert(r.0, i);
    }
    let procs = targets
        .iter()
        .map(|(label, root)| {
            let (mut rss, mut cpu) = (0u64, 0f32);
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![*root];
            while let Some(pid) = stack.pop() {
                // The ps snapshot is not atomic; a reused pid could form a cycle.
                if !seen.insert(pid) {
                    continue;
                }
                if let Some(&i) = by_pid.get(&pid) {
                    rss += rows[i].2;
                    cpu += rows[i].3;
                }
                if let Some(kids) = children.get(&pid) {
                    stack.extend(kids.iter().map(|&i| rows[i].0));
                }
            }
            (label.clone(), cpu, rss)
        })
        .collect();
    SysStats { procs }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
}

/// Current working directory of an arbitrary process (macOS, via PROC_PIDVNODEPATHINFO).
#[cfg(target_os = "macos")]
pub fn pid_cwd(pid: i32) -> Option<PathBuf> {
    const PROC_PIDVNODEPATHINFO: libc::c_int = 9;
    // sizeof(struct proc_vnodepathinfo) = 2 * (152 + MAXPATHLEN)
    const VNODE_INFO_SIZE: usize = 152;
    const MAXPATHLEN: usize = 1024;
    const SIZE: usize = 2 * (VNODE_INFO_SIZE + MAXPATHLEN);

    let mut buf = [0u8; SIZE];
    let ret = unsafe {
        proc_pidinfo(pid, PROC_PIDVNODEPATHINFO, 0, buf.as_mut_ptr() as *mut _, SIZE as libc::c_int)
    };
    if ret <= 0 {
        return None;
    }
    let path = &buf[VNODE_INFO_SIZE..VNODE_INFO_SIZE + MAXPATHLEN];
    let end = path.iter().position(|&b| b == 0).unwrap_or(MAXPATHLEN);
    if end == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&path[..end]).into_owned()))
}

#[cfg(not(target_os = "macos"))]
pub fn pid_cwd(pid: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// File path(s) from the clipboard: a copied Finder file, or raw image data
/// saved to a temp png. Used when a paste carries no text.
pub fn clipboard_paths() -> Option<String> {
    let run = |script: &str| -> Option<String> {
        let out = Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !s.is_empty()).then_some(s)
    };
    if let Some(p) = run("POSIX path of (the clipboard as «class furl»)") {
        // AppleScript happily coerces plain text into a fake file URL - only
        // trust paths that actually exist.
        if std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    super::sweep_paste_temp();
    let dest = std::env::temp_dir().join(format!(
        "kip-paste-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let dest_esc = dest.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "set d to the clipboard as «class PNGf»\n\
         set f to open for access POSIX file \"{dest_esc}\" with write permission\n\
         write d to f\n\
         close access f\n\
         return \"{dest_esc}\""
    );
    run(&script)
}

/// Foreground process group of a PTY. `_shell_pid` is unused on unix (the
/// pgid comes straight from the tty); it exists for the Windows signature,
/// which has no tty and derives the foreground process from the shell's tree.
pub fn foreground_pgid(master_fd: i32, _shell_pid: i32) -> Option<i32> {
    let pgid = unsafe { libc::tcgetpgrp(master_fd) };
    (pgid > 0).then_some(pgid)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_name(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
    fn proc_pidpath(pid: libc::c_int, buffer: *mut libc::c_void, buffersize: u32) -> libc::c_int;
}

/// Full executable path of a process. The claude binary is installed as
/// `.../claude/versions/<x.y.z>`, so its short name is just the version -
/// only the path reveals what it is.
#[cfg(target_os = "macos")]
pub fn process_path(pid: i32) -> Option<String> {
    // PROC_PIDPATHINFO_MAXSIZE = 4 * MAXPATHLEN.
    let mut buf = [0u8; 4096];
    let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(not(target_os = "macos"))]
pub fn process_path(pid: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Full command line of a process (argv), e.g. "claude --resume <id>".
pub fn process_args(pid: i32) -> Option<String> {
    // -ww: without it ps clips to 80 columns and truncates long session ids.
    let out = Command::new("ps")
        .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// Process looks like claude by name or executable path (the binary is
/// installed as `.../claude/versions/<x.y.z>`, so the name alone is not enough).
pub fn is_claude_proc(pid: i32) -> bool {
    process_name(pid).is_some_and(|n| n.contains("claude"))
        || process_path(pid).is_some_and(|p| p.contains("claude"))
}

/// First process in `root`'s tree (including root itself) that looks like
/// claude. Finds claude behind wrapper tools (session pickers like cchb spawn
/// it as a child, so the PTY foreground leader is the wrapper, not claude).
pub fn find_claude_desc(root: i32) -> Option<i32> {
    if is_claude_proc(root) {
        return Some(root);
    }
    let out = Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(Ok(pid)), Some(Ok(ppid))) =
            (it.next().map(str::parse::<i32>), it.next().map(str::parse::<i32>))
        else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
    }
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        if p != root && is_claude_proc(p) {
            return Some(p);
        }
        if let Some(kids) = children.get(&p) {
            stack.extend(kids);
        }
    }
    None
}

/// Short name of a process (the group leader pid works for PTY foreground groups).
#[cfg(target_os = "macos")]
pub fn process_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 64];
    let n = unsafe { proc_name(pid, buf.as_mut_ptr() as *mut _, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(not(target_os = "macos"))]
pub fn process_name(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn notify(title: &str, body: &str, sound: bool) {
    #[cfg(target_os = "macos")]
    let spawned = {
        // Raw newlines break AppleScript string literals; \0 truncates the argv.
        let esc = |s: &str| {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace(['\n', '\r'], " ")
                .replace('\0', "")
        };
        let script = format!(
            "display notification \"{}\" with title \"{}\"{}",
            esc(body),
            esc(title),
            if sound { " sound name \"Glass\"" } else { "" }
        );
        Command::new("osascript")
            .arg("-e")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    #[cfg(not(target_os = "macos"))]
    let spawned = {
        let _ = sound;
        Command::new("notify-send")
            .arg(title)
            .arg(body)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    // Reap in the background so notifications do not pile up as zombies.
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
