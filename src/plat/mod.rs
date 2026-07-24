//! Platform layer. The rest of the app calls `plat::*` for everything
//! OS-specific (process introspection, PTY foreground, notifications,
//! clipboard); the unix and windows submodules implement the same surface.

#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// Per-process resource sample: (label, cpu percent, memory kilobytes), one row
/// per target passed to `sample_stats` - the whole process tree when its `tree`
/// flag is set, or only the root process when it is not. Memory is macOS
/// phys_footprint, Windows private commit, and `ps` rss on other unix - a tree
/// is summed without counting shared pages once per process.
pub struct SysStats {
    pub procs: Vec<(String, f32, u64)>,
}

/// Delete day-old kip-paste-*.png files from the temp dir (background).
/// Pure std, identical on every platform.
pub fn sweep_paste_temp() {
    std::thread::spawn(|| {
        if let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) {
            for e in rd.flatten() {
                let name = e.file_name();
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| t.elapsed().is_ok_and(|d| d.as_secs() > 24 * 3600));
                if name.to_string_lossy().starts_with("kip-paste-") && old {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    });
}
