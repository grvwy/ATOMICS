use std::process::Command;

fn main() {
    // Also rerun after commits and tag changes in a linked worktree.
    for name in ["HEAD", "refs", "packed-refs", "index"] {
        if let Ok(output) = Command::new("git").args(["rev-parse", "--git-path", name]).output()
            && output.status.success()
        {
            println!("cargo:rerun-if-changed={}", String::from_utf8_lossy(&output.stdout).trim());
        }
    }
    let version = Command::new("git")
        .args(["describe", "--always", "--tags", "--dirty"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .filter(|version| !version.trim().is_empty())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").expect("crate version"));
    println!("cargo:rustc-env=ATOMICS_BUILD_VERSION={}", version.trim());
    println!("cargo:rustc-env=ATOMICS_TARGET={}", std::env::var("TARGET").expect("cargo's target"));
    release_signing_key();
}

/// Builds the release trust anchor in when the checkout has it, and nothing otherwise: the updater then refuses
/// to install a download. The folder is what is watched, because a missing file would rerun this every build.
fn release_signing_key() {
    println!("cargo:rerun-if-changed=../../assets");
    let key = std::fs::read_to_string("../../assets/release-signing-key.pub").ok();
    let out = std::path::Path::new(&std::env::var("OUT_DIR").expect("cargo's output folder")).join("release_key.rs");
    std::fs::write(out, format!("{key:?}")).expect("writing the release key's source");
}
