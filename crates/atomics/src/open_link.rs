//! `atomics://open` links: strict parsing, and handing a link to the ATOMICS that is running.
//!
//! A link only ever opens and focuses: `atomics://open` focuses the window,
//! `atomics://open?plan=<plan id>` shows an open plan, and
//! `atomics://open?ticket=<key or ticket URL>[&system=<name>]` shows the plan that links the
//! ticket, or offers to plan it. Anything else is refused.

use std::path::Path;

use atomics_core::control::ControlRequest;

#[cfg(target_os = "macos")]
pub mod macos;

const SCHEME: &str = "atomics";
const HOST: &str = "open";
const MAX_VALUE: usize = 2048;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenLink {
    pub plan: Option<String>,
    pub ticket: Option<String>,
    pub system: Option<String>,
}

/// What a launch was asked to do with a link: open it, or show why it was refused.
pub type Incoming = Result<OpenLink, String>;

/// Whether a command-line argument is meant as a link rather than a folder.
pub fn is_link(argument: &str) -> bool {
    argument
        .get(..SCHEME.len() + 1)
        .is_some_and(|head| head[..SCHEME.len()].eq_ignore_ascii_case(SCHEME) && head.ends_with(':'))
}

pub fn parse(raw: &str) -> Incoming {
    let url = url::Url::parse(raw.trim()).map_err(|_| "not a valid address".to_string())?;
    if url.scheme() != SCHEME {
        return Err(format!("scheme {:?} is not {SCHEME}", shown(url.scheme())));
    }
    if url.host_str().is_none_or(|host| !host.eq_ignore_ascii_case(HOST)) {
        return Err(format!("only {SCHEME}://{HOST} is understood"));
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return Err("no user, password or port is accepted".into());
    }
    if !matches!(url.path(), "" | "/") {
        return Err("no path is accepted".into());
    }
    if url.fragment().is_some() {
        return Err("no fragment is accepted".into());
    }
    let mut link = OpenLink::default();
    for (name, value) in url.query_pairs() {
        let slot = match name.as_ref() {
            "ticket" => &mut link.ticket,
            "system" => &mut link.system,
            "plan" => &mut link.plan,
            other => return Err(format!("parameter {:?} is not accepted", shown(other))),
        };
        if slot.is_some() {
            return Err(format!("parameter {name} is given twice"));
        }
        *slot = Some(value.into_owned());
    }
    link.validate()
}

/// Names from a link, shortened for a message.
fn shown(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(40).collect()
}

impl OpenLink {
    /// Trims the values and refuses empty, long or control-character ones, and combinations
    /// that mean nothing: a plan together with a ticket, or a system without a ticket.
    pub fn validate(mut self) -> Incoming {
        for (name, value) in [("plan", &mut self.plan), ("ticket", &mut self.ticket), ("system", &mut self.system)] {
            let Some(text) = value.as_mut() else { continue };
            *text = text.trim().to_string();
            if text.is_empty() {
                return Err(format!("parameter {name} is empty"));
            }
            if text.chars().count() > MAX_VALUE || text.chars().any(char::is_control) {
                return Err(format!("parameter {name} is not accepted"));
            }
        }
        match (&self.plan, &self.ticket, &self.system) {
            (Some(_), Some(_), _) => Err("give a plan or a ticket, not both".into()),
            (_, None, Some(_)) => Err("a system needs a ticket".into()),
            _ => Ok(self),
        }
    }

    pub fn request(incoming: Incoming) -> ControlRequest {
        match incoming {
            Ok(link) => {
                ControlRequest::OpenLink { plan: link.plan, ticket: link.ticket, system: link.system, rejected: None }
            }
            Err(reason) => ControlRequest::OpenLink { plan: None, ticket: None, system: None, rejected: Some(reason) },
        }
    }
}

/// Hands a link to the ATOMICS listening on `socket`. `None` when none is running, so the caller
/// starts one; otherwise the exit code of the process that forwarded it.
pub fn forward(socket: &Path, incoming: Incoming) -> Option<i32> {
    match crate::ipc::request_at(socket, &OpenLink::request(incoming)) {
        Ok(response) if response.ok => Some(0),
        Ok(response) => {
            eprintln!("atomics: {}", response.error.unwrap_or_else(|| "the link was not opened".into()));
            Some(1)
        }
        Err(crate::ipc::RequestError::NotRunning(_)) => None,
        Err(error) => {
            eprintln!("atomics: {error}");
            Some(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> OpenLink {
        parse(raw).unwrap_or_else(|error| panic!("{raw}: {error}"))
    }

    #[test]
    fn accepted_forms() {
        assert_eq!(ok("atomics://open"), OpenLink::default());
        assert_eq!(ok("atomics://open/"), OpenLink::default());
        assert_eq!(ok("ATOMICS://OPEN?plan=abc").plan.as_deref(), Some("abc"));
        assert_eq!(ok("atomics://open?plan=p-1").plan.as_deref(), Some("p-1"));
        let link = ok("atomics://open?ticket=ABC-12&system=Example");
        assert_eq!((link.ticket.as_deref(), link.system.as_deref()), (Some("ABC-12"), Some("Example")));
        // A ticket URL travels percent-encoded.
        let link = ok("atomics://open?ticket=https%3A%2F%2Ftickets.example%2F%3Fselected%3D1&system=x");
        assert_eq!(link.ticket.as_deref(), Some("https://tickets.example/?selected=1"));
        assert_eq!(ok("atomics://open?ticket=%20ABC-1%20").ticket.as_deref(), Some("ABC-1"));
        assert_eq!(ok("atomics://open?").plan, None);
    }

    #[test]
    fn rejected_schemes_hosts_and_shapes() {
        for raw in [
            "https://open?plan=a",
            "atomics-x://open",
            "atomics:open?plan=a",
            "atomics://close?plan=a",
            "atomics://open.example?plan=a",
            "atomics://user@open?plan=a",
            "atomics://open:80?plan=a",
            "atomics://open/plans?plan=a",
            "atomics://open?plan=a#frag",
            "",
            "not a url",
        ] {
            assert!(parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn rejected_parameters() {
        for raw in [
            "atomics://open?cmd=restart",
            "atomics://open?plan=a&extra=1",
            "atomics://open?plan=a&plan=b",
            "atomics://open?ticket=A-1&ticket=A-2",
            "atomics://open?plan=",
            "atomics://open?ticket=%20",
            "atomics://open?plan=a&ticket=A-1",
            "atomics://open?system=Example",
            "atomics://open?ticket=A%0A-1",
        ] {
            assert!(parse(raw).is_err(), "{raw}");
        }
        assert!(parse(&format!("atomics://open?plan={}", "a".repeat(MAX_VALUE + 1))).is_err());
        // A refusal never repeats a long name back.
        let error = parse(&format!("atomics://open?{}=1", "n".repeat(500))).unwrap_err();
        assert!(error.len() < 100, "{error}");
    }

    #[test]
    fn only_atomics_arguments_are_links() {
        assert!(is_link("atomics://open"));
        assert!(is_link("ATOMICS:whatever"));
        assert!(!is_link("/home/user/atomics"));
        assert!(!is_link("atomics"));
        assert!(!is_link("./atomics://x"));
        assert!(!is_link("é"));
    }

    #[test]
    fn a_second_instance_forwards_the_request_to_the_running_one() {
        use std::io::{BufRead, BufReader, Write};
        let dir = crate::test_support::tempdir().unwrap();
        let socket = dir.path().join("atomics.sock");
        // Nobody listens yet: the caller starts ATOMICS itself.
        assert_eq!(forward(&socket, ok_link("atomics://open?plan=a")), None);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            (&stream).write_all(b"{\"ok\":true,\"result\":null}\n").unwrap();
            line
        });
        assert_eq!(forward(&socket, ok_link("atomics://open?ticket=A-1&system=Example")), Some(0));
        let request: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
        assert_eq!(request["cmd"], "open_link");
        assert_eq!(request["ticket"], "A-1");
        assert_eq!(request["system"], "Example");
        assert!(request["plan"].is_null());
    }

    fn ok_link(raw: &str) -> Incoming {
        parse(raw)
    }

    #[test]
    fn a_refused_link_is_forwarded_as_a_reason_for_a_toast() {
        let request = OpenLink::request(parse("atomics://open?x=1"));
        let ControlRequest::OpenLink { plan: None, ticket: None, system: None, rejected: Some(reason) } = request
        else {
            panic!("rejection")
        };
        assert!(reason.contains("not accepted"), "{reason}");
    }
}
