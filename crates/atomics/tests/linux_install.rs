#![cfg(target_os = "linux")]

#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn script(path: &Path, text: &str) {
    test_support::write_program(path, text);
}

/// An install that asks nothing: the program opens a setup screen itself, and a person runs `atomics setup` only if they
/// want it from a terminal.
#[test]
fn install_and_update_ask_nothing_and_replace_the_program_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("fresh home");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    let ran = dir.path().join("ran");
    // Anything it runs says so. Setup would also ask, and find nobody to answer.
    script(
        &target.join("fixture-linux-native/release/atomics"),
        &format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 11\n", ran.display()),
    );
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    let install = |args: &[&str]| {
        test_support::output(
            Command::new("sh")
                .arg(&installer)
                .args(args)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join("data"))
                .env("CARGO_TARGET_DIR", &target)
                .env("PATH", &path)
                .stdin(Stdio::null()),
        )
    };
    let mut inode = None;
    // Builds from before this asked for `--no-setup` by name when they updated.
    for args in [&[][..], &[][..], &["--no-setup"][..]] {
        let output = install(args);
        assert!(output.status.success(), "{output:?}");
        let installed = home.join(".local/bin/atomics");
        let next_inode = installed.metadata().unwrap().ino();
        assert_ne!(Some(next_inode), inode, "updates must replace the inode atomically");
        inode = Some(next_inode);
        assert!(home.join("data/applications/atomics.desktop").exists());
        assert!(home.join("data/icons/hicolor/512x512/apps/atomics.png").exists());
        let said = String::from_utf8(output.stdout).unwrap();
        assert!(said.contains("setup screen"), "it says what comes next: {said}");
        assert!(said.contains("export PATH="), "the exact line for a terminal: {said}");
    }
    assert!(!ran.exists(), "nothing was run: {:?}", std::fs::read_to_string(&ran));
    let output = test_support::output(
        Command::new("sh").arg(installer).arg("--uninstall").env("HOME", &home).env("XDG_DATA_HOME", home.join("data")),
    );
    assert!(output.status.success());
    assert!(!home.join(".local/bin/atomics").exists());
}

/// The real `atomics setup`, in a home with a login shell of its own and only the tools a test puts there.
struct SetupHome {
    dir: tempfile::TempDir,
}

impl SetupHome {
    /// With a config that says `mode`, and these programs on PATH.
    fn new(mode: &str, programs: &[&str]) -> SetupHome {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for program in programs {
            script(&bin.join(program), "#!/bin/sh\nexit 0\n");
        }
        script(&bin.join("shell"), "#!/bin/sh\nprintf '__ATOMICS__%s__ATOMICS__' \"$PATH\"\n");
        std::fs::write(dir.path().join("config.toml"), format!("[isolation]\nmode = \"{mode}\"\n")).unwrap();
        SetupHome { dir }
    }

    fn run(&self, args: &[&str], answers: &str) -> std::process::Output {
        let mut child = test_support::TestChild::spawn(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .args(args)
                .env("HOME", self.dir.path())
                .env("PATH", self.dir.path().join("bin"))
                .env("SHELL", self.dir.path().join("bin/shell"))
                .env("ATOMICS_HOME", self.dir.path().join("state"))
                .env("ATOMICS_CONFIG", self.dir.path().join("config.toml"))
                .env("ATOMICS_SOCKET", self.dir.path().join("no-host.sock"))
                .stdin(Stdio::piped()),
            test_support::CHILD_TIMEOUT,
        )
        .unwrap();
        child.child.stdin.take().unwrap().write_all(answers.as_bytes()).unwrap();
        child.wait_output().unwrap()
    }
}

#[test]
fn setup_with_agents_on_this_machine_needs_one_agent_and_nothing_of_sandboxes() {
    let home = SetupHome::new("bare", &["git", "claude"]);
    let output = home.run(&["setup"], "");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(said.contains("sandboxes are not needed"), "{said}");
    assert!(said.contains("codex is not installed"), "it says how to get the other: {said}");
    for word in ["secret-tool", "sbx ", "KVM", "Docker"] {
        assert!(!said.contains(word), "{word} is not in use: {said}");
    }
}

#[test]
fn stopping_setup_reads_as_a_stop_and_it_can_still_ask_for_sandboxes() {
    // No agent yet, so it waits; `q` is a person leaving, not a broken install.
    let home = SetupHome::new("bare", &["git"]);
    let output = home.run(&["setup"], "q\n");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}");
    assert!(said.contains("ATOMICS is installed"), "{said}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("failed"));

    // Sandboxes asked for by name, on a machine whose agents run bare.
    let home = SetupHome::new("bare", &["git", "claude"]);
    let output = home.run(&["sandbox", "setup"], "q\n");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}");
    // What sandboxes need comes first: on Linux that stops at KVM, and on a Mac it reaches sbx.
    assert!(said.contains(if cfg!(target_os = "linux") { "kvm group" } else { "sbx" }), "{said}");
    assert!(said.contains("ATOMICS is installed"), "{said}");

    // Nobody to answer is not the same as stopping: it says so, and a script can tell.
    let output = SetupHome::new("bare", &["git"]).run(&["setup"], "");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("setup input ended"));
}

/// Claude Code's own installer leaves it in a folder many logins do not list, where the window finds it: so does setup.
#[test]
fn setup_finds_what_an_installer_left_in_the_users_own_programs_folder() {
    let home = SetupHome::new("bare", &["git"]);
    let local = home.dir.path().join(".local/bin");
    std::fs::create_dir_all(&local).unwrap();
    script(&local.join("claude"), "#!/bin/sh\nexit 0\n");
    let output = home.run(&["setup"], "");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(!said.contains("prerequisite: claude"), "{said}");
    assert!(said.contains("sandboxes are not needed"), "{said}");
}

#[test]
fn setup_with_sandboxes_in_use_still_checks_what_they_need() {
    let home = SetupHome::new("sandbox", &["git", "claude"]);
    let output = home.run(&["setup"], "q\n");
    let said = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success(), "{said}");
    #[cfg(target_os = "linux")]
    assert!(said.contains("prerequisite: secret-tool"), "{said}");
    assert!(said.contains("ATOMICS fresh-machine setup"), "{said}");
}

#[test]
fn installer_registers_the_atomics_link_scheme() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let bin = dir.path().join("tools");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(target.join("fixture-linux-native/release")).unwrap();
    script(&bin.join("cargo"), "#!/bin/sh\nexit 0\n");
    script(&bin.join("rustc"), "#!/bin/sh\necho 'host: fixture-linux-native'\n");
    script(&bin.join("xdg-mime"), "#!/bin/sh\necho \"$@\" >> \"$MIME_LOG\"\n");
    script(&target.join("fixture-linux-native/release/atomics"), "#!/bin/sh\nexit 0\n");
    let log = dir.path().join("mime.log");
    let installer = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/linux-install.sh");
    let run = |args: &[&str]| {
        test_support::output(
            Command::new("sh")
                .arg(&installer)
                .args(args)
                .env("HOME", &home)
                .env("XDG_DATA_HOME", home.join("data"))
                .env("CARGO_TARGET_DIR", &target)
                .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
                .env("MIME_LOG", &log),
        )
    };
    assert!(run(&[]).status.success());
    let entry = std::fs::read_to_string(home.join("data/applications/atomics.desktop")).unwrap();
    assert!(entry.contains("MimeType=x-scheme-handler/atomics;"), "{entry}");
    let exec = entry.lines().find(|line| line.starts_with("Exec=")).unwrap();
    assert!(exec.ends_with(" %u"), "{exec}");
    assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "default atomics.desktop x-scheme-handler/atomics");
    // Uninstalling registers nothing.
    assert!(run(&["--uninstall"]).status.success());
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}
