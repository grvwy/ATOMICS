//! `atomics update` from a terminal, the real program through the real queue, with a checkout as the source. What it
//! does with a signed release is tested beside the code: nothing at run time can name another key or release feed.

#[path = "../../../tests/support.rs"]
mod test_support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = test_support::output(
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
            .args(args),
    );
    assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

struct Fixture {
    dir: test_support::TempDir,
    checkout: PathBuf,
    other: PathBuf,
}

impl Fixture {
    /// A checkout of a remote's main with an install script of `text`, a second clone to push from, and a config
    /// that names the checkout.
    fn new(text: &str) -> Fixture {
        let dir = test_support::tempdir().unwrap();
        let (origin, checkout, other) =
            (dir.path().join("origin.git"), dir.path().join("checkout"), dir.path().join("other"));
        git(dir.path(), &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
        git(dir.path(), &["clone", "--quiet", origin.to_str().unwrap(), other.to_str().unwrap()]);
        git(&other, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        std::fs::create_dir_all(other.join("scripts")).unwrap();
        test_support::write_program(other.join("scripts/linux-install.sh"), text);
        std::fs::write(other.join("scripts/macos-app.sh"), text).unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "--quiet", "-m", "First"]);
        git(&other, &["push", "--quiet", "origin", "main"]);
        git(dir.path(), &["clone", "--quiet", origin.to_str().unwrap(), checkout.to_str().unwrap()]);
        let config = format!("[update]\nsource = {:?}\n", checkout.to_str().unwrap());
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Fixture { dir, checkout, other }
    }

    fn push(&self, subject: &str) -> String {
        std::fs::write(self.other.join("new.txt"), subject).unwrap();
        git(&self.other, &["add", "."]);
        git(&self.other, &["commit", "--quiet", "-m", subject]);
        git(&self.other, &["push", "--quiet", "origin", "main"]);
        git(&self.other, &["rev-parse", "HEAD"])
    }

    fn atomics(&self, args: &[&str]) -> Output {
        test_support::output(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .args(args)
                .env("HOME", self.dir.path())
                .env("ATOMICS_HOME", self.dir.path().join("state"))
                .env("ATOMICS_CONFIG", self.dir.path().join("config.toml"))
                .env("ATOMICS_SOCKET", self.dir.path().join("no-host.sock"))
                .env_remove("CARGO_BUILD_JOBS")
                .stdin(Stdio::null()),
        )
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Records its niceness and its arguments.
const RECORDS: &str =
    "#!/bin/sh\nnice > \"$(dirname \"$0\")/../ran\"\nprintf '%s\\n' \"$@\" >> \"$(dirname \"$0\")/../ran\"\n";

/// An update refuses a release whose program does not name its version here, so a change to this line would make
/// every installed copy refuse the release that made it.
#[test]
fn version_names_the_crate_version_first_and_the_build_after_it() {
    let fixture = Fixture::new(RECORDS);
    let output = fixture.atomics(&["--version"]);
    assert!(output.status.success(), "{output:?}");
    let said = stdout(&output);
    let line = said.trim();
    let expected = format!("atomics {} (", env!("CARGO_PKG_VERSION"));
    assert!(line.starts_with(&expected) && line.ends_with(')'), "{line}");
}

#[test]
fn check_says_what_is_new_and_moves_nothing() {
    let fixture = Fixture::new(RECORDS);
    let output = fixture.atomics(&["update", "--check"]);
    assert!(output.status.success(), "{output:?}");
    assert!(stdout(&output).contains("is up to date"), "{output:?}");

    let before = git(&fixture.checkout, &["rev-parse", "HEAD"]);
    fixture.push("Add the thing");
    let output = fixture.atomics(&["update", "--check"]);
    assert!(output.status.success(), "{output:?}");
    let said = stdout(&output);
    assert!(said.contains("is available"), "{said}");
    assert!(said.contains("1 new commit on origin/main; the latest: Add the thing"), "{said}");
    assert_eq!(git(&fixture.checkout, &["rev-parse", "HEAD"]), before);
    assert!(!fixture.checkout.join("ran").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn update_moves_the_checkout_and_runs_its_install_script_at_low_priority_in_the_queue() {
    let fixture = Fixture::new(RECORDS);
    let new = fixture.push("Add the thing");
    let output = fixture.atomics(&["update"]);
    assert!(output.status.success(), "{output:?}");
    let said = stdout(&output);
    assert!(said.contains("Installed "), "{said}");
    assert!(said.contains("The next start of ATOMICS runs it."), "no host listens here: {said}");
    assert_eq!(git(&fixture.checkout, &["rev-parse", "HEAD"]), new);
    let ran = std::fs::read_to_string(fixture.checkout.join("ran")).unwrap();
    assert_eq!(ran.lines().collect::<Vec<_>>(), ["19", "--no-setup"], "{said}");
    let log = std::fs::read_to_string(fixture.dir.path().join("state/update.log")).unwrap();
    assert!(log.contains("atomics queue: no ATOMICS socket"), "it went through the queue: {log}");
}

#[test]
fn a_failed_update_says_why_and_exits_nonzero() {
    let fixture = Fixture::new("#!/bin/sh\necho 'error: it does not build'\nexit 3\n");
    fixture.push("Add the thing");
    let output = fixture.atomics(&["update"]);
    assert!(!output.status.success(), "{output:?}");
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("error: it does not build"), "{said}");
}

#[test]
fn a_checkout_with_changes_of_its_own_is_left_alone() {
    let fixture = Fixture::new(RECORDS);
    fixture.push("Add the thing");
    std::fs::write(fixture.checkout.join("scripts/linux-install.sh"), "#!/bin/sh\n").unwrap();
    std::fs::write(fixture.checkout.join("scripts/macos-app.sh"), "#!/bin/sh\n").unwrap();
    let before = git(&fixture.checkout, &["rev-parse", "HEAD"]);
    let output = fixture.atomics(&["update"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("uncommitted changes"), "{output:?}");
    assert_eq!(git(&fixture.checkout, &["rev-parse", "HEAD"]), before);
}
