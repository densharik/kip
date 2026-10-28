// Embed the version as 0.1.<patch>, where patch is the commit count. CI passes
// KIP_PATCH; a local build derives it from git. The updater compares versions.
use std::process::Command;

fn main() {
    let patch = std::env::var("KIP_PATCH")
        .ok()
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
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
    // Covers repos whose refs are packed (loose refs/heads/main then absent).
    println!("cargo:rerun-if-changed=.git/packed-refs");
    build_ghostty_vt();
}

/// libghostty-vt (the terminal core) from the pinned vendor/ghostty submodule,
/// built with zig and linked statically. src/vt/sys.rs holds bindgen output for
/// the same commit's headers; regenerate it when the submodule moves.
fn build_ghostty_vt() {
    use std::path::PathBuf;
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let src = PathBuf::from(env("CARGO_MANIFEST_DIR")).join("vendor/ghostty");
    if !src.join("build.zig").exists() {
        panic!("vendor/ghostty is empty: run `git submodule update --init`");
    }
    for p in ["build.zig", "build.zig.zon", "src", "include", "pkg"] {
        println!("cargo:rerun-if-changed={}", src.join(p).display());
    }
    println!("cargo:rerun-if-env-changed=ZIG");

    // Name the target explicitly: zig's "native" would tune for the build
    // machine's CPU and pick the GNU ABI on Windows, where Rust links MSVC.
    let target = env("TARGET");
    let arch = env("CARGO_CFG_TARGET_ARCH");
    let zig_target = match env("CARGO_CFG_TARGET_OS").as_str() {
        "macos" => {
            let min = std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".into());
            format!("{arch}-macos.{min}")
        },
        "windows" if target.ends_with("msvc") => format!("{arch}-windows-msvc"),
        "windows" => format!("{arch}-windows-gnu"),
        "linux" => format!("{arch}-linux-gnu"),
        os => panic!("libghostty-vt: no zig target for {os}"),
    };
    println!("cargo:rerun-if-env-changed=MACOSX_DEPLOYMENT_TARGET");

    let out = PathBuf::from(env("OUT_DIR")).join("ghostty");
    let zig = std::env::var("ZIG").unwrap_or_else(|_| "zig".into());
    let mut cmd = Command::new(&zig);
    cmd.current_dir(&src)
        .args(["build", "-Demit-lib-vt", "-Doptimize=ReleaseFast", "-Demit-xcframework=false"])
        .arg(format!("-Dtarget={zig_target}"))
        .arg("--prefix")
        .arg(&out);
    if zig_target.contains("windows-msvc") {
        // simdutf/highway are C++ and do not build against the MSVC headers.
        cmd.arg("-Dsimd=false");
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("cannot run `{zig}` (zig 0.17 is needed to build libghostty-vt): {e}"));
    assert!(status.success(), "zig build of libghostty-vt failed");

    let lib = out.join("lib");
    if zig_target.contains("windows") {
        println!("cargo:rustc-link-search=native={}", lib.display());
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
        println!("cargo:rustc-link-lib=ntdll");
    } else {
        // lib/ also holds the shared library, which the linker would prefer.
        let only = out.join("static");
        std::fs::create_dir_all(&only).unwrap();
        std::fs::copy(lib.join("libghostty-vt.a"), only.join("libghostty-vt.a")).unwrap();
        println!("cargo:rustc-link-search=native={}", only.display());
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}
