//! Where passes come from. `ProfileIssuer` calls the ticket profile's `rendezvous_*` operations on
//! its MCP server as the user's own agent: whoami must be the profile's personal-agent kind and
//! name an owner, or nothing is registered. The calls may take seconds, so they run on a thread of
//! their own (`Desk`), and neither a renewal nor a slow server stalls the tunnel's frames.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use atomics_core::tickets::{
    Cleaner, McpServer, RendezvousRefusal, TicketConfig, Vars, Whoami, result_path, tool_output,
};
use serde_json::Value;
use zeroize::Zeroizing;

use super::keys::DeviceKeys;
use super::{Signal, StatusCell};
use crate::mcp_client::{McpClient, McpError, OAuth};

/// Why an issuer call failed, and what the host does about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueKind {
    /// The profile cannot do it (an operation or result path missing): wait for a change.
    Setup,
    /// The server wants the user to sign in.
    SignIn,
    /// The connection is not a person's own agent.
    Owner,
    /// The host id is not registered, or not yet.
    Unknown,
    /// The host was revoked: only new device keys register again.
    Revoked,
    /// The challenge or its signature was refused.
    Challenge,
    Throttled,
    /// Anything else, a transport failure included: retried after a backoff.
    Failed,
}

/// The message is cleaned of every credential the call knew.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueError {
    pub kind: IssueKind,
    pub message: String,
}

impl IssueError {
    pub fn new(kind: IssueKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

/// A pass. Never Debug, cleared when dropped.
pub struct Pass(pub Zeroizing<String>);

/// What registers hosts and sells passes.
pub trait Issuer: Send {
    /// What the desktop calls the service.
    fn label(&self) -> String;
    /// Register the host's public keys as `name`; the host id the service gave it.
    fn register(&mut self, keys: &DeviceKeys, name: &str, major: u32) -> Result<String, IssueError>;
    /// A fresh challenge for a pass.
    fn challenge(&mut self, host_id: &str) -> Result<String, IssueError>;
    /// The pass the signed challenge buys.
    fn pass(&mut self, host_id: &str, challenge: &str, signature: &str) -> Result<Pass, IssueError>;
}

/// The id as it is signed and sent: a UUID lowercase with hyphens, anything else as given.
pub fn canonical_id(id: &str) -> String {
    uuid::Uuid::parse_str(id.trim()).map(|id| id.hyphenated().to_string()).unwrap_or_else(|_| id.trim().to_string())
}

/// A value one thread publishes and others read, as an `Arc` snapshot.
pub struct Published<T>(Mutex<Option<Arc<T>>>);

impl<T> Default for Published<T> {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

impl<T> Published<T> {
    pub fn set(&self, value: Option<Arc<T>>) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let same = match (&*slot, &value) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            *slot = value;
        }
    }

    pub fn get(&self) -> Option<Arc<T>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

const OPERATIONS: [&str; 3] = ["rendezvous_register", "rendezvous_challenge", "rendezvous_pass"];

/// The ticket profile's operations, on a connection of this issuer's own.
pub struct ProfileIssuer {
    profile: Arc<Published<TicketConfig>>,
    oauth: OAuth,
    env: fn(&str) -> Option<String>,
    /// The connection, with the server and token it was made with.
    client: Option<(McpServer, Option<String>, McpClient)>,
}

impl ProfileIssuer {
    pub fn new(profile: Arc<Published<TicketConfig>>, oauth: OAuth) -> Self {
        Self { profile, oauth, env: crate::setup::env_var, client: None }
    }

    #[cfg(test)]
    pub fn with_env(mut self, env: fn(&str) -> Option<String>) -> Self {
        self.env = env;
        self
    }

    fn config(&self) -> Result<Arc<TicketConfig>, IssueError> {
        let setup = |message: String| IssueError::new(IssueKind::Setup, message);
        let config = self.profile.get().ok_or_else(|| {
            setup("no ticket profile is configured: the rendezvous registers through its operations".into())
        })?;
        if let Some(name) = OPERATIONS.into_iter().find(|name| config.operation(name).is_none()) {
            return Err(setup(format!("the {} ticket profile does not map operations.{name}", config.system)));
        }
        if config.operation("whoami").is_none() {
            return Err(setup(format!(
                "the {} ticket profile does not map operations.whoami, which tells whose agent registers the host",
                config.system
            )));
        }
        Ok(config)
    }

    fn cleaner(&self, config: &TicketConfig, more: &[&str]) -> Cleaner {
        let token = self.client.as_ref().and_then(|(_, _, client)| client.oauth_token().map(str::to_string));
        Cleaner::of(config, self.env).with(token).with(more.iter().map(|s| s.to_string()))
    }

    /// Calls `name`'s tool and returns what its result paths read from. `secrets` are values the
    /// arguments hold that no message may show.
    fn call(&mut self, config: &TicketConfig, name: &str, vars: &Vars, secrets: &[&str]) -> Result<Value, IssueError> {
        let operation = config.operation(name).expect("checked by config()");
        let arguments = operation.arguments(vars).map_err(|e| IssueError::new(IssueKind::Setup, e))?;
        let env = self.env;
        let token = config.server.bearer_env().and_then(env);
        if self.client.as_ref().is_none_or(|(server, used, _)| *server != config.server || *used != token) {
            self.client = None;
            let timeout = Duration::from_secs(config.timeout_secs.max(1));
            let bearer = config.server.bearer_env().map(str::to_string);
            let lookup = |name: &str| if bearer.as_deref() == Some(name) { token.clone() } else { env(name) };
            let oauth = config.server.uses_oauth().then_some(&self.oauth);
            match McpClient::connect_with(&config.server, timeout, lookup, oauth) {
                Ok(client) => self.client = Some((config.server.clone(), token.clone(), client)),
                Err(error) => return Err(self.failure(config, error, secrets)),
            }
        }
        let (_, _, client) = self.client.as_mut().expect("connected");
        match client.call_tool(&operation.tool, arguments) {
            Ok(result) => Ok(tool_output(&result)),
            Err(error) => {
                let failure = self.failure(config, error.clone(), secrets);
                if matches!(error, McpError::Transport { .. } | McpError::SignInRequired(_)) {
                    self.client = None;
                }
                Err(failure)
            }
        }
    }

    fn failure(&self, config: &TicketConfig, error: McpError, secrets: &[&str]) -> IssueError {
        let errors = &config.rendezvous.errors;
        let message = self.cleaner(config, secrets).error(&error.to_string());
        let refusal = match &error {
            McpError::SignInRequired(_) => return IssueError::new(IssueKind::SignIn, message),
            McpError::Tool { message, details } => errors.classify(&format!("{message} {details}")),
            McpError::Server(message) => errors.classify(message),
            McpError::Transport { .. } => None,
        };
        let kind = match refusal {
            Some(RendezvousRefusal::Owner) => IssueKind::Owner,
            Some(RendezvousRefusal::Unknown) => IssueKind::Unknown,
            Some(RendezvousRefusal::Revoked) => IssueKind::Revoked,
            Some(RendezvousRefusal::Challenge) => IssueKind::Challenge,
            Some(RendezvousRefusal::Throttled) => IssueKind::Throttled,
            None => IssueKind::Failed,
        };
        IssueError::new(kind, message)
    }

    /// The connection must be a person's own agent: registering as anyone else either fails at the
    /// service or, worse, binds the host to no human.
    fn check_owner(&mut self, config: &TicketConfig) -> Result<(), IssueError> {
        let value = self.call(config, "whoami", &Vars::new(&config.system), &[])?;
        let operation = config.operation("whoami").expect("checked by config()");
        let whoami = Whoami::read(operation, &value)
            .map_err(|e| IssueError::new(IssueKind::Setup, e))?
            .cleaned(&self.cleaner(config, &[]));
        let personal = whoami.kind.trim().eq_ignore_ascii_case(config.remote.personal_agent_kind.trim());
        let owned =
            whoami.owner_id.as_deref().map(str::trim).is_some_and(|owner| !owner.is_empty() && owner != whoami.id);
        if personal && owned {
            return Ok(());
        }
        Err(IssueError::new(
            IssueKind::Owner,
            format!(
                "{} is {} ({}), not a person's own agent with an owner: sign in to {} as your personal agent",
                config.system, whoami.name, whoami.kind, config.system
            ),
        ))
    }

    /// The text at `field`'s result path of `name`, checked by `fits`. The result is never quoted:
    /// it may hold a pass.
    fn field(
        config: &TicketConfig,
        name: &str,
        value: &Value,
        field: &str,
        path: impl Fn(&atomics_core::tickets::FieldPaths) -> &String,
        fits: impl Fn(&str) -> bool,
    ) -> Result<String, IssueError> {
        let operation = config.operation(name).expect("checked by config()");
        let path = path(&operation.result);
        if path.trim().is_empty() {
            return Err(IssueError::new(IssueKind::Setup, format!("operations.{name} must map result {field}")));
        }
        result_path(value, path).filter(|text| fits(text)).ok_or_else(|| {
            IssueError::new(IssueKind::Failed, format!("{} returned no usable {field} at {path}", operation.tool))
        })
    }
}

/// Printable ASCII and not too long: what may go in a header and in a renewal frame.
fn token_like(text: &str) -> bool {
    !text.is_empty() && text.len() <= 4096 && text.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

impl Issuer for ProfileIssuer {
    fn label(&self) -> String {
        self.profile.get().map_or_else(|| "the rendezvous service".into(), |c| c.rendezvous.label().to_string())
    }

    fn register(&mut self, keys: &DeviceKeys, name: &str, major: u32) -> Result<String, IssueError> {
        let config = self.config()?;
        self.check_owner(&config)?;
        let nonce = uuid::Uuid::new_v4().to_string();
        let (signing, noise) = (keys.signing_public(), keys.noise_public());
        let vars = Vars::new(&config.system)
            .with("nonce", Some(&nonce))
            .with("signing_key", Some(&signing))
            .with("noise_key", Some(&noise))
            .with("host_name", Some(name))
            .typed("protocol_major", Value::from(major));
        let value = self.call(&config, "rendezvous_register", &vars, &[])?;
        let id = Self::field(
            &config,
            "rendezvous_register",
            &value,
            "host_id",
            |p| &p.host_id,
            |id| id.len() <= 128 && atomics_core::tickets::is_plain_key(id),
        )?;
        Ok(canonical_id(&id))
    }

    fn challenge(&mut self, host_id: &str) -> Result<String, IssueError> {
        let config = self.config()?;
        let vars = Vars::new(&config.system).with("host_id", Some(host_id));
        let value = self.call(&config, "rendezvous_challenge", &vars, &[])?;
        Self::field(&config, "rendezvous_challenge", &value, "challenge", |p| &p.challenge, token_like)
    }

    fn pass(&mut self, host_id: &str, challenge: &str, signature: &str) -> Result<Pass, IssueError> {
        let config = self.config()?;
        let vars = Vars::new(&config.system)
            .with("host_id", Some(host_id))
            .with("challenge", Some(challenge))
            .with("signature", Some(signature));
        let value = self.call(&config, "rendezvous_pass", &vars, &[challenge, signature])?;
        Self::field(&config, "rendezvous_pass", &value, "pass", |p| &p.pass, token_like)
            .map(|pass| Pass(Zeroizing::new(pass)))
    }
}

/// What the tunnel's thread asks of the issuer's.
pub enum Ask {
    Register { name: String, major: u32 },
    Pass { host_id: String },
}

pub enum Reply {
    Registered(String),
    Pass(Pass),
}

struct Answer {
    id: u64,
    result: Result<Reply, IssueError>,
}

/// The tunnel thread's side of the issuer thread: one question at a time, answers matched by id
/// so a late answer to an abandoned question is dropped.
pub struct Desk {
    asks: mpsc::Sender<(u64, Ask)>,
    answers: mpsc::Receiver<Answer>,
    next: u64,
    pending: Option<u64>,
}

impl Desk {
    /// The issuer thread answers each question and wakes the tunnel (`signal`), and keeps the
    /// status's service label current. It ends when the desk is dropped.
    pub fn spawn(
        mut issuer: Box<dyn Issuer>,
        keys: Arc<DeviceKeys>,
        signal: Arc<Signal>,
        status: Arc<StatusCell>,
    ) -> std::io::Result<Self> {
        let (asks, questions) = mpsc::channel::<(u64, Ask)>();
        let (replies, answers) = mpsc::channel();
        status.update(|s| s.service = issuer.label());
        std::thread::Builder::new().name("atomics-rendezvous-issuer".into()).spawn(move || {
            for (id, ask) in questions {
                let result = match ask {
                    Ask::Register { name, major } => issuer.register(&keys, &name, major).map(Reply::Registered),
                    Ask::Pass { host_id } => buy_pass(issuer.as_mut(), &keys, &host_id).map(Reply::Pass),
                };
                status.update(|s| s.service = issuer.label());
                if replies.send(Answer { id, result }).is_err() {
                    break;
                }
                signal.wake_tunnel();
            }
        })?;
        Ok(Self { asks, answers, next: 0, pending: None })
    }

    /// Ask without waiting; `poll` brings the answer. A question still pending is abandoned.
    pub fn send(&mut self, ask: Ask) {
        self.next += 1;
        self.pending = Some(self.next);
        let _ = self.asks.send((self.next, ask));
    }

    pub fn waiting(&self) -> bool {
        self.pending.is_some()
    }

    /// The answer to the pending question, once it has come.
    pub fn poll(&mut self) -> Option<Result<Reply, IssueError>> {
        while let Ok(answer) = self.answers.try_recv() {
            if Some(answer.id) == self.pending {
                self.pending = None;
                return Some(answer.result);
            }
        }
        None
    }

    /// Ask and wait for the answer; `None` once the rendezvous is stopping.
    pub fn ask(&mut self, ask: Ask, signal: &Signal) -> Option<Result<Reply, IssueError>> {
        self.send(ask);
        loop {
            if signal.stopped() {
                return None;
            }
            match self.answers.recv_timeout(Duration::from_millis(50)) {
                Ok(answer) if Some(answer.id) == self.pending => {
                    self.pending = None;
                    return Some(answer.result);
                }
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Some(Err(IssueError::new(IssueKind::Failed, "the issuer thread ended")));
                }
            }
        }
    }
}

/// A challenge, signed, for a pass. A refused challenge (spent or expired meanwhile) is tried once
/// more with a fresh one.
pub(super) fn buy_pass(issuer: &mut dyn Issuer, keys: &DeviceKeys, host_id: &str) -> Result<Pass, IssueError> {
    let mut attempt = 0;
    loop {
        let challenge = issuer.challenge(host_id)?;
        let signature = keys.sign_challenge(host_id, &challenge);
        match issuer.pass(host_id, &challenge, &signature) {
            Err(error) if error.kind == IssueKind::Challenge && attempt == 0 => attempt += 1,
            other => return other,
        }
    }
}
