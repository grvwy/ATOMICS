//! The device key's second home (docs/decisions/2026-11-07-passkey-sealed-device-key.md). WebKit gives a frame
//! whose origin differs from the top-level one storage that lasts only while the browser runs, so this frame's
//! own store (`web/device-store.js`) forgets every pairing when the browser closes. Where that is so, and the
//! hosting page keeps copies, the key is also sealed under a passkey's PRF output (`web/passkey.js`) and the
//! sealed record handed to the hosting page. The page keeps it in its own storage but cannot open it: the
//! passkey's RP ID is this origin's host. On a cold start that finds this frame's store empty, one tap and the
//! passkey's user verification open it.
//!
//! What the driver decides with: the record as the embed contract carries it, what this browser offers for
//! passkeys, what the glue is asked to do on the next tap and what came of it, and where this frame's key came
//! from, for the pairing screens' Storage line.

use std::fmt;

use atomics_core::noise::{self, PublicKey, Secret};
use serde_json::{Value, json};

use crate::device::Device;

/// The record's version, its `v`.
pub const RECORD_VERSION: u64 = 2;
/// The longest credential ID taken, in base64url characters (1023 bytes, WebAuthn's limit, is 1364).
const MAX_CREDENTIAL: usize = 1400;
/// AES-GCM's 12-byte nonce, and the 32-byte device key with its 16-byte tag, in unpadded base64.
const IV_CHARS: usize = 16;
const SEALED_CHARS: usize = 64;
/// The longest failure text taken from the glue.
const MAX_DETAIL: usize = 120;

/// A device key sealed under a passkey. Everything in it is public or ciphertext.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    pub host_id: String,
    /// The host's Noise static key, bound into the seal's additional data.
    pub host_key: PublicKey,
    /// The passkey's credential ID, unpadded base64url: an unlock asks for exactly this passkey.
    pub credential: String,
    /// The nonce and the sealed key, unpadded base64.
    pub iv: String,
    pub sealed: String,
    /// When it was sealed, in milliseconds since the epoch.
    pub paired_at: u64,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("host_id", &self.host_id)
            .field("paired_at", &self.paired_at)
            .finish_non_exhaustive()
    }
}

fn base64(text: &str, url: bool) -> bool {
    text.bytes().all(|b| b.is_ascii_alphanumeric() || if url { b == b'-' || b == b'_' } else { b == b'+' || b == b'/' })
}

impl Sealed {
    /// A record as the contract carries it: exactly its fields' shapes, or nothing.
    pub fn read(value: &Value) -> Option<Self> {
        let text = |field: &str| value.get(field).and_then(Value::as_str);
        if value.get("v").and_then(Value::as_u64) != Some(RECORD_VERSION) {
            return None;
        }
        let host_id = text("host_id").filter(|id| noise::host_id_ok(id))?;
        let host_key = text("host_key").and_then(PublicKey::parse)?;
        let credential = text("credential").filter(|c| (16..=MAX_CREDENTIAL).contains(&c.len()) && base64(c, true))?;
        let iv = text("iv").filter(|iv| iv.len() == IV_CHARS && base64(iv, false))?;
        let sealed = text("sealed").filter(|s| s.len() == SEALED_CHARS && base64(s, false))?;
        let paired_at = value.get("paired_at").and_then(Value::as_u64)?;
        Some(Self {
            host_id: host_id.to_string(),
            host_key,
            credential: credential.to_string(),
            iv: iv.to_string(),
            sealed: sealed.to_string(),
            paired_at,
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "v": RECORD_VERSION, "host_id": self.host_id, "host_key": self.host_key.text(),
            "credential": self.credential, "iv": self.iv, "sealed": self.sealed, "paired_at": self.paired_at,
        })
    }
}

/// What this browser offers for passkeys, as `web/passkey.js` (`passkeyFacts`) found it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Passkeys {
    /// The glue has said; until then no passkey is asked for at pairing.
    pub known: bool,
    /// A platform authenticator with user verification.
    pub available: bool,
    /// The browser's own word on PRF; `None` where it does not say.
    pub prf: Option<bool>,
    /// This frame's storage lasts only while the browser runs: WebKit, in a frame on another origin.
    pub per_session: bool,
    /// A passkey can be made in this frame; else only at top level, in a window of its own (WebKit refuses
    /// `create()` in a frame whose origin differs from its ancestors').
    pub create_in_frame: bool,
}

impl Passkeys {
    pub fn read(facts: &Value) -> Self {
        let flag = |field: &str| facts.get(field).and_then(Value::as_bool);
        Self {
            known: true,
            available: flag("available").unwrap_or(false),
            prf: flag("prf"),
            per_session: flag("per_session").unwrap_or(false),
            create_in_frame: flag("create_in_frame").unwrap_or(false),
        }
    }

    /// Why a passkey cannot keep a pairing here, if it cannot; unknown support counts as none.
    pub fn unusable(&self) -> Option<&'static str> {
        if !self.known {
            Some("passkey support not known")
        } else if !self.available {
            Some("no passkey with user verification here")
        } else if self.prf == Some(false) {
            Some("no passkey PRF in this browser")
        } else {
            None
        }
    }

    /// Why a passkey surely cannot open a kept record here; unknown support is worth the try.
    pub fn cannot_unlock(&self) -> Option<&'static str> {
        self.unusable().filter(|_| self.known)
    }

    /// For the Storage line.
    pub fn line(&self) -> String {
        if !self.known {
            return "passkeys unknown".into();
        }
        let prf = match self.prf {
            Some(true) => "PRF yes",
            Some(false) => "PRF no",
            None => "PRF unreported",
        };
        let frame = if self.per_session { "frame storage per session" } else { "frame storage kept" };
        format!("passkeys {}, {prf}, {frame}", if self.available { "yes" } else { "no" })
    }
}

/// Why a passkey step failed, as the glue said: the step, the error's name and its message, shortened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: String,
    pub name: String,
    pub detail: String,
}

impl Failure {
    pub fn read(value: &Value) -> Self {
        let clean = |field: &str, max: usize| -> String {
            let raw = value.get(field).and_then(Value::as_str).unwrap_or_default();
            raw.chars().filter(|c| !c.is_control()).take(max).collect::<String>().trim().to_string()
        };
        Self { step: clean("step", 16), name: clean("name", 40), detail: clean("detail", MAX_DETAIL) }
    }

    /// The user cancelled, or the browser refused without saying more (WebAuthn's `NotAllowedError`).
    pub fn cancelled(&self) -> bool {
        self.name == "NotAllowedError"
    }

    pub fn text(&self) -> String {
        let what = match self.step.as_str() {
            "open" => "the passkey did not open this copy",
            "window" => "the passkey window did not open",
            "create" => "making the passkey failed",
            _ if self.cancelled() => "cancelled or not allowed",
            _ => "the passkey failed",
        };
        match (self.name.as_str(), self.detail.as_str()) {
            ("", _) => what.to_string(),
            (name, "") => format!("{what} ({name})"),
            (name, detail) => format!("{what} ({name}: {detail})"),
        }
    }
}

/// What the next tap on the page's passkey button does.
pub enum Ask {
    /// Seal `device`'s key: with the passkey `credential` names; with whichever passkey the browser offers when
    /// `made` (one was just made in its own window); else make one first, here when `create_in_frame`, else in a
    /// window on this origin.
    Keep { device: Device, credential: Option<String>, made: bool, create_in_frame: bool },
    /// Open the record.
    Unlock(Sealed),
    /// Nothing: the button goes, and what was prepared for it is zeroed.
    Done,
}

impl fmt::Debug for Ask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ask::Keep { device, made, create_in_frame, credential } => write!(
                f,
                "Keep({}, credential: {}, made: {made}, in frame: {create_in_frame})",
                device.host_id,
                credential.is_some()
            ),
            Ask::Unlock(sealed) => write!(f, "Unlock({})", sealed.host_id),
            Ask::Done => f.write_str("Done"),
        }
    }
}

/// What came of a tap.
pub enum Answer {
    Sealed(Sealed),
    /// The record's device key.
    Unlocked(Secret),
    /// The passkey window opened.
    Enrolling,
    /// A passkey was made; its credential ID where that was said.
    Enrolled(Option<String>),
    /// The passkey gives no PRF output here, or cannot.
    NoPrf,
    Failed(Failure),
}

impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Answer::Sealed(sealed) => write!(f, "Sealed({sealed:?})"),
            Answer::Unlocked(_) => f.write_str("Unlocked(hidden)"),
            Answer::Enrolling => f.write_str("Enrolling"),
            Answer::Enrolled(credential) => write!(f, "Enrolled({})", credential.is_some()),
            Answer::NoPrf => f.write_str("NoPrf"),
            Answer::Failed(failure) => write!(f, "Failed({failure:?})"),
        }
    }
}

/// Where this frame's key came from on this page, and what came of keeping a new pairing: the Storage line's
/// `Key:` part.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Path {
    /// Nothing read yet.
    #[default]
    Unread,
    /// This frame's own store had it.
    FrameStore,
    /// The hosting page's sealed copy waits for the passkey.
    Locked,
    /// The hosting page's sealed copy, opened with the passkey.
    Unsealed,
    /// No key here, and no passkey to open the hosting page's copy with: why.
    NoPasskey(String),
    /// No key here, and the hosting page keeps no copy for this host.
    Nothing,
    /// Paired on this page: whether the hosting page keeps a sealed copy, and why not.
    Paired { kept: bool, why: Option<String> },
}

impl Path {
    pub fn line(&self) -> String {
        match self {
            Path::Unread => "not read".into(),
            Path::FrameStore => "frame store".into(),
            Path::Locked => "sealed copy, waiting for the passkey".into(),
            Path::Unsealed => "unsealed with the passkey".into(),
            Path::NoPasskey(why) => format!("no passkey ({why})"),
            Path::Nothing => "none kept here or by the page".into(),
            Path::Paired { kept: true, .. } => "paired, sealed copy kept by the page".into(),
            Path::Paired { kept: false, why } => {
                format!("paired, no sealed copy ({})", why.as_deref().unwrap_or("not asked"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Value {
        json!({
            "v": 2, "host_id": "host-1", "host_key": PublicKey::from_bytes([7; 32]).text(),
            "credential": "AbCdEfGhIjKlMnOpQrStUv-_", "iv": "A".repeat(16), "sealed": "B/".repeat(32),
            "paired_at": 1_762_510_688_123u64,
        })
    }

    #[test]
    fn a_record_reads_only_in_its_exact_shape_and_round_trips() {
        let sealed = Sealed::read(&record()).expect("a record");
        assert_eq!(sealed.to_json(), record());
        assert!(!format!("{sealed:?}").contains("B/B/"), "Debug shows no ciphertext");
        for (field, bad) in [
            ("v", json!(1)),
            ("host_id", json!("a b")),
            ("host_key", json!("short")),
            ("credential", json!("not base64url!!!!!")),
            ("credential", json!("short")),
            ("iv", json!("A".repeat(15))),
            ("sealed", json!("B-".repeat(32))),
            ("paired_at", json!(-1)),
        ] {
            let mut value = record();
            value[field] = bad;
            assert_eq!(Sealed::read(&value), None, "{field}");
        }
        assert_eq!(Sealed::read(&Value::Null), None);
    }

    #[test]
    fn passkeys_say_why_they_cannot_help() {
        assert_eq!(Passkeys::default().unusable(), Some("passkey support not known"));
        assert_eq!(Passkeys::default().cannot_unlock(), None, "unknown support is worth a try");
        let none = Passkeys::read(&json!({"available": false, "per_session": true}));
        assert_eq!(none.cannot_unlock(), Some("no passkey with user verification here"));
        let no_prf = Passkeys::read(&json!({"available": true, "prf": false}));
        assert_eq!(no_prf.unusable(), Some("no passkey PRF in this browser"));
        let safari = Passkeys::read(&json!({"available": true, "per_session": true, "create_in_frame": false}));
        assert_eq!(safari.unusable(), None);
        assert_eq!(safari.line(), "passkeys yes, PRF unreported, frame storage per session");
    }

    #[test]
    fn a_failure_reads_bounded_and_says_its_step() {
        let failure = Failure::read(&json!({"step": "get", "name": "NotAllowedError", "detail": "x\n".repeat(200)}));
        assert!(failure.cancelled());
        assert!(failure.detail.chars().count() <= MAX_DETAIL);
        assert!(failure.text().starts_with("cancelled or not allowed (NotAllowedError: x"), "{}", failure.text());
        let open = Failure::read(&json!({"step": "open", "name": "OperationError"}));
        assert_eq!(open.text(), "the passkey did not open this copy (OperationError)");
    }
}
