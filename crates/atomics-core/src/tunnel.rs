//! The rendezvous tunnel's wire: how a host's socket to a rendezvous service is framed, and the
//! close codes both legs use (docs/daemon.md, "Rendezvous"). Browser-safe, so the web client
//! reads the same codes.
//!
//! On the host's socket every binary message is `[kind u8][channel u32 BE][body]`. A browser's
//! socket is one channel, so its messages are bare bodies. `data` bodies are opaque to the
//! service; `control` bodies are an operation byte and its small arguments.

/// The subprotocol every rendezvous socket asks for, and the service answers with.
pub const SUBPROTOCOL: &str = "atomics.rendezvous.v1";

/// Kind and channel.
pub const HEADER_BYTES: usize = 5;

/// The longest body a frame carries; a host message may hold the header besides.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// The text a host signs with its device key to buy a pass: this, LF, the host id, LF, the
/// service's challenge.
pub const PASS_CONTEXT: &str = "atomics-tunnel-pass-v1";

const DATA: u8 = 0;
const CONTROL: u8 = 1;
const JOINED: u8 = 1;
const LEFT: u8 = 2;
const CLOSE: u8 = 3;
const RENEW: u8 = 4;
const EXPIRY: u8 = 5;

/// Channel 0 is the host's own; the service numbers browser channels from 1, never reusing one
/// on a socket.
pub const HOST_CHANNEL: u32 = 0;

/// Close codes. The service's codes (44xx) end a host socket or a channel; the host closes a
/// channel only with `NORMAL` or a code in 4000-4999, and anything else would end its whole socket.
pub mod close {
    pub const NORMAL: u16 = 1000;
    pub const GOING_AWAY: u16 = 1001;
    pub const PROTOCOL_ERROR: u16 = 1002;
    pub const UNSUPPORTED: u16 = 1003;
    pub const ABNORMAL: u16 = 1006;
    pub const TOO_BIG: u16 = 1009;
    pub const INTERNAL: u16 = 1011;
    /// The host refused the client: its first frames broke the protocol.
    pub const REFUSED: u16 = 4000;
    /// The client sent nothing for too long.
    pub const IDLE: u16 = 4001;
    /// The host could not serve the channel (no thread, or shutting down).
    pub const UNAVAILABLE: u16 = 4002;
    /// The browser's key is not on the host's allowlist: never paired, or revoked there. Pair again.
    pub const NOT_PAIRED: u16 = 4003;
    /// No pairing is open on the host: none was offered, or it expired or was used. Asking again
    /// cannot help; the host must show a new link.
    pub const NO_PAIRING: u16 = 4004;
    /// The host's pairing link is open, but another device is pairing with it now. It may come
    /// free, so a browser asks again while its link may be valid. Older hosts send `NO_PAIRING`
    /// for this too; an older browser reads this code as any dropped channel and dials again,
    /// until the link is used or expires and the host says `NO_PAIRING`.
    pub const PAIRING_BUSY: u16 = 4005;
    /// The pass lapsed unrenewed, a renewal was refused, or (on a channel) the browser's session ended.
    pub const UNAUTHORISED: u16 = 4401;
    /// The host was revoked: it needs a new device key.
    pub const REVOKED: u16 = 4403;
    /// The host is not online, or it left.
    pub const HOST_OFFLINE: u16 = 4404;
    /// A buffer stayed full too long; the client reconnects and gets a fresh snapshot.
    pub const SLOW: u16 = 4408;
    /// The same host connected again; the old socket ends.
    pub const REPLACED: u16 = 4409;
    /// Too many hosts for the person, or channels for the host.
    pub const QUOTA: u16 = 4429;

    /// Whether a host may close a channel with `code`.
    pub fn host_may_send(code: u16) -> bool {
        code == NORMAL || (4000..=4999).contains(&code)
    }
}

/// What the service sends a host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FromService {
    /// A channel's bytes. The service always says `Joined` first.
    Data {
        channel: u32,
        body: Vec<u8>,
    },
    Joined(u32),
    /// A channel ended, with its close code. Nothing for it follows.
    Left {
        channel: u32,
        code: u16,
    },
    /// The host's admission ends at this Unix time in milliseconds.
    Expiry(i64),
    /// A kind or operation this host does not know: from a newer service, so ignored.
    Unknown,
}

/// Parse one binary message from the service; `None` when it is malformed.
pub fn parse(message: &[u8]) -> Option<FromService> {
    if message.len() < HEADER_BYTES {
        return None;
    }
    let channel = u32::from_be_bytes([message[1], message[2], message[3], message[4]]);
    let body = &message[HEADER_BYTES..];
    match message[0] {
        DATA if channel != HOST_CHANNEL => Some(FromService::Data { channel, body: body.to_vec() }),
        DATA => None,
        CONTROL => match (body.first().copied(), &body[body.len().min(1)..]) {
            (Some(JOINED), []) if channel != HOST_CHANNEL => Some(FromService::Joined(channel)),
            (Some(LEFT), [high, low]) if channel != HOST_CHANNEL => {
                Some(FromService::Left { channel, code: u16::from_be_bytes([*high, *low]) })
            }
            (Some(EXPIRY), until) if channel == HOST_CHANNEL && until.len() == 8 => {
                Some(FromService::Expiry(i64::from_be_bytes(until.try_into().expect("eight bytes"))))
            }
            (Some(JOINED | LEFT | EXPIRY), _) | (None, _) => None,
            (Some(_), _) => Some(FromService::Unknown),
        },
        _ => Some(FromService::Unknown),
    }
}

fn header(kind: u8, channel: u32, body: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_BYTES + body);
    frame.push(kind);
    frame.extend_from_slice(&channel.to_be_bytes());
    frame
}

/// A data frame for `channel`.
pub fn data(channel: u32, body: &[u8]) -> Vec<u8> {
    let mut frame = header(DATA, channel, body.len());
    frame.extend_from_slice(body);
    frame
}

/// Ask the service to close `channel` with `code` (see [`close::host_may_send`]). The browser gets
/// what was queued for it first.
pub fn close_channel(channel: u32, code: u16) -> Vec<u8> {
    let mut frame = header(CONTROL, channel, 3);
    frame.push(CLOSE);
    frame.extend_from_slice(&code.to_be_bytes());
    frame
}

/// Extend the host's admission with a fresh pass.
pub fn renew(pass: &str) -> Vec<u8> {
    let mut frame = header(CONTROL, HOST_CHANNEL, 1 + pass.len());
    frame.push(RENEW);
    frame.extend_from_slice(pass.as_bytes());
    frame
}

/// What a host signs to buy a pass.
pub fn pass_message(host_id: &str, challenge: &str) -> String {
    format!("{PASS_CONTEXT}\n{host_id}\n{challenge}")
}

/// The service's own messages, for tests and for a service written in Rust.
pub mod service {
    use super::*;

    pub fn joined(channel: u32) -> Vec<u8> {
        let mut frame = header(CONTROL, channel, 1);
        frame.push(JOINED);
        frame
    }

    pub fn left(channel: u32, code: u16) -> Vec<u8> {
        let mut frame = header(CONTROL, channel, 3);
        frame.push(LEFT);
        frame.extend_from_slice(&code.to_be_bytes());
        frame
    }

    pub fn expiry(until_ms: i64) -> Vec<u8> {
        let mut frame = header(CONTROL, HOST_CHANNEL, 9);
        frame.push(EXPIRY);
        frame.extend_from_slice(&until_ms.to_be_bytes());
        frame
    }

    /// What a host sent the service.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum FromHost {
        Data { channel: u32, body: Vec<u8> },
        Close { channel: u32, code: u16 },
        Renew(String),
    }

    /// `None` for anything a service closes the socket over (1002).
    pub fn parse(message: &[u8]) -> Option<FromHost> {
        if message.len() < HEADER_BYTES {
            return None;
        }
        let channel = u32::from_be_bytes([message[1], message[2], message[3], message[4]]);
        let body = &message[HEADER_BYTES..];
        match (message[0], body.first().copied()) {
            (DATA, _) if channel != HOST_CHANNEL => Some(FromHost::Data { channel, body: body.to_vec() }),
            (CONTROL, Some(CLOSE)) if channel != HOST_CHANNEL && body.len() == 3 => {
                let code = u16::from_be_bytes([body[1], body[2]]);
                close::host_may_send(code).then_some(FromHost::Close { channel, code })
            }
            (CONTROL, Some(RENEW)) if channel == HOST_CHANNEL && body.len() > 1 => {
                let pass = &body[1..];
                pass.iter()
                    .all(|b| (0x21..=0x7e).contains(b))
                    .then(|| FromHost::Renew(String::from_utf8(pass.to_vec()).expect("printable ASCII")))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::service::FromHost;
    use super::*;

    #[test]
    fn every_frame_round_trips_between_host_and_service() {
        assert_eq!(parse(&service::joined(7)), Some(FromService::Joined(7)));
        assert_eq!(parse(&service::left(7, close::SLOW)), Some(FromService::Left { channel: 7, code: 4408 }));
        assert_eq!(parse(&service::expiry(1_793_000_000_123)), Some(FromService::Expiry(1_793_000_000_123)));
        assert_eq!(
            parse(&data(u32::MAX, b"{\"kind\":\"hello\"}")),
            Some(FromService::Data { channel: u32::MAX, body: b"{\"kind\":\"hello\"}".to_vec() })
        );
        assert_eq!(service::parse(&data(3, b"x")), Some(FromHost::Data { channel: 3, body: b"x".to_vec() }));
        assert_eq!(service::parse(&close_channel(3, 1000)), Some(FromHost::Close { channel: 3, code: 1000 }));
        assert_eq!(service::parse(&renew("pass.A-b_9")), Some(FromHost::Renew("pass.A-b_9".into())));
        // The layout itself, byte for byte.
        assert_eq!(data(0x0102_0304, b"ab"), [0, 1, 2, 3, 4, b'a', b'b']);
        assert_eq!(close_channel(1, 4408), [1, 0, 0, 0, 1, 3, 0x11, 0x38]);
        assert_eq!(renew("p"), [1, 0, 0, 0, 0, 4, b'p']);
        assert_eq!(service::expiry(1), [1, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 1]);
    }

    #[test]
    fn malformed_frames_are_refused_and_newer_ones_ignored() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[1, 0, 0, 0]), None, "shorter than a header");
        assert_eq!(parse(&data(0, b"x")), None, "no data on the host's own channel");
        assert_eq!(parse(&[1, 0, 0, 0, 1]), None, "a control frame without an operation");
        assert_eq!(parse(&[1, 0, 0, 0, 0, 1]), None, "joined names a browser channel");
        assert_eq!(parse(&[1, 0, 0, 0, 2, 2, 0]), None, "left carries two bytes");
        assert_eq!(parse(&[1, 0, 0, 0, 0, 5, 0]), None, "expiry carries eight bytes");
        assert_eq!(parse(&[1, 0, 0, 0, 1, 99]), Some(FromService::Unknown));
        assert_eq!(parse(&[9, 0, 0, 0, 1]), Some(FromService::Unknown));
        assert_eq!(service::parse(&close_channel(1, 1001)), None, "only 1000 or 4000-4999");
        assert_eq!(service::parse(&close_channel(0, 1000)), None);
        assert_eq!(service::parse(&renew("has space")), None);
        assert_eq!(service::parse(&renew("")), None);
        assert!(close::host_may_send(close::SLOW) && close::host_may_send(close::REFUSED));
        assert!(!close::host_may_send(close::GOING_AWAY) && !close::host_may_send(5000));
    }

    #[test]
    fn the_signed_text_is_context_host_and_challenge_on_three_lines() {
        assert_eq!(
            pass_message("0190a1b2-c3d4-8e5f-a617-283940516273", "n0nce"),
            "atomics-tunnel-pass-v1\n0190a1b2-c3d4-8e5f-a617-283940516273\nn0nce"
        );
    }
}
