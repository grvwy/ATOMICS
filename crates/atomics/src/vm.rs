//! The ATOMICS VM on a Mac (docs/nixos-vm.md): its Lima instance, the Mac bundle built with it, and
//! `atomics vm setup|start|stop|status|check|config`. Everything runs Lima's own `limactl` and the
//! bundle's scripts; nothing here signs in anywhere, and nothing replaces an instance or writes a
//! config file without the user saying so.
//!
//! The bundle (`scripts/macos-vm-build.sh`) names its Lima instance and the VM's user in
//! `atomics-vm.json`, so every command here works on the instance the bundle was built for. An
//! instance `atomics vm setup` did not create (an older one of the same name) is never reused or
//! replaced unless the user confirms it.

mod process;
mod service;
pub mod setup;
pub use service::Monitor;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// What `atomics-vm.json` in the bundle says about the VM it was built with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleInfo {
    pub version: u32,
    /// The Lima instance the bundle's wrappers and forward reach.
    pub instance: String,
    /// The VM's login user.
    pub user: String,
    /// Its home folder in the VM.
    pub home: String,
    /// The Mac account the VM mounts folders of.
    pub mac_user: String,
    /// Where the bundle is installed on the Mac.
    pub install_dir: String,
    /// The ATOMICS host in the VM, when the bundle was built with it (`--host`).
    #[serde(default)]
    pub host: Option<HostInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostInfo {
    /// The protocol server's port on the guest's loopback; the bundle's forward opens the same port
    /// on the Mac's loopback.
    pub port: u16,
}

pub const BUNDLE_INFO: &str = "atomics-vm.json";

impl BundleInfo {
    pub fn read(bundle: &Path) -> Result<Self> {
        let file = bundle.join(BUNDLE_INFO);
        let text = std::fs::read_to_string(&file).with_context(|| {
            format!("reading {} (rebuild the bundle with scripts/macos-vm-build.sh)", file.display())
        })?;
        let info: Self = serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        if info.version != 1 {
            bail!("{} is version {}; this ATOMICS reads version 1", file.display(), info.version);
        }
        if !valid_instance(&info.instance) {
            bail!("{} names an invalid Lima instance", file.display());
        }
        Ok(info)
    }

    /// The bundle installed on this Mac, if any: `ATOMICS_VM_BUNDLE`, else the default folder.
    pub fn installed() -> Option<(PathBuf, Self)> {
        let dir = installed_dir();
        Self::read(&dir).ok().map(|info| (dir, info))
    }
}

/// Where the Mac bundle is installed (the module's default `installDir`).
pub fn installed_dir() -> PathBuf {
    atomics_core::paths::env_override("VM_BUNDLE")
        .map(PathBuf::from)
        .or_else(|| {
            std::fs::read_to_string(record_dir().join("selected-bundle"))
                .ok()
                .map(PathBuf::from)
                .filter(|p| p.join(BUNDLE_INFO).is_file())
        })
        .unwrap_or_else(|| atomics_core::paths::home().join(".local/share/atomics-vm"))
}

pub fn valid_instance(name: &str) -> bool {
    name.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The VM as the UI and `atomics vm status` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No bundle is installed, or its instance does not exist yet.
    NotSetUp,
    Stopped,
    /// `limactl start` is running (here or elsewhere), or the instance is up but its host is not yet.
    Starting,
    Running,
    /// Lima reports something else (`Broken`), or `limactl` could not be asked.
    Unknown(String),
}

impl Status {
    pub fn label(&self) -> &str {
        match self {
            Status::NotSetUp => "not set up",
            Status::Stopped => "stopped",
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Unknown(what) => what,
        }
    }
}

/// `limactl`, found on the login shell's PATH or where Homebrew puts it; a Mac app's own PATH has neither.
pub fn limactl() -> Option<PathBuf> {
    crate::setup::find_program("limactl").or_else(|| {
        ["/opt/homebrew/bin/limactl", "/usr/local/bin/limactl"].iter().map(PathBuf::from).find(|path| path.is_file())
    })
}

fn limactl_command() -> Result<Command> {
    let program = limactl().context("limactl is not installed; install Lima 2.2 or newer (brew install lima)")?;
    let mut command = Command::new(program);
    // limactl asks before some actions on a terminal; here it never may.
    command.env("LIMA_TTY", "0").stdin(Stdio::null());
    Ok(command)
}

/// One instance as `limactl list --json` lists it.
#[derive(Debug, Clone, Deserialize)]
struct Listed {
    name: String,
    status: String,
}

/// Lima's status for `instance`, or `None` when it does not exist.
pub fn lima_status(instance: &str) -> Result<Option<String>> {
    let output = limactl_command()?.args(["list", "--json"]).output().context("running limactl list")?;
    if !output.status.success() {
        bail!("limactl list failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(parse_list(&String::from_utf8_lossy(&output.stdout)).into_iter().find(|l| l.name == instance).map(|l| l.status))
}

fn parse_list(text: &str) -> Vec<Listed> {
    text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect()
}

/// The VM's status for `instance`; `starting` while a start this process made is under way.
pub fn status(instance: &str, starting: bool) -> Status {
    match lima_status(instance) {
        Ok(None) => Status::NotSetUp,
        Ok(Some(_)) if starting => Status::Starting,
        Ok(Some(state)) => match state.as_str() {
            "Running" => Status::Running,
            "Stopped" => Status::Stopped,
            other => Status::Unknown(other.to_lowercase()),
        },
        Err(error) => Status::Unknown(format!("{error:#}")),
    }
}

/// The record `atomics vm setup` keeps of each instance it created, so it reuses those and never
/// takes over one it did not make.
fn record_dir() -> PathBuf {
    atomics_core::paths::atomics_home().join("vm")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    instance: String,
    image: String,
    created_at: u64,
    #[serde(default)]
    bundle: Option<PathBuf>,
}

pub fn created_here(instance: &str) -> bool {
    std::fs::read_to_string(record_dir().join(format!("{instance}.json")))
        .ok()
        .and_then(|text| serde_json::from_str::<Record>(&text).ok())
        .is_some_and(|record| record.instance == instance)
}

fn remember(instance: &str, image: &Path) -> Result<()> {
    let dir = record_dir();
    std::fs::create_dir_all(&dir)?;
    let record = Record {
        instance: instance.to_string(),
        image: image.display().to_string(),
        bundle: None,
        created_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
    };
    std::fs::write(dir.join(format!("{instance}.json")), serde_json::to_string_pretty(&record)?)?;
    Ok(())
}

fn forget(instance: &str) {
    let _ = std::fs::remove_file(record_dir().join(format!("{instance}.json")));
}

/// Runs `command` through `atomics queue --class qa`, so the VM's start and its checks wait for a QA
/// slot on a shared machine. Inherits stdio; returns whether it succeeded.
pub fn queued(command: &[String]) -> bool {
    let socket = atomics_core::paths::socket_path();
    crate::governor::run_queued(&socket, atomics_core::governor::SlotClass::Qa, command) == 0
}

/// Starts the instance (through the QA queue) and waits until Lima reports it running.
pub fn start(instance: &str) -> Result<()> {
    let program = limactl().context("limactl is not installed")?;
    let command = [program.display().to_string(), "start".into(), "--tty=false".into(), instance.into()];
    if !queued(&command) {
        bail!("limactl start {instance} failed; `limactl list` and ~/.lima/{instance}/ha.stderr.log say why");
    }
    Ok(())
}

pub fn stop(instance: &str) -> Result<()> {
    let status = limactl_command()?
        .args(["stop", instance])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running limactl stop")?;
    if !status.success() {
        bail!("limactl stop {instance} failed");
    }
    Ok(())
}

/// `limactl shell` in the guest's home, as the VM's user, with a login shell's PATH.
pub fn guest_shell(info: &BundleInfo) -> Result<Command> {
    let mut command = limactl_command()?;
    command.args(["shell", &format!("--workdir={}", info.home), &info.instance]);
    Ok(command)
}

/// `atomics vm …` on Linux: the VM path is a Mac's.
pub const MACOS_ONLY: &str = "The ATOMICS VM is for macOS: on Linux, ATOMICS and its agents run on the machine itself \
     (docs/nixos-vm.md). Build the VM's image here with `nix build .#atomics-vm-image` for a Mac to use.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lima_lists_one_instance_per_line() {
        let text = concat!(
            r#"{"name":"atomics","status":"Stopped","dir":"/home/user/.lima/atomics","cpus":4}"#,
            "\n",
            r#"{"name":"atomics-host","status":"Running","dir":"/home/user/.lima/atomics-host"}"#,
            "\nnot json\n"
        );
        let listed = parse_list(text);
        assert_eq!(listed.len(), 2);
        assert_eq!((listed[1].name.as_str(), listed[1].status.as_str()), ("atomics-host", "Running"));
    }

    #[test]
    fn the_bundle_names_its_instance_and_user() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"{"version":1,"instance":"atomics-host","user":"atomics","home":"/home/atomics",
            "macUser":"user","installDir":"/Users/user/.local/share/atomics-vm","host":{"port":47800}}"#;
        std::fs::write(dir.path().join(BUNDLE_INFO), text).unwrap();
        let info = BundleInfo::read(dir.path()).unwrap();
        assert_eq!(info.instance, "atomics-host");
        assert_eq!(info.host, Some(HostInfo { port: 47800 }));
        std::fs::write(dir.path().join(BUNDLE_INFO), text.replace("atomics-host", "a b")).unwrap();
        assert!(BundleInfo::read(dir.path()).is_err(), "an instance name a shell would split");
        std::fs::write(dir.path().join(BUNDLE_INFO), text.replace("\"version\":1", "\"version\":2")).unwrap();
        assert!(BundleInfo::read(dir.path()).is_err(), "a newer bundle format");
    }

    #[test]
    fn instance_names_are_limas() {
        assert!(valid_instance("atomics-host"));
        assert!(valid_instance("a.b_c-1"));
        for bad in ["", "a b", "a/b", "a;b", "~x"] {
            assert!(!valid_instance(bad), "{bad}");
        }
    }
}

/// A bundle installed for this instance; the historical default remains supported.
pub fn bundle_for(instance: &str) -> Option<(PathBuf, BundleInfo)> {
    if !valid_instance(instance) {
        return None;
    }
    let record = std::fs::read(record_dir().join(format!("{instance}.json")))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok());
    record
        .and_then(|r| r.bundle)
        .and_then(|path| BundleInfo::read(&path).ok().map(|i| (path, i)))
        .or_else(BundleInfo::installed)
        .filter(|(_, i)| i.instance == instance)
}
fn remember_bundle(instance: &str, image: &Path, bundle: &Path) -> Result<()> {
    remember(instance, image)?;
    let path = record_dir().join(format!("{instance}.json"));
    let mut record: Record = serde_json::from_slice(&std::fs::read(&path)?)?;
    record.bundle = Some(bundle.to_path_buf());
    std::fs::write(path, serde_json::to_vec_pretty(&record)?)?;
    Ok(())
}
fn client_lock_path(instance: &str) -> Result<PathBuf> {
    if !valid_instance(instance) {
        bail!("invalid VM instance");
    }
    // One directory per OS user, independent of the ATOMICS home a window uses.
    let uid = unsafe { libc::geteuid() };
    Ok(std::env::temp_dir().join(format!("atomics-vm-clients-{uid}")).join(format!("{instance}.lock")))
}
/// Shared for the lifetime of a VM window, exclusive throughout a deletion.
pub(crate) fn instance_lock(instance: &str, exclusive: bool) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    let path = client_lock_path(instance)?;
    let dir = path.parent().unwrap();
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
        bail!("VM lock directory is not private");
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let mode = if exclusive { libc::LOCK_EX } else { libc::LOCK_SH };
    // SAFETY: flock is applied to our own open descriptor and never waits.
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
        bail!("VM is in use by another ATOMICS window");
    }
    Ok(file)
}
fn in_use(instance: &str) -> bool {
    if !client_lock_path(instance).is_ok_and(|p| p.exists()) {
        return false;
    }
    instance_lock(instance, true).is_err()
}
