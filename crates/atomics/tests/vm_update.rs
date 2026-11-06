//! The macOS command with a Lima stand-in: no VM, Nix, sign-ins or app window is touched.
#![cfg(target_os = "macos")]

#[path = "../../../tests/support.rs"]
mod test_support;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture {
    dir: test_support::TempDir,
    bundle: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = test_support::tempdir().unwrap();
        let bundle = dir.path().join("installed");
        std::fs::create_dir(&bundle).unwrap();
        let metadata = serde_json::json!({
            "version":1,"instance":"atomics-test","user":"atomics","home":"/home/atomics",
            "macUser":"alice","installDir":bundle,"host":{"port":47800},
            "build":{"cpus":4,"memoryGiB":8,"diskGiB":100}
        });
        std::fs::write(bundle.join("atomics-vm.json"), metadata.to_string()).unwrap();
        std::fs::write(
            bundle.join("install.sh"),
            r#"#!/bin/sh
printf 'install %s\n' "$0" >> "$VM_LOG"
case "$0" in */previous/install.sh) exit 0 ;; esac
test "$VM_MODE" != install-fail
"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("config.toml"), "").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        test_support::write_program(bin.join("login-shell"), "#!/bin/sh\nexec /bin/sh -c \"$4\"\n");
        std::fs::create_dir_all(dir.path().join("home/Applications/ATOMICS.app")).unwrap();
        for program in ["open", "osascript"] {
            test_support::write_program(
                bin.join(program),
                "#!/bin/sh\nprintf '%s %s\\n' \"${0##*/}\" \"$*\" >> \"$VM_LOG\"\n",
            );
        }
        test_support::write_program(
            bin.join("limactl"),
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$VM_LOG"
case "$1" in
  list)
    if [ "$2" = --json ]; then
      printf '{"name":"atomics-test","status":"Running"}\n'
    else
      test "$2" = --format && test "$3" = '{{json .Config}}' && test "$4" = atomics-test
      printf '{"cpus":4,"memory":"8GiB","disk":"100GiB","mounts":[]}\n'
    fi ;;
  shell)
    test "$2" = --workdir=/home/atomics && test "$3" = atomics-test
    test "$4" = bash && test "$5" = --login
    if [ "$6" = -s ]; then
      cat > "$VM_BUILD_SCRIPT"
      test "$VM_MODE" != build-fail
      printf '/tmp/atomics-vm-update.fixture\n/nix/store/00000000000000000000000000000000-nixos-system-test\n/nix/store/11111111111111111111111111111111-atomics-vm-mac\n'
    elif [ "$6" = -c ]; then
      # Scripts run in a plain bash that the login shell execs: `exec bash -c "$1" atomics-vm-update SCRIPT`.
      script=$7
      if [ "$7" = 'exec bash -c "$1"' ]; then test "$8" = atomics-vm-update; script=$9; fi
      case "$script" in
        *'atomics ctl'*)
          if [ "$VM_MODE" = busy ]; then
            printf '{"ok":true,"result":{"sessions":[{"status":"needs you","title":"worker"}]}}\n'
          elif [ "$VM_MODE" = late-busy ] && [ -e "$VM_BUILD_SCRIPT" ]; then
            printf '{"ok":true,"result":{"sessions":[],"update_blockers":["shell running"]}}\n'
          elif [ "$VM_MODE" = unavailable ]; then
            exit 1
          else
            printf '{"ok":true,"result":{"sessions":[]}}\n'
          fi ;;
        *'switch-to-configuration'*)
          test "$VM_MODE" != boot-fail
          if [ "$VM_MODE" = changed ]; then printf 'changed\n'; else printf 'unchanged\n'; fi ;;
        'rm -rf -- '*) exit 0 ;;
        *) echo 'unexpected guest script' >&2; exit 1 ;;
      esac
    else exit 1
    fi ;;
  copy)
    test "$VM_MODE" != copy-fail
    test "$2" = -r
    cp -R "$VM_BUNDLE" "$4" ;;
  stop|start) exit 0 ;;
  *) exit 1 ;;
esac
"#,
        );
        Self { dir, bundle }
    }

    fn run(&self, mode: &str, args: &[&str]) -> Output {
        test_support::output(
            Command::new(env!("CARGO_BIN_EXE_atomics"))
                .args(["vm", "update"])
                .args(args)
                .env("HOME", self.dir.path().join("home"))
                .env("SHELL", self.dir.path().join("bin/login-shell"))
                .env("LIMA_HOME", self.dir.path().join("lima"))
                .env("PATH", format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", self.dir.path().join("bin").display()))
                .env("ATOMICS_HOME", self.dir.path().join("state"))
                .env("ATOMICS_SOCKET", self.dir.path().join("absent.sock"))
                .env("ATOMICS_CONFIG", self.dir.path().join("config.toml"))
                .env("ATOMICS_VM_BUNDLE", &self.bundle)
                .env("VM_BUNDLE", &self.bundle)
                .env("VM_LOG", self.dir.path().join("log"))
                .env("VM_BUILD_SCRIPT", self.dir.path().join("build.sh"))
                .env("VM_MODE", mode)
                .stdin(Stdio::null()),
        )
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("log")).unwrap()
    }
}

#[test]
fn a_no_change_run_reinstalls_the_bundle_and_keeps_the_vm_running() {
    let fixture = Fixture::new();
    let output = fixture.run("unchanged", &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        stdout,
        concat!(
            "Source: github:xmagit/ATOMICS/v",
            env!("CARGO_PKG_VERSION"),
            "\n",
            "Building the VM system and Mac bundle (waits for a test slot)…\n",
            "VM system: unchanged\nMac bundle: reinstalled\nVM: not restarted (system unchanged)\n"
        )
    );
    println!("{stdout}");
    let log = fixture.log();
    assert!(log.contains("copy -r atomics-test:"));
    assert!(log.contains("install "));
    assert!(log.contains("osascript -e if application id"));
    assert!(log.contains("open -a "));
    assert!(!log.lines().any(|line| line == "stop atomics-test" || line.starts_with("start ")));
}

#[test]
fn busy_work_is_refused_unless_yes_and_is_checked_again_after_building() {
    for mode in ["busy", "late-busy", "unavailable"] {
        let fixture = Fixture::new();
        let output = fixture.run(mode, &[]);
        assert!(!output.status.success());
        assert!(!fixture.log().contains("switch-to-configuration"));
        assert!(!fixture.log().contains("install "));
    }
    let fixture = Fixture::new();
    let output = fixture.run("busy", &["--yes", "--no-restart"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Continuing with busy work (--yes)"));
}

#[test]
fn build_copy_and_install_failures_leave_the_boot_selection_alone() {
    for mode in ["build-fail", "copy-fail", "install-fail"] {
        let fixture = Fixture::new();
        let output = fixture.run(mode, &["--no-restart"]);
        assert!(!output.status.success(), "{mode}");
        assert!(!fixture.log().contains("switch-to-configuration"), "{mode}");
        if mode == "install-fail" {
            assert!(fixture.log().contains("/previous/install.sh"), "restore the previous bundle");
        }
    }
}

#[test]
fn changed_system_restarts_only_when_requested_and_boot_failure_restores_the_bundle() {
    for args in [vec![], vec!["--no-restart"]] {
        let fixture = Fixture::new();
        let output = fixture.run("changed", &args);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(fixture.log().contains("stop atomics-test"), args.is_empty());
        assert_eq!(fixture.log().contains("start --tty=false atomics-test"), args.is_empty());
        assert_eq!(fixture.log().contains("osascript -e"), args.is_empty());
        assert_eq!(fixture.log().contains("open -a "), args.is_empty());
        assert!(String::from_utf8_lossy(&output.stdout).contains("VM system: changed"));
    }
    let fixture = Fixture::new();
    let output = fixture.run("boot-fail", &[]);
    assert!(!output.status.success());
    assert!(fixture.log().contains("/previous/install.sh"));
    assert!(!fixture.log().contains("stop atomics-test"));
}
