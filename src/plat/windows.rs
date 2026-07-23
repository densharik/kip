//! Windows platform layer. No PTY master fd and no foreground process groups
//! here - "busy" and "which process is claude" are derived from the shell's
//! process tree (Toolhelp snapshots). Mirrors the surface of `plat::unix`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

use super::SysStats;

/// One Toolhelp snapshot as (pid, ppid) pairs.
fn enum_processes() -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                out.push((entry.th32ProcessID as i32, entry.th32ParentProcessID as i32));
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
}

fn children_map() -> HashMap<i32, Vec<i32>> {
    let mut m: HashMap<i32, Vec<i32>> = HashMap::new();
    for (pid, ppid) in enum_processes() {
        m.entry(ppid).or_default().push(pid);
    }
    m
}

fn open_query(pid: i32) -> HANDLE {
    if pid <= 0 {
        return std::ptr::null_mut();
    }
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) }
}

/// Full executable path of a process (empty when access is denied).
pub fn process_path(pid: i32) -> Option<String> {
    let h = open_query(pid);
    if h.is_null() {
        return None;
    }
    let mut buf = [0u16; 4096];
    let mut size = buf.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(h) };
    if ok == 0 || size == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..size as usize]))
}

/// Short executable name of a process.
pub fn process_name(pid: i32) -> Option<String> {
    process_path(pid).map(|p| {
        p.rsplit(['\\', '/']).next().unwrap_or(&p).to_string()
    })
}

/// Command line of a process. Reading another process's argv needs its PEB;
/// not worth the fragile unsafe for v1 - the typed-command binding and the
/// sessions/<pid>.json metadata cover session ids without it.
pub fn process_args(_pid: i32) -> Option<String> {
    None
}

/// Process looks like claude by its executable path (the native installer puts
/// it under a `claude` directory).
pub fn is_claude_proc(pid: i32) -> bool {
    process_path(pid).is_some_and(|p| p.to_ascii_lowercase().contains("claude"))
}

/// First process in `root`'s tree that looks like claude (claude may run
/// behind a wrapper, so it can be a descendant rather than `root` itself).
pub fn find_claude_desc(root: i32) -> Option<i32> {
    if is_claude_proc(root) {
        return Some(root);
    }
    let kids = children_map();
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        if p != root && is_claude_proc(p) {
            return Some(p);
        }
        if let Some(cs) = kids.get(&p) {
            stack.extend(cs);
        }
    }
    None
}

/// The process currently running under the shell, or `shell_pid` when the
/// shell sits idle at its prompt. Callers treat `!= shell_pid` as "busy".
/// `_master_fd` is unused on Windows (there is no tty master).
pub fn foreground_pgid(_master_fd: i32, shell_pid: i32) -> Option<i32> {
    if shell_pid <= 0 {
        return None;
    }
    let kids = children_map();
    match kids.get(&shell_pid).and_then(|v| v.iter().copied().max()) {
        Some(child) => Some(child),
        None => Some(shell_pid),
    }
}

/// A process's working directory is not readable without walking its PEB;
/// on Windows the session cwd stays at what it was spawned/navigated to.
pub fn pid_cwd(_pid: i32) -> Option<PathBuf> {
    None
}

/// No file/image clipboard handling on Windows in v1 (egui still handles text
/// paste and Explorer drag-and-drop).
pub fn clipboard_paths() -> Option<String> {
    None
}

/// Desktop notifications need a toast/window handle (owned by eframe) and are
/// deferred to a later version; the sidebar unread markers still work.
pub fn notify(_title: &str, _body: &str, _sound: bool) {}

fn working_set_kb(pid: i32) -> u64 {
    let h = open_query(pid);
    if h.is_null() {
        return 0;
    }
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetProcessMemoryInfo(
            h,
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    unsafe { CloseHandle(h) };
    if ok == 0 { 0 } else { counters.WorkingSetSize as u64 / 1024 }
}

/// Memory per process tree (rss in KB). CPU sampling needs two time snapshots
/// with an interval and is left at 0 for v1; the memory bars stay meaningful.
pub fn sample_stats(targets: &[(String, i32)]) -> SysStats {
    let kids = children_map();
    let procs = targets
        .iter()
        .map(|(label, root)| {
            let mut rss = 0u64;
            let mut seen = HashSet::new();
            let mut stack = vec![*root];
            while let Some(p) = stack.pop() {
                if !seen.insert(p) {
                    continue;
                }
                rss += working_set_kb(p);
                if let Some(cs) = kids.get(&p) {
                    stack.extend(cs);
                }
            }
            (label.clone(), 0.0f32, rss)
        })
        .collect();
    SysStats { procs }
}
