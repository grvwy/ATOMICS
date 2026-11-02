//! The web UI crate must build for the browser: it may not pull a desktop-only dependency, on any
//! target. Section 4.1 of docs/decisions/2026-11-02-embedded-atomics.md lists them.

use std::process::Command;

/// Crates that do not build for `wasm32-unknown-unknown`, or that a browser has no use for.
const DESKTOP_ONLY: &[&str] = &[
    "atomics",
    "syntect",
    "onig",
    "onig_sys",
    "two-face",
    "alacritty_terminal",
    "cpal",
    "rfd",
    "notify",
    "ureq",
    "fontique",
    "fontconfig-parser",
    "yeslogic-fontconfig-sys",
    "ring",
    "tempfile",
    "dirs",
    "libc",
];

fn tree(args: &[&str]) -> String {
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["tree", "--locked", "-p", "atomics-ui", "--edges", "normal,build", "--prefix", "none"])
        .args(["--format", "{p} {f}"])
        .args(args)
        .output()
        .expect("cargo tree runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(output.stdout).expect("cargo tree prints UTF-8")
}

fn assert_clean(tree: &str, target: &str) {
    for package in DESKTOP_ONLY {
        let prefix = format!("{package} v");
        assert!(!tree.lines().any(|line| line.starts_with(&prefix)), "{target}: atomics-ui pulls in {package}");
    }
    let core = tree.lines().find(|line| line.starts_with("atomics-core v")).expect("atomics-ui reads atomics-core");
    let features: Vec<_> = core.split_whitespace().last().unwrap_or("").split(',').collect();
    assert!(!features.contains(&"native"), "{target}: atomics-core must be used without `native`: {core}");
}

#[test]
fn web_ui_crate_pulls_no_desktop_only_dependency() {
    assert_clean(&tree(&[]), "host target");
    assert_clean(&tree(&["--target", "wasm32-unknown-unknown"]), "wasm32");
}

#[test]
fn the_check_catches_a_desktop_dependency() {
    // Guard the guard: `atomics` itself pulls in several of the listed crates.
    let output = Command::new(env!("CARGO"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["tree", "--locked", "-p", "atomics", "--edges", "normal,build", "--prefix", "none"])
        .output()
        .expect("cargo tree runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let desktop = String::from_utf8(output.stdout).unwrap();
    for package in ["syntect", "alacritty_terminal", "cpal", "rfd", "notify", "ureq"] {
        assert!(desktop.lines().any(|line| line.starts_with(&format!("{package} v"))), "desktop lacks {package}");
    }
}
