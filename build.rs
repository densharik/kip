// Embed a build id (git commit) so the updater can compare builds without a
// version number. CI passes KIP_BUILD; a local build falls back to git HEAD.
use std::process::Command;

fn main() {
    let build = std::env::var("KIP_BUILD")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| format!("build-{}", String::from_utf8_lossy(&o.stdout).trim()))
                .filter(|s| s.len() > 6)
        })
        .unwrap_or_else(|| "dev".into());
    println!("cargo:rustc-env=KIP_BUILD={build}");
    println!("cargo:rerun-if-env-changed=KIP_BUILD");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
}
