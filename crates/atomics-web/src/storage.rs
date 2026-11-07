//! What the browser's storage did with this host's device key, as `web/device-store.js` reports it
//! (`storageFacts`): how its database opened, what the last read and save found, when this browser last
//! saved or forgot the host's device, and whether the browser keeps the storage persistently.
//!
//! The pairing screens show it under their reason, and a pairing adds a short form of it to the name the
//! host records, so a key that went missing can be told apart afterwards: a database made anew (the
//! browser dropped the storage), one kept without a sealing key (no save got that far), or one kept
//! without this host's record. Names, counts, outcomes and times only; never key material.

use serde_json::Value;

/// The longest fact taken, in characters.
const MAX_FACT: usize = 48;

/// The device store's facts. Text facts are `unknown` when the store did not say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Storage {
    /// How the database opened: `new` (made during this page's life), `existing`, `blocked`,
    /// `failed <error>`, or `not-run`.
    pub open: String,
    /// The sealing key: `present`, `absent`, `made` (by a save on this page), or `unknown`.
    pub seal: String,
    /// How many hosts' devices the database holds.
    pub records: Option<u64>,
    /// The last read: `found`, `none`, `failed <error>`, `started`, or `not-run`.
    pub read: String,
    /// The last save: `confirmed` (committed and read back), `failed <step> <error>`, `started`, or
    /// `not-run`.
    pub save: String,
    /// `saved <time>`, `forgot <time>` or `none`: kept in localStorage, beside the database.
    pub mark: String,
    /// Whether the browser keeps this origin's storage persistently; `None` where it cannot say.
    pub persisted: Option<bool>,
    /// The last answer to asking for persistence.
    pub persist: Option<bool>,
    /// Whether the frame has its unpartitioned storage (`document.hasStorageAccess()`).
    pub access: Option<bool>,
    /// On iOS: an app on the Home Screen rather than a browser tab.
    pub standalone: Option<bool>,
}

impl Storage {
    /// The facts as `storageFacts` returns them, JSON-encoded; what is missing or malformed is unknown.
    pub fn read(facts: &Value) -> Self {
        let flag = |field: &str| facts.get(field).and_then(Value::as_bool);
        Self {
            open: fact(facts, "open"),
            seal: fact(facts, "seal"),
            records: facts.get("records").and_then(Value::as_u64),
            read: fact(facts, "read"),
            save: fact(facts, "save"),
            mark: fact(facts, "mark"),
            persisted: flag("persisted"),
            persist: flag("persist"),
            access: flag("access"),
            standalone: flag("standalone"),
        }
    }

    /// One line for the pairing screens and the console.
    pub fn line(&self) -> String {
        let records = self.records.map_or_else(|| "?".to_string(), |n| n.to_string());
        let mut line = format!(
            "Storage: database {}, sealing key {}, {records} saved; read {}; save {}; mark {}; persisted {}; \
             storage access {}",
            self.open,
            self.seal,
            self.read,
            self.save,
            self.mark,
            yes(self.persisted),
            yes(self.access),
        );
        if let Some(app) = self.standalone {
            line.push_str(if app { "; Home Screen app" } else { "; browser tab" });
        }
        line
    }

    /// A short form for the name a pairing gives the host, with `more` at its end: `[new s0 k0 no mark p0]`, for
    /// the database (`new`, `kept` or `fail`), `s` the sealing key, `k` the devices kept, the last mark and `p`
    /// persistence. With any device name and the driver's ` c1 a1` it fits the host's limit on names.
    pub fn tag(&self, more: &str) -> String {
        let db = match self.open.as_str() {
            "new" => "new",
            "existing" => "kept",
            _ => "fail",
        };
        let seal = match self.seal.as_str() {
            "present" | "made" => "s1",
            "absent" => "s0",
            _ => "s?",
        };
        let keys = self.records.map_or_else(|| "k?".to_string(), |n| format!("k{}", n.min(99)));
        let persisted = match self.persisted {
            Some(true) => "p1",
            Some(false) => "p0",
            None => "p?",
        };
        format!("[{db} {seal} {keys} {} {persisted}{more}]", short_mark(&self.mark))
    }
}

fn yes(flag: Option<bool>) -> &'static str {
    match flag {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

/// `saved 2026-11-07T10:18:08.123Z` as `saved 11-07 10:18Z`; no mark as `no mark`.
fn short_mark(mark: &str) -> String {
    match mark.split_once(' ') {
        Some((kind @ ("saved" | "forgot"), time)) => match (time.get(5..10), time.get(11..16)) {
            (Some(day), Some(minute)) => format!("{kind} {day} {minute}Z"),
            _ => kind.to_string(),
        },
        _ if mark == "none" => "no mark".to_string(),
        _ => "mark ?".to_string(),
    }
}

/// A text fact: letters, digits, spaces and `:.-` only, at most [`MAX_FACT`] characters.
fn fact(facts: &Value, field: &str) -> String {
    let text = facts.get(field).and_then(Value::as_str).unwrap_or_default();
    let clean: String =
        text.chars().filter(|c| c.is_ascii_alphanumeric() || " :.-".contains(*c)).take(MAX_FACT).collect();
    match clean.trim() {
        "" => "unknown".to_string(),
        clean => clean.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use atomics_core::noise::{self, MAX_DEVICE_NAME};
    use serde_json::json;

    use super::*;

    fn kept() -> Value {
        json!({
            "open": "existing", "seal": "present", "records": 0, "read": "none", "save": "not-run",
            "mark": "saved 2026-11-07T10:18:08.123Z", "persisted": false, "persist": false, "access": true,
            "standalone": true,
        })
    }

    #[test]
    fn the_facts_read_as_one_line_and_a_short_tag() {
        let storage = Storage::read(&kept());
        assert_eq!(
            storage.line(),
            "Storage: database existing, sealing key present, 0 saved; read none; save not-run; mark saved \
             2026-11-07T10:18:08.123Z; persisted no; storage access yes; Home Screen app"
        );
        assert_eq!(storage.tag(""), "[kept s1 k0 saved 11-07 10:18Z p0]");
        let fresh = Storage::read(&json!({"open": "new", "seal": "absent", "records": 0, "mark": "none"}));
        assert_eq!(fresh.tag(" c1 a1"), "[new s0 k0 no mark p? c1 a1]");
        assert!(fresh.line().ends_with("persisted unknown; storage access unknown"), "{}", fresh.line());
    }

    #[test]
    fn missing_or_strange_facts_are_unknown_and_bounded() {
        let storage =
            Storage::read(&json!({"open": "<b>new</b>\n", "records": -1, "mark": 7, "save": "x".repeat(500)}));
        assert_eq!(storage.open, "bnewb");
        assert_eq!((storage.records, storage.mark.as_str(), storage.read.as_str()), (None, "unknown", "unknown"));
        assert_eq!(storage.save.len(), MAX_FACT);
        assert_eq!(storage.tag(""), "[fail s? k? mark ? p?]");
        assert_eq!(Storage::read(&Value::Null).open, "unknown");
    }

    #[test]
    fn a_tagged_name_fits_the_hosts_limit() {
        let mut facts = kept();
        facts["records"] = json!(12_345);
        facts["mark"] = json!("forgot 2026-11-07T10:18:08.123Z");
        let name = format!("Firefox on ChromeOS {}", Storage::read(&facts).tag(" cx a?"));
        assert!(name.chars().count() <= MAX_DEVICE_NAME, "{name}");
        assert_eq!(noise::device_name(&name), name, "the host keeps it whole");
    }
}
