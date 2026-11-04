//! The rendezvous: this host dials out to a rendezvous service, which pairs its socket with the
//! user's browsers and carries their frames (docs/daemon.md, "Rendezvous"). Off unless
//! `[remote.rendezvous] enabled`, and only beside a running protocol server.
//!
//! - `atomics-rendezvous` loads the device keys, registers them, buys a pass, dials, and then pumps
//!   the socket (`tunnel.rs`) until it ends; then it decides from the close code whether to dial
//!   again, wait, or stop. It never takes the `State` lock, and no lock of its is held across I/O.
//! - `atomics-rendezvous-issuer` makes the ticket profile's MCP calls (`issuer.rs`), which may take
//!   seconds, so a renewal never stalls the frames.
//! - Each channel the service opens has a thread of its own (`channel.rs`): a Noise handshake
//!   with a paired device, or a pairing, then one `atomics.v1` client session inside the
//!   encrypted channel, served like a loopback client (`protocol::Sessions`) with the role the
//!   device's allowlist entry gives it. The service sees only ciphertext. Stopping waits for these
//!   threads too, since a session writes its device's last-seen time as it ends.
//! - `devices.rs` keeps the allowlist and the one pairing the desktop may offer.

mod channel;
pub mod devices;
pub mod issuer;
pub mod keys;
mod socket;
#[cfg(test)]
mod tests;
mod tunnel;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use atomics_core::tunnel::close;
use ring::rand::SecureRandom;
use serde::Serialize;

use self::channel::{Crew, Gate};
use self::devices::Devices;
use self::issuer::{Ask, Desk, IssueError, IssueKind, Issuer, Reply};
use self::keys::DeviceKeys;
use self::socket::{DialError, Target};
use self::tunnel::Ended;
use super::Sessions;
use super::link::Waker;

/// Bounds and timings. The defaults suit a service with five-minute passes and a 25 s ping.
#[derive(Debug, Clone)]
pub struct Settings {
    pub url: String,
    /// How the service lists this host.
    pub name: String,
    pub protocol_major: u32,
    /// Channels served at once; the service's own quota may be lower.
    pub max_channels: usize,
    /// What one channel may have queued toward the service, or waiting to be read from it.
    pub channel_buffer: usize,
    /// How long a session waits for room in its channel's queue before it ends as slow.
    pub send_timeout: Duration,
    /// How long one socket write may block before the tunnel counts as broken.
    pub write_timeout: Duration,
    /// For connecting, TLS and the upgrade together.
    pub connect_timeout: Duration,
    /// Silence (not even a ping) after which the tunnel counts as dead.
    pub idle_timeout: Duration,
    /// The longest the pump waits without looking at its timers.
    pub tick: Duration,
    /// A renewal goes out at half the admission's remaining life, within these bounds.
    pub renew: (Duration, Duration),
    /// The jittered backoff between failed attempts: first and longest.
    pub backoff: (Duration, Duration),
    /// A tunnel up this long resets the backoff.
    pub stable: Duration,
    /// How often a setup problem (the profile, a sign-in, the connection's owner) is tried again.
    pub retry: Duration,
    pub throttle_wait: Duration,
    pub quota_wait: Duration,
    /// How long after registering an unknown host id means "not applied yet" rather than "gone".
    pub registration_grace: Duration,
    /// How long a channel has, from `joined`, to send its handshake.
    pub handshake_timeout: Duration,
    /// How long a pairing device has, after the host's answer, to prove the psk and name itself.
    pub pairing_timeout: Duration,
    /// How often the host rekeys what it sends on a channel.
    pub rekey_every: Duration,
}

impl Settings {
    pub fn new(url: &str, name: &str) -> Self {
        Self {
            url: url.trim().to_string(),
            name: host_name(name),
            protocol_major: u32::from(atomics_core::wire::VERSION.major),
            max_channels: 4,
            channel_buffer: 1 << 20,
            send_timeout: Duration::from_secs(5),
            write_timeout: Duration::from_secs(15),
            connect_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(75),
            tick: Duration::from_secs(1),
            renew: (Duration::from_secs(15), Duration::from_secs(150)),
            backoff: (Duration::from_secs(1), Duration::from_secs(60)),
            stable: Duration::from_secs(30),
            retry: Duration::from_secs(30),
            throttle_wait: Duration::from_secs(60),
            quota_wait: Duration::from_secs(300),
            registration_grace: Duration::from_secs(60),
            handshake_timeout: Duration::from_secs(10),
            pairing_timeout: devices::HANDSHAKE_WINDOW,
            rekey_every: Duration::from_secs(3600),
        }
    }
}

/// The configured name, else this machine's, cut to the 80 characters a service may hold.
fn host_name(configured: &str) -> String {
    let name = Some(configured.trim()).filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| {
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is valid for writes of its length; gethostname NUL-terminates within it.
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
        Some(String::from_utf8_lossy(&buf[..end]).trim().to_string())
            .filter(|n| ok && !n.is_empty())
            .unwrap_or_else(|| "ATOMICS".into())
    });
    name.chars().filter(|c| !c.is_control()).take(80).collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Starting,
    Registering,
    Connecting,
    Online,
    /// Waiting before trying again; `note` says why.
    Waiting,
    /// Given up until ATOMICS restarts; `note` says why.
    Stopped,
}

/// What the desktop's indicator and `atomics ctl '{"cmd":"remote"}'` show.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Status {
    pub phase: Phase,
    /// What the profile calls the service.
    pub service: String,
    /// The service's host name (never the URL's path).
    pub host: String,
    pub registered: bool,
    pub host_id: Option<String>,
    pub online: bool,
    pub channels: usize,
    /// Why it waits or stopped, cleaned of credentials.
    pub note: Option<String>,
}

/// The status, behind a lock of its own that no I/O ever holds, and a hook (a repaint) that
/// runs after each change.
pub struct StatusCell {
    status: Mutex<Status>,
    changed: Box<dyn Fn() + Send + Sync>,
}

impl StatusCell {
    pub fn new(changed: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { status: Mutex::default(), changed: Box::new(changed) })
    }

    pub fn get(&self) -> Status {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn update(&self, edit: impl FnOnce(&mut Status)) {
        let changed = {
            let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            let before = status.clone();
            edit(&mut status);
            *status != before
        };
        if changed {
            (self.changed)();
        }
    }
}

/// Stop, and the wakes that end the rendezvous thread's waits: a sleep between attempts, and
/// the pump's read, through the live tunnel's waker.
#[derive(Default)]
pub struct Signal {
    stopped: AtomicBool,
    sleep: Mutex<()>,
    bell: Condvar,
    tunnel: Mutex<Option<Arc<Waker>>>,
}

impl Signal {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // Under the sleep lock, so a sleeper between its check and its wait still hears it.
        let guard = self.sleep.lock().unwrap_or_else(|e| e.into_inner());
        self.bell.notify_all();
        drop(guard);
        self.wake_tunnel();
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Sleep up to `duration`; false once stopping.
    pub fn sleep(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        let mut guard = self.sleep.lock().unwrap_or_else(|e| e.into_inner());
        while !self.stopped() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            guard = self.bell.wait_timeout(guard, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        false
    }

    fn set_tunnel(&self, waker: Option<Arc<Waker>>) {
        *self.tunnel.lock().unwrap_or_else(|e| e.into_inner()) = waker;
    }

    fn wake_tunnel(&self) {
        if let Some(waker) = self.tunnel.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            waker.wake();
        }
    }
}

/// Exponential, with each wait a random 50-100% of its step, so hosts that lost one service do
/// not all dial back at once.
pub struct Backoff {
    first: Duration,
    most: Duration,
    step: Duration,
}

impl Backoff {
    pub fn new((first, most): (Duration, Duration)) -> Self {
        Self { first, most, step: first }
    }

    pub fn next(&mut self) -> Duration {
        let mut byte = [0u8; 1];
        let _ = ring::rand::SystemRandom::new().fill(&mut byte);
        let wait = self.step.mul_f64(0.5 + f64::from(byte[0]) / 510.0);
        self.step = (self.step * 2).min(self.most);
        wait
    }

    pub fn reset(&mut self) {
        self.step = self.first;
    }
}

/// What the rendezvous thread works with.
pub struct Context {
    pub settings: Settings,
    pub sessions: Sessions,
    pub devices: Arc<Devices>,
    pub status: Arc<StatusCell>,
    pub signal: Arc<Signal>,
    pub crew: Arc<Crew>,
}

/// Loads (or makes) the device keys, off the runtime thread.
pub type KeySource = Box<dyn FnOnce() -> anyhow::Result<DeviceKeys> + Send>;

pub struct Rendezvous {
    signal: Arc<Signal>,
    status: Arc<StatusCell>,
    thread: Option<JoinHandle<()>>,
    /// Says the thread has finished, so `stop` knows whether a join would wait.
    done: std::sync::mpsc::Receiver<()>,
    /// The channels' threads.
    crew: Arc<Crew>,
}

/// How long `stop` waits for the threads: they answer a stop at once, except inside a dial, which
/// may take up to the connect timeout.
const STOP_WAIT: Duration = Duration::from_secs(2);

impl Rendezvous {
    pub fn start(
        settings: Settings,
        sessions: Sessions,
        devices: Arc<Devices>,
        issuer: Box<dyn Issuer>,
        keys: KeySource,
        status: Arc<StatusCell>,
    ) -> std::io::Result<Self> {
        let signal = Arc::new(Signal::default());
        let crew = Arc::new(Crew::default());
        let context =
            Context { settings, sessions, devices, status: status.clone(), signal: signal.clone(), crew: crew.clone() };
        let (finished, done) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("atomics-rendezvous".into()).spawn(move || {
            run(context, issuer, keys);
            let _ = finished.send(());
        })?;
        Ok(Self { signal, status, thread: Some(thread), done, crew })
    }

    #[allow(dead_code)] // Read by tests; the desktop holds its own handle.
    pub fn status(&self) -> Status {
        self.status.get()
    }

    /// Close the tunnel and end the thread, then wait for the channels' threads, which end with
    /// the tunnel: their sessions write their devices' last-seen times as they end. A thread still
    /// inside a dial after `STOP_WAIT` is left to see the stop when the dial returns, so quitting
    /// never waits on a network. The issuer thread likewise finishes a call in flight on its own.
    pub fn stop(&mut self) {
        self.signal.stop();
        let deadline = Instant::now() + STOP_WAIT;
        if let Some(thread) = self.thread.take()
            && self.done.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_ok()
        {
            let _ = thread.join();
        }
        self.crew.wait(deadline);
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a close code from the service means for this host.
#[derive(Debug, PartialEq, Eq)]
enum Next {
    /// Dial again after the backoff.
    Again(String),
    /// Wait this long first.
    Wait(Duration, String),
    /// Give up until restarted.
    Stop(String),
}

fn after(ended: Ended, settings: &Settings) -> Option<Next> {
    Some(match ended {
        Ended::Stopped => return None,
        Ended::Revoked(message) => Next::Stop(format!("the service revoked this host: {message}")),
        Ended::Idle => Next::Again(format!("the service sent nothing for {} s", settings.idle_timeout.as_secs())),
        Ended::Broken(why) => Next::Again(why),
        Ended::Closed(code) => match code {
            Some(close::UNAUTHORISED) => {
                Next::Again("the service ended the admission (4401); dialing with a new pass".into())
            }
            Some(close::REVOKED) => {
                Next::Stop("the service revoked this host (4403): it registers again only with new device keys".into())
            }
            Some(close::REPLACED) => {
                Next::Stop("this host connected again elsewhere and replaced this connection (4409)".into())
            }
            Some(close::QUOTA) => Next::Wait(
                settings.quota_wait,
                "the service's quota of hosts or channels is full (4429): close another host".into(),
            ),
            Some(close::HOST_OFFLINE) => Next::Again("the service dropped the tunnel (4404)".into()),
            Some(close::SLOW) => Next::Again("the service closed the tunnel as slow (4408)".into()),
            Some(code @ (close::PROTOCOL_ERROR | close::UNSUPPORTED | close::TOO_BIG)) => {
                Next::Again(format!("the service refused a frame ({code})"))
            }
            Some(code) => Next::Again(format!("the service closed the tunnel ({code})")),
            None => Next::Again("the service closed the tunnel".into()),
        },
    })
}

fn run(ctx: Context, issuer: Box<dyn Issuer>, keys: KeySource) {
    let status = ctx.status.clone();
    let stopped = |note: String| {
        // A host that stopped (revoked, replaced, misconfigured) offers no pairing.
        ctx.devices.set_host(None);
        status.update(|s| {
            s.phase = Phase::Stopped;
            s.online = false;
            s.note = Some(note);
        })
    };
    let target = match Target::parse(&ctx.settings.url) {
        Ok(target) => target,
        Err(error) => return stopped(error),
    };
    status.update(|s| s.host = target.host().to_string());
    let keys = match keys() {
        Ok(keys) => Arc::new(keys),
        Err(error) => return stopped(format!("the device keys could not be read or stored: {error}")),
    };
    ctx.devices.set_key(keys.noise_key());
    let mut desk = match Desk::spawn(issuer, keys.clone(), ctx.signal.clone(), status.clone()) {
        Ok(desk) => desk,
        Err(error) => return stopped(format!("could not start the issuer thread: {error}")),
    };
    let mut backoff = Backoff::new(ctx.settings.backoff);
    let mut host: Option<(String, Instant)> = None;
    while !ctx.signal.stopped() {
        let (host_id, registered) = match host.clone() {
            Some(known) => known,
            None => {
                status.update(|s| s.phase = Phase::Registering);
                let ask = Ask::Register { name: ctx.settings.name.clone(), major: ctx.settings.protocol_major };
                match desk.ask(ask, &ctx.signal) {
                    None => break,
                    Some(Ok(Reply::Registered(id))) => {
                        ctx.devices.set_host(Some(&id));
                        status.update(|s| {
                            s.registered = true;
                            s.host_id = Some(id.clone());
                        });
                        let at = Instant::now();
                        host = Some((id.clone(), at));
                        (id, at)
                    }
                    Some(Ok(Reply::Pass(_))) => continue,
                    Some(Err(error)) => {
                        if !pause(&ctx, &error, &mut backoff) {
                            break;
                        }
                        continue;
                    }
                }
            }
        };
        status.update(|s| s.phase = Phase::Connecting);
        let pass = match desk.ask(Ask::Pass { host_id: host_id.clone() }, &ctx.signal) {
            None => break,
            Some(Ok(Reply::Pass(pass))) => pass,
            Some(Ok(Reply::Registered(_))) => continue,
            Some(Err(error)) => {
                // Right after registering, the service may not have applied it yet; later, the
                // host is gone there, and registering again brings it back.
                if error.kind == IssueKind::Unknown && registered.elapsed() > ctx.settings.registration_grace {
                    host = None;
                    ctx.devices.set_host(None);
                    status.update(|s| s.registered = false);
                }
                if !pause(&ctx, &error, &mut backoff) {
                    break;
                }
                continue;
            }
        };
        let next = match socket::dial(&target, &pass.0, &ctx.settings) {
            Err(DialError::Refused(code)) => Next::Again(format!("the service refused the connection (HTTP {code})")),
            Err(DialError::Failed(why)) => Next::Again(why),
            Ok(socket) => {
                drop(pass);
                let gate = Arc::new(Gate {
                    host_id: host_id.clone(),
                    keys: keys.clone(),
                    devices: ctx.devices.clone(),
                    sessions: ctx.sessions.clone(),
                    handshake_timeout: ctx.settings.handshake_timeout,
                    pairing_timeout: ctx.settings.pairing_timeout,
                    send_timeout: ctx.settings.send_timeout,
                    rekey_every: ctx.settings.rekey_every,
                    crew: ctx.crew.clone(),
                });
                match after(tunnel::run(socket, &ctx, &mut desk, gate, &mut backoff), &ctx.settings) {
                    Some(next) => next,
                    None => break,
                }
            }
        };
        let resumed = match next {
            Next::Again(note) => wait(&ctx, backoff.next(), note),
            Next::Wait(duration, note) => wait(&ctx, duration, note),
            Next::Stop(note) => {
                stopped(note);
                park(&ctx)
            }
        };
        if !resumed {
            break;
        }
    }
    status.update(|s| {
        s.online = false;
        s.channels = 0;
    });
}

/// Wait before trying again, saying why; false once stopping.
fn wait(ctx: &Context, duration: Duration, note: String) -> bool {
    let seconds = duration.as_secs_f64().ceil() as u64;
    ctx.status.update(|s| {
        s.phase = Phase::Waiting;
        s.note = Some(format!("{note}; trying again in {seconds} s"));
    });
    ctx.signal.sleep(duration)
}

/// Until the rendezvous stops: what a revoked or replaced host does.
fn park(ctx: &Context) -> bool {
    while ctx.signal.sleep(Duration::from_secs(3600)) {}
    false
}

/// After an issuer failure: wait as its kind asks; false once stopping.
fn pause(ctx: &Context, error: &IssueError, backoff: &mut Backoff) -> bool {
    let settings = &ctx.settings;
    match error.kind {
        IssueKind::Revoked => {
            ctx.devices.set_host(None);
            ctx.status.update(|s| {
                s.phase = Phase::Stopped;
                s.note = Some(format!("the service revoked this host: {}", error.message));
            });
            park(ctx)
        }
        IssueKind::Setup | IssueKind::SignIn | IssueKind::Owner => wait(ctx, settings.retry, error.message.clone()),
        IssueKind::Throttled => wait(ctx, settings.throttle_wait, error.message.clone()),
        IssueKind::Unknown | IssueKind::Challenge | IssueKind::Failed => {
            wait(ctx, backoff.next(), error.message.clone())
        }
    }
}
