//! The embedding page's half of the frame (docs/daemon.md, "The browser client and its embedding"): the
//! boot it posts, the fresh tickets it posts later, and what the frame posts back. Pure, so its rules
//! are tested without a browser.
//!
//! A message counts only when it comes from the configured parent origin, byte for byte, and from
//! the window that framed this page; both are checked before anything in it is read. A boot's
//! ticket and pairing keys are secrets: `Debug` leaves them out, and nothing here prints them.

use std::fmt;

use atomics_core::noise::{self, Pairing};
use atomics_core::theme::{ThemeMode, ThemeTokens};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::keep::Sealed;

/// The embed protocol's version, every message's `v`.
pub const VERSION: u64 = 1;
/// Frame to parent: the frame has loaded and listens; it asks for its boot.
pub const READY: &str = "atomics.ready";
/// Frame to parent: the frame took its boot.
pub const BOOTED: &str = "atomics.booted";
/// Frame to parent: the frame needs a fresh connect ticket, to reconnect.
pub const WANTS_TICKET: &str = "atomics.ticket";
/// Frame to parent: the host accepted this device, the codes having matched.
pub const PAIRED: &str = "atomics.paired";
/// Parent to frame: the boot.
pub const BOOT: &str = "atomics.embed.boot";
/// Parent to frame: a fresh connect ticket, the answer to [`WANTS_TICKET`].
pub const TICKET: &str = "atomics.embed.ticket";
/// Frame to parent: keep this sealed record for its host, replacing any before it (`crate::keep`).
pub const KEEP: &str = "atomics.keep";
/// Parent to frame: the answer to [`KEEP`], once the record was stored and read back, or could not be.
pub const KEPT: &str = "atomics.embed.kept";
/// Frame to parent: forget the sealed record for this host; only after the host's own sealed refusal.
pub const FORGET: &str = "atomics.forget";
/// The global a hosting page's config script sets to its exact origin.
pub const PARENT_ORIGIN_GLOBAL: &str = "ATOMICS_PARENT_ORIGIN";
/// A browser offers its connect ticket to the broker as a second subprotocol: this, then the ticket.
pub const TICKET_SUBPROTOCOL: &str = "atomics.ticket.";
/// The longest connect ticket taken.
pub const MAX_TICKET_BYTES: usize = 4096;
const MAX_TEXT_CHARS: usize = 200;
const MAX_URL_BYTES: usize = 2048;
const MAX_VIEW_BYTES: usize = 64;

/// The one origin allowed to boot this frame, written as a browser writes `MessageEvent.origin`:
/// scheme and host in lower case, a port only where it is not the scheme's own. `https`, or `http`
/// to this machine's loopback for a local preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentOrigin(String);

impl ParentOrigin {
    /// The configured origin, or `None` when it is not exactly an origin (no path, query, fragment or
    /// user), in which case nothing may boot the frame.
    pub fn parse(configured: &str) -> Option<Self> {
        let text = configured.trim();
        let text = text.strip_suffix('/').unwrap_or(text);
        let (scheme, authority) = text.split_once("://")?;
        let (scheme, authority) = (scheme.to_ascii_lowercase(), authority.to_ascii_lowercase());
        let (host, port) = split_port(&authority)?;
        let own = match scheme.as_str() {
            "https" => 443,
            "http" if loopback(host) => 80,
            _ => return None,
        };
        Some(Self(match port {
            Some(port) if port != own => format!("{scheme}://{host}:{port}"),
            _ => format!("{scheme}://{host}"),
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether a message came from this origin: exactly, byte for byte.
    pub fn sent(&self, origin: &str) -> bool {
        origin == self.0
    }
}

/// A host and an optional port from an authority with no user part; `None` when either is malformed.
fn split_port(authority: &str) -> Option<(&str, Option<u16>)> {
    if authority.contains(['@', '/', '?', '#', '\\']) || authority.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') || host.ends_with(']') => (host, Some(port)),
        _ => (authority, None),
    };
    let port = match port {
        None => None,
        Some(port) if !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit()) => {
            Some(port.parse::<u16>().ok().filter(|p| *p != 0)?)
        }
        Some(_) => return None,
    };
    host_ok(host).then_some((host, port))
}

fn host_ok(host: &str) -> bool {
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return !inner.is_empty() && inner.bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.');
    }
    !host.is_empty()
        && host.len() <= 253
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
}

fn loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Whether `url` is a broker this frame may dial: `wss://`, or `ws://` to this machine's loopback,
/// with no user part and no fragment. It is used exactly as given.
pub fn broker_ok(url: &str) -> bool {
    if url.len() > MAX_URL_BYTES || url.chars().any(|c| c.is_whitespace() || c.is_control()) || url.contains('#') {
        return false;
    }
    let Some((scheme, rest)) = url.split_once("://") else { return false };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let Some((host, _)) = split_port(&rest[..end].to_ascii_lowercase()).map(|(h, p)| (h.to_string(), p)) else {
        return false;
    };
    match scheme {
        "wss" => true,
        "ws" => loopback(&host),
        _ => false,
    }
}

/// A single-use connect ticket. It goes only into the broker upgrade's `Sec-WebSocket-Protocol`.
#[derive(Clone, PartialEq, Eq)]
pub struct Ticket(Zeroizing<String>);

impl fmt::Debug for Ticket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Ticket(hidden)")
    }
}

impl Ticket {
    /// A ticket the broker can take as a subprotocol token (RFC 7230 `tchar`s), at most
    /// [`MAX_TICKET_BYTES`].
    pub fn parse(value: Option<&Value>) -> Option<Self> {
        let text = value?.as_str()?;
        let tchar = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
        ((1..=MAX_TICKET_BYTES).contains(&text.len()) && text.bytes().all(tchar))
            .then(|| Self(Zeroizing::new(text.to_string())))
    }

    /// The subprotocol that presents it.
    pub fn subprotocol(&self) -> Zeroizing<String> {
        Zeroizing::new(format!("{TICKET_SUBPROTOCOL}{}", self.0.as_str()))
    }
}

/// Dark or light, as the hosting page looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Dark,
    Light,
}

/// What the hosting page boots the frame with.
#[derive(Clone)]
pub struct Boot {
    /// The host to reach, as the rendezvous service names it.
    pub host_id: String,
    pub ticket: Ticket,
    /// The broker's absolute WebSocket URL.
    pub broker: String,
    /// The signed-in person, as the hosting page names them.
    pub display_name: String,
    /// The hosting page's theme; a built-in ATOMICS theme of the same name is used, else one of `scheme`.
    pub theme: String,
    pub scheme: Scheme,
    pub locale: String,
    /// What to open on, if the hosting page asked (`board`, `plans`, `approvals`, `diffs`, `terminal`,
    /// `chief`; anything else opens the board).
    pub view: Option<String>,
    /// A ticket's key: open the plan that follows it, or whose task does (`docs/tickets.md`); the board
    /// when none does. The boot's `ticket` is the connect ticket, hence the name.
    pub ticket_key: Option<String>,
    /// Set on a pairing boot: the pairing link's host key and psk.
    pub pairing: Option<Pairing>,
    /// On a pairing boot, how long before the boot the hosting page got the link (`pair.age_ms`):
    /// the link's lifetime runs from then. 0 when the page did not say.
    pub pairing_age_ms: u64,
    /// The hosting page keeps sealed records ([`KEEP`]).
    pub keeps: bool,
    /// The sealed record the hosting page keeps for this host.
    pub sealed: Option<Sealed>,
    /// The page sent a record for this host that could not be read: it is ignored, not refused.
    pub sealed_unreadable: bool,
}

impl fmt::Debug for Boot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Boot")
            .field("host_id", &self.host_id)
            .field("broker", &self.broker)
            .field("theme", &self.theme)
            .field("scheme", &self.scheme)
            .field("locale", &self.locale)
            .field("view", &self.view)
            .field("pairing", &self.pairing.is_some())
            .field("pairing_age_ms", &self.pairing_age_ms)
            .field("keeps", &self.keeps)
            .field("sealed", &self.sealed.is_some())
            .finish_non_exhaustive()
    }
}

impl Boot {
    /// The theme to draw with: the built-in ATOMICS theme the hosting page's theme names, else the
    /// first built-in of its scheme, else the default.
    pub fn theme_tokens(&self) -> ThemeTokens {
        use atomics_core::theme::{DEFAULT, builtin, builtin_names};
        let mode = match self.scheme {
            Scheme::Dark => ThemeMode::Dark,
            Scheme::Light => ThemeMode::Light,
        };
        builtin(&self.theme)
            .or_else(|| builtin_names().filter_map(builtin).find(|tokens| tokens.mode == Some(mode)))
            .or_else(|| builtin(DEFAULT))
            .expect("the default theme is built in")
    }
}

/// What the parent said.
#[derive(Debug, Clone)]
pub enum FromParent {
    Boot(Box<Boot>),
    Ticket {
        host_id: String,
        ticket: Ticket,
    },
    /// The answer to [`KEEP`].
    Kept {
        host_id: String,
        ok: bool,
    },
}

/// Why a message was not taken. It names a field at most, never a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Not the configured parent origin.
    Origin,
    /// The right origin, but not the window that framed this page.
    Window,
    /// Not a message of this protocol, or of another version of it.
    Unknown,
    /// A known message with this field missing or malformed.
    Field(&'static str),
}

/// Read a message event: its origin, whether its source is this page's parent window, and its data.
pub fn read(parent: &ParentOrigin, origin: &str, from_parent: bool, data: &Value) -> Result<FromParent, Refused> {
    if !parent.sent(origin) {
        return Err(Refused::Origin);
    }
    if !from_parent {
        return Err(Refused::Window);
    }
    let kind = data.get("type").and_then(Value::as_str).ok_or(Refused::Unknown)?;
    if data.get("v").and_then(Value::as_u64) != Some(VERSION) {
        return Err(Refused::Unknown);
    }
    match kind {
        BOOT => boot(data).map(|boot| FromParent::Boot(Box::new(boot))),
        TICKET => {
            let host_id = host_id(data)?;
            let ticket = Ticket::parse(data.get("ticket")).ok_or(Refused::Field("ticket"))?;
            Ok(FromParent::Ticket { host_id, ticket })
        }
        KEPT => {
            let host_id = host_id(data)?;
            let ok = data.get("ok").and_then(Value::as_bool).ok_or(Refused::Field("ok"))?;
            Ok(FromParent::Kept { host_id, ok })
        }
        _ => Err(Refused::Unknown),
    }
}

/// The frame's own message of `kind` ([`READY`], [`BOOTED`], [`WANTS_TICKET`]).
pub fn say(kind: &str) -> Value {
    json!({ "type": kind, "v": VERSION })
}

/// [`PAIRED`]: the host accepted this device; `kept`, whether the hosting page keeps a sealed copy of its key.
pub fn say_paired(kept: bool) -> Value {
    json!({ "type": PAIRED, "v": VERSION, "kept": kept })
}

/// [`KEEP`]: the record, which is public data and ciphertext only.
pub fn say_keep(sealed: &Sealed) -> Value {
    json!({ "type": KEEP, "v": VERSION, "host_id": sealed.host_id, "record": sealed.to_json() })
}

/// [`FORGET`].
pub fn say_forget(host_id: &str) -> Value {
    json!({ "type": FORGET, "v": VERSION, "host_id": host_id })
}

fn host_id(data: &Value) -> Result<String, Refused> {
    data.get("host_id")
        .and_then(Value::as_str)
        .filter(|id| noise::host_id_ok(id))
        .map(str::to_string)
        .ok_or(Refused::Field("host_id"))
}

/// A display string: control characters dropped, trimmed, at most [`MAX_TEXT_CHARS`]; empty when absent.
fn text(data: &Value, field: &str) -> String {
    let raw = data.get(field).and_then(Value::as_str).unwrap_or_default();
    let clean: String = raw.chars().filter(|c| !c.is_control()).collect();
    clean.trim().chars().take(MAX_TEXT_CHARS).collect()
}

fn boot(data: &Value) -> Result<Boot, Refused> {
    let host_id = host_id(data)?;
    let ticket = Ticket::parse(data.get("ticket")).ok_or(Refused::Field("ticket"))?;
    let broker = data
        .get("broker")
        .and_then(Value::as_str)
        .filter(|url| broker_ok(url))
        .ok_or(Refused::Field("broker"))?
        .to_string();
    let scheme = match data.get("scheme").and_then(Value::as_str) {
        Some("light") => Scheme::Light,
        _ => Scheme::Dark,
    };
    // A view the frame cannot name is ignored, not refused: the frame opens on its default.
    let view = data.get("view").and_then(Value::as_str).filter(|view| {
        (1..=MAX_VIEW_BYTES).contains(&view.len())
            && view.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    });
    let pairing = match data.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(mode)) if mode == "pair" => Some(pairing(data, &host_id)?),
        Some(_) => return Err(Refused::Field("mode")),
    };
    // Only a page that keeps records sends one, and a record it cannot have meant is left out, never refused.
    let keeps = data.get("keeps").and_then(Value::as_bool) == Some(true);
    let sent = data.get("sealed").filter(|record| keeps && !record.is_null());
    let sealed = sent.and_then(Sealed::read).filter(|record| record.host_id == host_id);
    Ok(Boot {
        keeps,
        sealed_unreadable: sent.is_some() && sealed.is_none(),
        sealed,
        ticket,
        broker,
        display_name: text(data, "display_name"),
        theme: text(data, "theme"),
        scheme,
        locale: text(data, "locale"),
        view: view.map(str::to_string),
        ticket_key: Some(text(data, "ticket_key")).filter(|key| !key.is_empty()),
        pairing_age_ms: if pairing.is_some() { pairing_age_ms(data) } else { 0 },
        pairing,
        host_id,
    })
}

/// A pairing boot's `pair.age_ms`: milliseconds, a finite number of at least 0. Anything else, or
/// none, counts as 0, so the link's time runs from the boot.
fn pairing_age_ms(data: &Value) -> u64 {
    let age = data.get("pair").and_then(|pair| pair.get("age_ms")).and_then(Value::as_f64);
    age.filter(|ms| ms.is_finite() && *ms >= 0.0).map_or(0, |ms| ms as u64)
}

/// A pairing boot's `pair {v, host_noise_pubkey, psk}`, read as the link's fragment would be.
fn pairing(data: &Value, host_id: &str) -> Result<Pairing, Refused> {
    let pair = data.get("pair").ok_or(Refused::Field("pair"))?;
    let version = pair.get("v").and_then(Value::as_u64).map(|v| v.to_string());
    if version.as_deref() != Some(noise::LINK_VERSION) {
        return Err(Refused::Field("pair.v"));
    }
    let key = |field: &'static str| {
        pair.get(field).and_then(Value::as_str).filter(|k| k.len() == 43).ok_or(Refused::Field(field))
    };
    let (host_key, psk) = (key("host_noise_pubkey")?, key("psk")?);
    let fragment = Zeroizing::new(format!("{}.{host_id}.{host_key}.{psk}", noise::LINK_VERSION));
    Pairing::read(&fragment).map_err(|_| Refused::Field("pair"))
}

#[cfg(test)]
mod tests;
