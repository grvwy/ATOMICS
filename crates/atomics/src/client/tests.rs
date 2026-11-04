//! The native client against servers that misbehave: malformed frames are skipped, a host of another
//! major version stops it with a message rather than retrying forever, and a refused token stops it too.
//! The real host's gates are in `app::client_tests`.

use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use atomics_core::wire::{self, Envelope, Message};
use atomics_web::driver::{Status, Stop};
use tungstenite::handshake::server::{Request, Response};
use zeroize::Zeroizing;

use super::ClientApp;
use super::link::Endpoint;

/// What the fake server does after it accepts the upgrade.
type Script = fn(&mut tungstenite::WebSocket<std::net::TcpStream>);

/// A server that accepts one connection: refuses its upgrade with `refuse`, or runs `script`.
fn server(refuse: Option<u16>, script: Script) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let Ok((stream, _)) = listener.accept() else { return };
        #[allow(clippy::result_large_err)] // the error type is tungstenite's callback contract
        let callback = |request: &Request, mut response: Response| {
            assert_eq!(
                request.headers().get("Authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer the-token"),
                "the token goes in the upgrade"
            );
            if let Some(status) = refuse {
                return Err(tungstenite::http::Response::builder().status(status).body(None).unwrap());
            }
            response.headers_mut().insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
            Ok(response)
        };
        if let Ok(mut ws) = tungstenite::accept_hdr(stream, callback) {
            script(&mut ws);
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    addr
}

fn client(addr: SocketAddr) -> ClientApp {
    let ctx = egui::Context::default();
    ClientApp::fixed(&ctx, move || {
        Ok(Endpoint { addr, host: format!("127.0.0.1:{}", addr.port()), token: Zeroizing::new("the-token".into()) })
    })
}

fn until(app: &mut ClientApp, what: &str, done: impl Fn(&Status) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(10));
    while !done(app.driver().status()) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}: {:?}", app.driver().status());
        app.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The client's hello, then the host's welcome: what every script starts with.
fn greet(ws: &mut tungstenite::WebSocket<std::net::TcpStream>) {
    let hello = ws.read().unwrap();
    let hello = wire::decode(hello.to_text().unwrap()).unwrap();
    assert!(matches!(hello.message, Message::Hello { .. }), "{hello:?}");
}

#[test]
fn malformed_frames_are_skipped_and_another_major_version_stops_the_client() {
    let addr = server(None, |ws| {
        greet(ws);
        for junk in ["{not json", "[1,2]", r#"{"v":{"major":1,"minor":0},"kind":"teleport"}"#] {
            ws.send(tungstenite::Message::text(junk)).unwrap();
        }
        let ping = Envelope::new(Message::Ping { nonce: Some("still-here".into()) });
        ws.send(tungstenite::Message::text(serde_json::to_string(&ping).unwrap())).unwrap();
        // The client answers the ping after the junk: it skipped it and is still there.
        loop {
            let frame = ws.read().unwrap();
            let Ok(envelope) = wire::decode(frame.to_text().unwrap()) else { continue };
            if matches!(envelope.message, Message::Pong { .. }) {
                break;
            }
        }
        ws.send(tungstenite::Message::text(r#"{"v":{"major":2,"minor":0},"kind":"ping"}"#)).unwrap();
    });
    let mut app = client(addr);
    until(&mut app, "the version stop", |status| matches!(status, Status::Stopped(_)));
    assert_eq!(app.driver().status(), &Status::Stopped(Stop::Version));
    assert!(Stop::Version.message().contains("version"));
}

#[test]
fn a_host_of_another_major_version_refusing_the_hello_stops_the_client() {
    let addr = server(None, |ws| {
        greet(ws);
        let refusal = Envelope::new(Message::Error {
            code: wire::ErrorCode::UnsupportedVersion,
            message: "this host speaks 2.0".into(),
        });
        ws.send(tungstenite::Message::text(serde_json::to_string(&refusal).unwrap())).unwrap();
        let _ = ws.close(None);
    });
    let mut app = client(addr);
    until(&mut app, "the version stop", |status| matches!(status, Status::Stopped(_)));
    assert_eq!(app.driver().status(), &Status::Stopped(Stop::Version));
}

#[test]
fn a_refused_token_stops_the_client() {
    let addr = server(Some(401), |_| {});
    let mut app = client(addr);
    until(&mut app, "the refusal", |status| matches!(status, Status::Stopped(_)));
    assert_eq!(app.driver().status(), &Status::Stopped(Stop::Unauthorised));
}

#[test]
fn a_host_that_is_not_there_is_tried_again() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut app = client(addr);
    until(&mut app, "a reconnect", |status| matches!(status, Status::Reconnecting { .. }));
}
