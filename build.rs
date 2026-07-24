// Embed the version as 0.1.<patch>, where patch is the commit count. CI passes
// KIP_PATCH; a local build derives it from git. The updater compares versions.
use std::process::Command;

fn main() {
    let patch = std::env::var("KIP_PATCH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-list", "--count", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "0".into());
    println!("cargo:rustc-env=KIP_VERSION=0.1.{patch}");
    println!("cargo:rerun-if-env-changed=KIP_PATCH");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads/main");
}
