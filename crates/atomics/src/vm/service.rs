//! Local management worker shared by the in-process host and native client. No command here travels
//! over atomics.v1. The UI only clones a snapshot and queues actions.
use super::{BundleInfo, process};
use anyhow::{Context, Result, bail};
use atomics_ui::vm::{Action, Artifact, Capability, Instance, Resources, Snapshot, State};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

const LIMIT: usize = 64 * 1024;
const KEYCHAIN: &str = "atomics-vm-host-v1";

#[derive(Clone)]
pub struct Monitor {
    shared: Arc<Mutex<Snapshot>>,
    tx: mpsc::SyncSender<Action>,
    cancel: Arc<AtomicBool>,
    use_mac: Arc<AtomicBool>,
    input: Arc<Mutex<VecDeque<String>>>,
    owners: Arc<()>,
}
struct Worker {
    shared: Arc<Mutex<Snapshot>>,
    cancel: Arc<AtomicBool>,
    wake: Arc<dyn Fn() + Send + Sync>,
    source: PathBuf,
    instance: String,
    in_use: Option<String>,
    lima: PathBuf,
    binary: PathBuf,
    keychain: crate::relay::Keychain,
    input: Arc<Mutex<VecDeque<String>>>,
}
impl Monitor {
    pub fn start(window: crate::host::Window) -> Option<Self> {
        if cfg!(test) || !window.is_attached() {
            return None;
        }
        Some(Self::new(Arc::new(move || window.request_repaint()), None))
    }
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>, in_use: Option<String>) -> Self {
        let (tx, rx) = mpsc::sync_channel(1);
        let shared = Arc::new(Mutex::new(Snapshot { supported: cfg!(target_os = "macos"), ..Default::default() }));
        let cancel = Arc::new(AtomicBool::new(false));
        let input = Arc::new(Mutex::new(VecDeque::new()));
        let monitor = Self {
            shared: shared.clone(),
            tx,
            cancel: cancel.clone(),
            use_mac: Arc::default(),
            input: input.clone(),
            owners: Arc::new(()),
        };
        if !cfg!(target_os = "macos") || cfg!(test) {
            return monitor;
        }
        // Discovery, including login-shell PATH and keychain metadata, belongs to this thread too.
        let failed = shared.clone();
        let repaint = wake.clone();
        let spawn = std::thread::Builder::new().name("atomics-vm-manager".into()).spawn(move || {
            let source = atomics_core::paths::env_override("VM_BUILD").map(PathBuf::from).unwrap_or_else(|| {
                let cwd = std::env::current_dir().unwrap_or_default();
                let root = cwd.ancestors().find(|p| p.join("scripts/macos-vm-build.sh").is_file()).unwrap_or(&cwd);
                root.join("target/atomics-vm")
            });
            let instance = atomics_core::paths::env_override("VM_INSTANCE")
                .map(|s| s.to_string_lossy().into_owned())
                .or_else(|| in_use.clone())
                .or_else(|| BundleInfo::read(&source.join("bundle")).ok().map(|i| i.instance))
                .or_else(|| BundleInfo::installed().map(|(_, i)| i.instance))
                .unwrap_or_else(|| "atomics-vm".into());
            let mut worker = Worker {
                shared,
                cancel,
                wake,
                source,
                instance,
                in_use,
                lima: super::limactl().unwrap_or_else(|| "limactl".into()),
                binary: std::env::current_exe().unwrap_or_else(|_| "atomics".into()),
                keychain: crate::relay::Keychain::current(),
                input,
            };
            worker.refresh();
            loop {
                match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(action) => {
                        worker.cancel.store(false, Ordering::Release);
                        worker.change(|s| {
                            s.error = None;
                            s.log.clear();
                            s.busy = Some(action_name(&action).into());
                        });
                        let result = worker.perform(action);
                        worker.cancel.store(false, Ordering::Release);
                        worker.change(|s| {
                            s.error = result.err().map(|e| format!("{e:#}"));
                            s.busy = None;
                        });
                        worker.refresh();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => worker.refresh(),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        if let Err(error) = spawn {
            failed.lock().unwrap().error = Some(format!("Starting VM worker: {error}"));
            repaint();
        }
        monitor
    }
    pub fn take_use_mac(&self) -> bool {
        self.use_mac.swap(false, Ordering::AcqRel)
    }
}
impl Capability for Monitor {
    fn snapshot(&self) -> Snapshot {
        self.shared.lock().unwrap().clone()
    }
    fn send(&self, action: Action) {
        if let Action::SubmitCode(code) = action {
            if self.shared.lock().unwrap().signing_in
                && code.len() <= 256
                && code.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.~#/-".contains(&b))
            {
                let mut input = self.input.lock().unwrap();
                if input.is_empty() {
                    input.push_back(format!("{}\n", serde_json::json!({"code": code})));
                }
            }
            return;
        }
        if matches!(action, Action::Cancel) {
            self.cancel.store(true, Ordering::Release);
            return;
        }
        if matches!(action, Action::UseMac) {
            self.use_mac.store(true, Ordering::Release);
            return;
        }
        let mut view = self.shared.lock().unwrap();
        if view.busy.is_some() || !view.supported {
            return;
        }
        let label = action_name(&action).to_string();
        if self.tx.try_send(action).is_ok() {
            view.busy = Some(label);
        }
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        if Arc::strong_count(&self.owners) == 1 {
            self.cancel.store(true, Ordering::Release);
        }
    }
}
fn action_name(action: &Action) -> &'static str {
    match action {
        Action::Select { .. } | Action::Refresh => "Checking VM",
        Action::Build(_) => "Building image (test queue)",
        Action::Setup => "Setting up VM",
        Action::Start => "Starting VM (QA queue)",
        Action::Stop => "Stopping VM",
        Action::Resize { .. } => "Resizing VM",
        Action::Delete { .. } => "Deleting VM",
        Action::OpenWindow => "Opening VM window",
        Action::SignIn(_) => "Waiting for sign-in in the guest",
        Action::UseMac => "Use this Mac",
        Action::Cancel => "Cancelling",
        Action::SubmitCode(_) => "Sending one-time code",
    }
}
impl Worker {
    fn change(&self, change: impl FnOnce(&mut Snapshot)) {
        change(&mut self.shared.lock().unwrap());
        (self.wake)();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(&self.lima);
        command.env("LIMA_TTY", "0");
        if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
            command.env("PATH", path);
        }
        command
    }
    fn queued(&self, class: &str, program: &Path, args: &[String]) -> Command {
        let mut command = Command::new(&self.binary);
        command.args(["queue", "--class", class, "--"]).arg(program).args(args);
        if let Some(path) = crate::setup::SHELL_PATH.as_ref() {
            command.env("PATH", path);
        }
        command
    }
    fn run(&self, command: &mut Command, live: bool, timeout: Duration) -> Result<String> {
        let mut text = String::new();
        let status = process::run(command, &self.cancel, timeout, |part| {
            append(&mut text, part);
            if live {
                self.change(|s| append(&mut s.log, part));
            }
        })?;
        if !status.success() {
            bail!("VM operation failed ({status}): {}", text.trim());
        }
        Ok(text)
    }
    fn list(&self) -> Result<Vec<Instance>> {
        let text = self.run(self.command().args(["list", "--json"]), false, Duration::from_secs(15))?;
        Ok(text
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|v| {
                let name = v["name"].as_str()?.to_string();
                Some(Instance {
                    in_use: self.in_use.as_ref() == Some(&name) || super::in_use(&name),
                    name,
                    state: match v["status"].as_str()? {
                        "Running" => State::Running,
                        "Stopped" => State::Stopped,
                        "Starting" => State::Starting,
                        other => State::Unknown(other.into()),
                    },
                    resources: Resources {
                        cpus: v["cpus"].as_u64().unwrap_or(0) as u32,
                        memory_gib: v["memory"].as_u64().unwrap_or(0).div_ceil(1 << 30),
                        disk_gib: v["disk"].as_u64().unwrap_or(0).div_ceil(1 << 30),
                    },
                })
            })
            .collect())
    }
    fn bundle(&self) -> Option<(PathBuf, BundleInfo)> {
        super::bundle_for(&self.instance).or_else(|| {
            let path = self.source.join("bundle");
            BundleInfo::read(&path).ok().filter(|i| i.instance == self.instance).map(|i| (path, i))
        })
    }
    fn refresh(&mut self) {
        let listed = self.list();
        let built = BundleInfo::read(&self.source.join("bundle")).ok();
        let image = artifact(&self.source.join("atomics-vm.raw"), &self.source);
        let bundle = artifact(&self.source.join("bundle/atomics-vm.json"), &self.source);
        let installed = super::bundle_for(&self.instance);
        let outdated = installed.as_ref().is_some_and(|(path, _)| {
            !socket_forward(path)
                || (built.as_ref().is_some_and(|i| i.instance == self.instance)
                    && std::fs::read(path.join("build.json")).ok()
                        != std::fs::read(self.source.join("build.json")).ok())
        });
        let pairing = self
            .keychain
            .contains_item(KEYCHAIN, &self.instance)
            .map(|yes| if yes { "paired" } else { "not paired" }.to_string())
            .unwrap_or_else(|e| e.to_string());
        self.change(|s| {
            s.source = self.source.display().to_string();
            s.instance = self.instance.clone();
            s.image = image;
            s.bundle = bundle;
            s.outdated = outdated;
            s.installed = installed.as_ref().map(|(path, _)| artifact(&path.join("atomics-vm.json"), path));
            s.defaults = defaults();
            s.paired = pairing;
            s.can_use_mac = self.in_use.is_some();
            match listed {
                Ok(instances) => s.instances = instances,
                Err(error) => s.error = Some(format!("{error:#}")),
            }
            s.host = match installed.as_ref().and_then(|(_, i)| i.host.as_ref()) {
                Some(_) => "waiting for guest check",
                None => "not configured",
            }
            .into();
        });
        // Checks are read-only and quiet. A stopped VM never gets a shell or an automatic start.
        let running = self.shared.lock().unwrap().selected().is_some_and(|i| i.state == State::Running);
        if running {
            if let Some((_, info)) = self.bundle() {
                self.check_sign_ins(&info);
            }
        } else {
            self.change(|s| {
                s.sign_ins.clear();
                s.sign_in_note = Some("Start the VM to check sign-ins.".into());
            });
        }
    }
    fn perform(&mut self, action: Action) -> Result<()> {
        if let Action::Select { source, instance } = action {
            if !super::valid_instance(&instance) {
                bail!("Use a Lima instance name: letters, digits, dots, underscores and hyphens");
            }
            self.source = atomics_core::paths::expand(&source);
            self.instance = instance;
            return Ok(());
        }
        if !super::valid_instance(&self.instance) {
            bail!("Invalid instance name");
        }
        match action {
            Action::Refresh => Ok(()),
            Action::Build(resources) => {
                validate_resources(&resources)?;
                let root =
                    self.source.ancestors().find(|path| path.join("scripts/macos-vm-build.sh").is_file()).context(
                        "Choose a build folder inside an ATOMICS checkout, such as <checkout>/target/atomics-vm",
                    )?;
                let script = root.join("scripts/macos-vm-build.sh");
                let args = vec![
                    "--host".into(),
                    "--instance".into(),
                    self.instance.clone(),
                    "--out".into(),
                    self.source.display().to_string(),
                    "--cpus".into(),
                    resources.cpus.to_string(),
                    "--memory-gib".into(),
                    resources.memory_gib.to_string(),
                    "--disk-gib".into(),
                    resources.disk_gib.to_string(),
                ];
                self.run(
                    self.queued("test", &script, &args).env("ATOMICS_VM_BUILD_QUEUED", "1"),
                    true,
                    Duration::from_secs(14400),
                )?;
                Ok(())
            }
            Action::Setup => self.setup(),
            Action::Start => self.start(),
            Action::Stop => {
                self.run(self.command().args(["stop", &self.instance]), true, Duration::from_secs(120))?;
                Ok(())
            }
            Action::Resize { resources, stop_first } => self.resize(resources, stop_first),
            Action::Delete { confirmation } => self.delete(&confirmation),
            Action::OpenWindow => {
                let (bundle, _) = self.bundle().context("Set up this VM first")?;
                let mut command = Command::new(&self.binary);
                command
                    .args(["--connect", "vm"])
                    .env("ATOMICS_VM_BUNDLE", bundle)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut child = command.spawn().context("opening VM window")?;
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(())
            }
            Action::SignIn(_) => bail!("This guest needs in-app onboarding support"),
            Action::Cancel | Action::UseMac | Action::Select { .. } | Action::SubmitCode(_) => Ok(()),
        }
    }
    fn start(&self) -> Result<()> {
        let args = vec!["start".into(), "--tty=false".into(), self.instance.clone()];
        self.change(|s| {
            if let Some(i) = s.instances.iter_mut().find(|i| i.name == self.instance) {
                i.state = State::Starting;
            }
        });
        self.run(&mut self.queued("qa", &self.lima, &args), true, Duration::from_secs(600))?;
        Ok(())
    }
    fn setup(&self) -> Result<()> {
        if !cfg!(test) {
            super::setup::check_mac()?;
        }
        let bundle = self.source.join("bundle");
        let info = BundleInfo::read(&bundle)?;
        if info.instance != self.instance {
            bail!("This bundle is for {}. Build it for {} first.", info.instance, self.instance);
        }
        let image = std::fs::canonicalize(self.source.join("atomics-vm.raw")).context("Build the image first")?;
        let existing = self.list()?.into_iter().find(|i| i.name == self.instance);
        if existing.is_some() && !super::created_here(&self.instance) {
            bail!("This instance was not created by ATOMICS setup. Choose another name; it was kept.");
        }
        if !socket_forward(&bundle) {
            bail!("The bundle has the old port-only forward. Build or update the image first.");
        }
        let installed = super::record_dir().join("bundles").join(&self.instance);
        // Copying just this instance's bundle never replaces the user's default bundle or config.
        copy_bundle(&bundle, &installed)?;
        if self.source.join("build.json").is_file() {
            std::fs::copy(self.source.join("build.json"), installed.join("build.json"))?;
        }
        if self.source.join("build-info.json").is_file() {
            std::fs::copy(self.source.join("build-info.json"), installed.join("build-info.json"))?;
        }
        if existing.is_none() {
            let location = format!(".images[0].location = {}", serde_json::to_string(&image.display().to_string())?);
            self.run(
                self.command()
                    .args(["create", "--tty=false", &format!("--name={}", self.instance), "--set", &location])
                    .arg(installed.join("lima.yaml")),
                true,
                Duration::from_secs(120),
            )?;
        }
        super::remember_bundle(&self.instance, &image, &installed)?;
        std::fs::write(super::record_dir().join("selected-bundle"), installed.as_os_str().as_encoded_bytes())?;
        if !existing.is_some_and(|i| i.state == State::Running) {
            self.start()?;
        }
        Ok(())
    }
    fn resize(&self, resources: Resources, stop_first: bool) -> Result<()> {
        validate_resources(&resources)?;
        let instance =
            self.list()?.into_iter().find(|i| i.name == self.instance).context("This instance no longer exists")?;
        if resources.disk_gib < instance.resources.disk_gib {
            bail!("The disk can only grow (currently {} GiB)", instance.resources.disk_gib);
        }
        if instance.state != State::Stopped {
            if !stop_first || instance.state != State::Running {
                bail!("Stop the VM before resizing it");
            }
            self.run(self.command().args(["stop", &self.instance]), true, Duration::from_secs(120))?;
        }
        if self.list()?.iter().find(|i| i.name == self.instance).is_none_or(|i| i.state != State::Stopped) {
            bail!("The VM is not stopped; no size changed");
        }
        self.run(
            self.command().args([
                "edit",
                "--tty=false",
                &self.instance,
                "--cpus",
                &resources.cpus.to_string(),
                "--memory",
                &resources.memory_gib.to_string(),
                "--disk",
                &resources.disk_gib.to_string(),
            ]),
            true,
            Duration::from_secs(120),
        )?;
        Ok(())
    }
    fn delete(&self, confirmation: &str) -> Result<()> {
        if confirmation != self.instance {
            bail!("Type the instance name exactly before deleting");
        }
        if self.in_use.as_ref() == Some(&self.instance) {
            bail!("This window is using that VM");
        }
        let _unused =
            super::instance_lock(&self.instance, true).context("An ATOMICS window is using that VM; close it first")?;
        if let Some(instance) = self.list()?.into_iter().find(|i| i.name == self.instance) {
            if instance.state != State::Stopped {
                self.run(self.command().args(["stop", &self.instance]), true, Duration::from_secs(120))?;
            }
            self.run(self.command().args(["delete", "--force", &self.instance]), true, Duration::from_secs(120))?;
        }
        self.keychain
            .remove_item(KEYCHAIN, &self.instance)
            .context("VM deleted, but its pairing could not be removed; unlock the keychain and retry")?;
        super::forget(&self.instance);
        Ok(())
    }
    fn check_sign_ins(&self, info: &BundleInfo) {
        let mut text = String::new();
        let result = process::run(
            self.command().args([
                "shell",
                &format!("--workdir={}", info.home),
                &info.instance,
                "atomics-onboard",
                "--check",
            ]),
            &self.cancel,
            Duration::from_secs(90),
            |part| append(&mut text, part),
        );
        let items = parse_sign_ins(&text);
        self.change(|s| {
            s.host = items
                .iter()
                .find(|i| i.name == "host")
                .map_or("not checked".into(), |i| format!("{} — {}", i.state, i.label));
            s.sign_in_note = if result.is_err() || items.is_empty() {
                Some("Could not check sign-ins in the guest. Refresh to retry.".into())
            } else {
                None
            };
            s.sign_ins = items;
        });
    }
}
fn append(text: &mut String, part: &str) {
    text.push_str(part);
    if text.len() > LIMIT {
        let mut cut = text.len() - LIMIT;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text.drain(..cut);
    }
}
pub(super) fn parse_sign_ins(text: &str) -> Vec<atomics_ui::vm::SignIn> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let state = words.next()?;
            if !["ok", "missing", "skipped"].contains(&state) {
                return None;
            }
            let name = words.next()?;
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
                return None;
            }
            Some(atomics_ui::vm::SignIn {
                state: state.into(),
                name: name.into(),
                label: words.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}
fn validate_resources(r: &Resources) -> Result<()> {
    if r.cpus == 0 || r.memory_gib == 0 || r.disk_gib == 0 {
        bail!("CPU, memory and disk must be positive whole numbers");
    }
    Ok(())
}
fn defaults() -> Resources {
    let cpus = std::thread::available_parallelism().map_or(4, |n| n.get() as u32);
    #[cfg(target_os = "macos")]
    let memory = {
        let mut value: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: a correctly sized output buffer for this read-only sysctl.
        if unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&mut value as *mut u64).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            value >> 30
        } else {
            16
        }
    };
    #[cfg(not(target_os = "macos"))]
    let memory = 16;
    Resources { cpus: (cpus / 2).max(1), memory_gib: (memory / 2).max(1), disk_gib: 100 }
}
fn artifact(file: &Path, dir: &Path) -> Artifact {
    let Ok(meta) = file.metadata() else { return Artifact::default() };
    let record: serde_json::Value = std::fs::read(dir.join("build-info.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let version = record["version"].as_str().unwrap_or("legacy build (version not recorded)").to_string();
    let date = record["builtAt"].as_str().map(str::to_string).unwrap_or_else(|| {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or("unknown date".into(), |d| format!("built at Unix {}", d.as_secs()))
    });
    Artifact { built: true, version, date }
}
fn socket_forward(bundle: &Path) -> bool {
    std::fs::read_to_string(bundle.join("bin/atomics-vm-host-forward"))
        .is_ok_and(|s| s.contains("absolute socket path"))
}
fn copy_bundle(source: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            copy_bundle(&entry.path(), &dest.join(entry.file_name()))?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), dest.join(entry.file_name()))?;
        } else {
            bail!("Bundle contains a link or special file: {}", entry.path().display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn script(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn worker(root: &Path, running: bool) -> Worker {
        let state = root.join("state");
        std::fs::write(&state, if running { "Running" } else { "Stopped" }).unwrap();
        let lima = root.join("limactl");
        script(
            &lima,
            &format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{0}/calls'
case "$1" in
  list) printf '{{"name":"test-vm","status":"%s","cpus":4,"memory":8589934592,"disk":107374182400}}\n' "$(cat '{0}/state')" ;;
  stop) printf Stopped > '{0}/state' ;;
  edit) test "$(cat '{0}/state')" = Stopped ;;
  shell) printf 'ok claude subscription\nmissing codex not signed in\nok private private files\nok host reachable\n'; exit 1 ;;
esac
"#,
                root.display()
            ),
        );
        let binary = root.join("atomics");
        script(&binary, "#!/bin/sh\n[ \"$1\" = queue ] || exit 10\nshift 4\nexec \"$@\"\n");
        Worker {
            shared: Arc::new(Mutex::new(Snapshot::default())),
            cancel: Arc::default(),
            wake: Arc::new(|| {}),
            source: root.join("target/atomics-vm"),
            instance: "test-vm".into(),
            in_use: None,
            lima,
            binary,
            keychain: crate::relay::Keychain::in_folder(&root.join("keychain")),
            input: Arc::default(),
        }
    }
    #[test]
    fn resize_checks_fresh_state_and_never_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let w = worker(dir.path(), true);
        let size = Resources { cpus: 6, memory_gib: 12, disk_gib: 110 };
        assert!(w.resize(size.clone(), false).unwrap_err().to_string().contains("Stop"));
        assert!(
            w.resize(Resources { disk_gib: 99, ..size.clone() }, true).unwrap_err().to_string().contains("only grow")
        );
        assert!(!std::fs::read_to_string(dir.path().join("calls")).unwrap().contains("stop"));
        w.resize(size, true).unwrap();
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(
            calls.contains("stop test-vm\nlist --json\nedit --tty=false test-vm --cpus 6 --memory 12 --disk 110"),
            "{calls}"
        );
    }
    #[test]
    fn deletion_requires_the_typed_name_and_an_unused_instance() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = worker(dir.path(), false);
        assert!(w.delete("wrong").is_err());
        w.in_use = Some(w.instance.clone());
        assert!(w.delete("test-vm").unwrap_err().to_string().contains("using"));
        assert!(!dir.path().join("calls").exists());
    }
    #[test]
    fn a_build_uses_the_test_queue_and_reports_output_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = worker(dir.path(), false);
        std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
        script(
            &dir.path().join("scripts/macos-vm-build.sh"),
            "#!/bin/sh\necho building-image\necho build-failed >&2\nexit 9\n",
        );
        let error = w.perform(Action::Build(Resources::default())).unwrap_err().to_string();
        assert!(error.contains("build-failed"), "{error}");
        assert!(w.shared.lock().unwrap().log.contains("building-image"));
    }
    #[test]
    fn onboarding_checks_accept_missing_exit_status_and_list_extra_steps() {
        let dir = tempfile::tempdir().unwrap();
        let w = worker(dir.path(), true);
        let info = BundleInfo {
            version: 1,
            instance: "test-vm".into(),
            user: "user".into(),
            home: "/home/user".into(),
            mac_user: "user".into(),
            install_dir: String::new(),
            host: None,
        };
        w.check_sign_ins(&info);
        let view = w.shared.lock().unwrap();
        assert_eq!(view.sign_ins.len(), 4);
        assert!(view.sign_in_note.is_none());
        assert!(view.host.starts_with("ok"));
        let extra = parse_sign_ins("noise\nmissing registry Sign in to registry\nskipped host not configured\n");
        assert_eq!(extra.len(), 2);
        assert_eq!(extra[0].name, "registry");
    }
    #[test]
    fn a_cancelled_build_ends_its_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("build");
        script(&script_path, "#!/bin/sh\necho waiting\nsleep 30 &\nwait\n");
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelling = cancel.clone();
        let mut seen = false;
        let start = std::time::Instant::now();
        let result = process::run(&mut Command::new(script_path), &cancel, Duration::from_secs(5), |part| {
            if part.contains("waiting") {
                seen = true;
                cancelling.store(true, Ordering::Release);
            }
        });
        assert!(seen && result.unwrap_err().to_string().contains("Cancelled"));
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn old_port_only_bundles_are_outdated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("bin")).unwrap();
        let script_path = dir.path().join("bin/atomics-vm-host-forward");
        script(&script_path, "#!/bin/sh\necho port only\n");
        assert!(!socket_forward(dir.path()));
        script(&script_path, "#!/bin/sh\n# takes a port or an absolute socket path\n");
        assert!(socket_forward(dir.path()));
    }
}
