//! `atomics --connect vm` (`[client] connect = "vm"`): the window as a client of the ATOMICS host in this
//! Mac's VM (docs/nixos-vm.md). Each dial makes sure of what that needs, in order: the VM is running
//! (only the Start button starts it), the bundle's forward from this Mac's loopback to the guest's is up
//! (`bin/atomics-vm-host-forward`, held as a child of this window), and this window is paired with the
//! host. Pairing happens once: the host mints a token over Lima's ssh (`atomics ctl pair_client` in the
//! guest), and the token is kept in this Mac's keychain; a token the host no longer takes is forgotten
//! and the window pairs again.

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use zeroize::Zeroizing;

use super::link::Endpoint;
use crate::vm::{BundleInfo, Status};

/// The keychain item that holds this Mac's token for a VM's host, by instance.
pub const KEYCHAIN_SERVICE: &str = "atomics-vm-host-v1";

/// What the window shows of the VM while it is not connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub instance: String,
    pub status: Status,
    /// What the last dial was doing or why it stopped.
    pub note: Option<String>,
    /// A start is under way.
    pub starting: bool,
}

pub struct Gate {
    info: Option<BundleInfo>,
    install_dir: std::path::PathBuf,
    shared: Arc<Mutex<Shared>>,
}

#[derive(Default)]
struct Shared {
    status: Option<Status>,
    note: Option<String>,
    starting: bool,
    forward: Option<Child>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(mut forward) = self.forward.take() {
            let _ = forward.kill();
            let _ = forward.wait();
        }
    }
}

impl Gate {
    pub fn new() -> Self {
        let (install_dir, info) = match BundleInfo::installed() {
            Some((dir, info)) => (dir, Some(info)),
            None => (crate::vm::installed_dir(), None),
        };
        Self { info, install_dir, shared: Arc::default() }
    }

    pub fn instance(&self) -> Option<&str> {
        self.info.as_ref().map(|info| info.instance.as_str())
    }

    pub fn view(&self) -> View {
        let shared = self.shared.lock().unwrap();
        View {
            instance: self.instance().unwrap_or("atomics").to_string(),
            status: shared.status.clone().unwrap_or(Status::Unknown("checking".into())),
            note: shared.note.clone(),
            starting: shared.starting,
        }
    }

    /// Start the VM on a thread of its own (through the QA queue); `done` is called when it is up or failed.
    pub fn start(&self, done: Arc<dyn Fn() + Send + Sync>) {
        let Some(info) = self.info.clone() else { return };
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.starting {
                return;
            }
            shared.starting = true;
            shared.status = Some(Status::Starting);
            shared.note = Some(format!("Starting the VM {}…", info.instance));
        }
        let shared = self.shared.clone();
        let _ = std::thread::Builder::new().name("atomics-vm-start".into()).spawn(move || {
            let result = crate::vm::start(&info.instance);
            {
                let mut shared = shared.lock().unwrap();
                shared.starting = false;
                shared.note = result.err().map(|e| format!("{e:#}"));
                shared.status = Some(crate::vm::status(&info.instance, false));
            }
            done();
        });
    }

    /// A dial's resolver: everything the host needs, then its endpoint. Runs on the link's thread.
    pub fn resolver(&self) -> super::Resolve {
        let info = self.info.clone();
        let install_dir = self.install_dir.clone();
        let shared = self.shared.clone();
        Box::new(move || {
            let result = resolve(info, &install_dir, &shared);
            if let Err(error) = &result {
                // A VM that is not running or not set up says so itself; what failed past that is noted.
                let mut shared = shared.lock().unwrap();
                if shared.status == Some(Status::Running) {
                    shared.note = Some(format!("{error:#}"));
                }
            }
            result
        })
    }

    /// The host refused the token: forget it, so the next dial pairs again.
    pub fn forget_token(&self) {
        if let Some(info) = &self.info {
            let _ = crate::relay::Keychain::current().remove_item(KEYCHAIN_SERVICE, &info.instance);
        }
    }
}

fn set(shared: &Mutex<Shared>, status: Status, note: Option<String>) {
    let mut shared = shared.lock().unwrap();
    if !shared.starting {
        shared.status = Some(status);
    }
    shared.note = note;
}

fn resolve(info: Option<BundleInfo>, install_dir: &std::path::Path, shared: &Mutex<Shared>) -> Result<Endpoint> {
    let Some(info) = info else {
        set(shared, Status::NotSetUp, None);
        bail!(
            "no ATOMICS VM is set up on this Mac: build it with scripts/macos-vm-build.sh --host and run atomics vm setup"
        );
    };
    let Some(port) = info.host.as_ref().map(|host| host.port) else {
        set(shared, Status::NotSetUp, None);
        bail!("the VM's bundle has no ATOMICS host: rebuild it with scripts/macos-vm-build.sh --host");
    };
    let status = crate::vm::status(&info.instance, false);
    let running = status == Status::Running;
    set(shared, status.clone(), None);
    if !running {
        bail!("the VM {} is {}", info.instance, status.label());
    }
    forward(&info, install_dir, port, shared)?;
    let token = token(&info)?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    Ok(Endpoint { addr, host: addr.to_string(), token })
}

/// The bundle's forward, started unless it runs, and its port answering.
fn forward(info: &BundleInfo, install_dir: &std::path::Path, port: u16, shared: &Mutex<Shared>) -> Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    {
        let mut shared = shared.lock().unwrap();
        let alive = shared.forward.as_mut().is_some_and(|child| matches!(child.try_wait(), Ok(None)));
        if !alive {
            let script = install_dir.join("bin/atomics-vm-host-forward");
            let child = Command::new("/bin/sh")
                .arg(&script)
                .arg(port.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .with_context(|| format!("starting {}", script.display()))?;
            shared.forward = Some(child);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return Ok(());
        }
        let mut shared = shared.lock().unwrap();
        if let Some(child) = shared.forward.as_mut()
            && let Ok(Some(_)) = child.try_wait()
        {
            let mut why = String::new();
            if let Some(mut stderr) = child.stderr.take() {
                use std::io::Read;
                let _ = stderr.read_to_string(&mut why);
            }
            shared.forward = None;
            bail!("the forward to the VM {} ended: {}", info.instance, why.trim());
        }
        drop(shared);
        if Instant::now() > deadline {
            bail!("the forward to the VM {} does not answer on {addr}", info.instance);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The token from the keychain, or a new one the host mints for this Mac, kept there.
fn token(info: &BundleInfo) -> Result<Zeroizing<String>> {
    let keychain = crate::relay::Keychain::current();
    if let Some(token) = keychain.read_item(KEYCHAIN_SERVICE, &info.instance)?.filter(|t| !t.is_empty()) {
        return Ok(Zeroizing::new(token));
    }
    let name = format!("ATOMICS on {}", atomics_core::paths::host_name());
    let request = serde_json::json!({ "cmd": "pair_client", "name": name }).to_string();
    // A login shell in the guest, so `atomics ctl` reaches the host's own socket (ATOMICS_SOCKET).
    let output = crate::vm::guest_shell(info)?
        .args(["bash", "--login", "-c", r#"exec atomics ctl "$1""#, "atomics-pair", &request])
        .stderr(Stdio::piped())
        .output()
        .context("asking the VM's host to pair this Mac")?;
    let answer: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
    let Some(token) = answer["result"]["token"].as_str().filter(|_| output.status.success()) else {
        bail!(
            "the VM's host did not pair this Mac: {}",
            answer["error"]
                .as_str()
                .map_or_else(|| String::from_utf8_lossy(&output.stderr).trim().to_string(), str::to_string)
        );
    };
    let token = Zeroizing::new(token.to_string());
    let label = format!("ATOMICS host in the VM {}", info.instance);
    keychain.write_item(KEYCHAIN_SERVICE, &info.instance, &label, &token)?;
    Ok(token)
}
