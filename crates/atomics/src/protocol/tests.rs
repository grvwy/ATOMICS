use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use atomics_core::diff::{DiffLine, FileDiff, FileStatus, Hunk, LineKind};
use atomics_core::mirror::Mirror;
use atomics_core::plan::PlanStatus;
use atomics_core::projection::{
    ApprovalInfo, DiffFileSummary, DiffSummary, PlanInfo, Projections, SessionInfo,
};
use atomics_core::session::AgentStatus;
use atomics_core::wire::{
    self, CAPABILITIES, Envelope, ErrorCode, Message, ResetReason, Resume, Seq, VERSION,
};
use tungstenite::client::IntoClientRequest;
use tungstenite::{HandshakeError, WebSocket};

use super::hub::{Hub, RingLimits, Taken};
use super::{DiffSource, Server, ServerConfig, SessionConfig, Terminals};

pub(crate) fn wait(base: Duration) -> Duration {
    crate::test_support::scaled_timeout(base)
}

pub(crate) fn session(id: &str, title: &str, status: AgentStatus) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        agent: "claude".into(),
        title: title.into(),
        cwd: "/w/a".into(),
        worktree: Some("/w/a".into()),
        branch: Some("fix/a".into()),
        status,
        chief: false,
        task: None,
        pending_approvals: 0,
    }
}

fn plan(id: &str, goal: &str) -> PlanInfo {
    PlanInfo {
        id: id.into(),
        status: PlanStatus::Open,
        goal: goal.into(),
        project: None,
        approved: false,
        integration: None,
        tasks: Vec::new(),
    }
}

fn approval(id: &str) -> ApprovalInfo {
    ApprovalInfo {
        id: id.into(),
        session: "a".into(),
        tool: "Bash".into(),
        command: Some("git status".into()),
        cwd: "/w/a".into(),
        reason: None,
    }
}

fn summary(worktree: &str, revision: u64) -> DiffSummary {
    DiffSummary {
        worktree: worktree.into(),
        revision,
        base_ref: "main".into(),
        base: "abc123".into(),
        partial: false,
        files: vec![DiffFileSummary {
            old_path: None,
            new_path: Some("a.rs".into()),
            status: FileStatus::Added,
            binary: false,
            additions: 1,
            deletions: 0,
            omitted: 0,
        }],
    }
}

fn file_diff() -> FileDiff {
    FileDiff {
        old_path: None,
        new_path: Some("a.rs".into()),
        status: FileStatus::Added,
        binary: false,
        hunks: vec![Hunk {
            header: "@@ -0,0 +1 @@".into(),
            old_start: 0,
            new_start: 1,
            lines: vec![DiffLine {
                kind: LineKind::Added,
                old_no: None,
                new_no: Some(1),
                text: "fn main() {}".into(),
                emphasis: Vec::new(),
            }],
        }],
        omitted: 0,
    }
}

#[derive(Default)]
struct FakeDiffs(Mutex<HashMap<String, (u64, FileDiff)>>);

impl DiffSource for FakeDiffs {
    fn file(&self, worktree: &str, path: &str) -> Option<(u64, FileDiff)> {
        let files = self.0.lock().unwrap();
        let (revision, file) = files.get(worktree)?;
        (file.new_path.as_deref() == Some(path)).then(|| (*revision, file.clone()))
    }
}

fn populate(p: &Projections) {
    p.publish_sessions(vec![session("a", "fix the parser", AgentStatus::Working)]);
    p.publish_plans(vec![plan("p1", "ship it")]);
    p.publish_approvals(vec![approval("x1")]);
    p.publish_diffs(vec![summary("/w/a", 1)]);
}

fn config() -> ServerConfig {
    ServerConfig {
        poll_interval: Duration::from_millis(5),
        session: SessionConfig { tick: Duration::from_millis(5), ..SessionConfig::default() },
        ..ServerConfig::default()
    }
}

struct Fixture {
    server: Server,
    projections: Arc<Projections>,
}

fn start_with(config: ServerConfig) -> Fixture {
    let projections = Arc::new(Projections::default());
    let diffs = Arc::new(FakeDiffs::default());
    diffs.0.lock().unwrap().insert("/w/a".into(), (1, file_diff()));
    let server = Server::start(config, projections.clone(), diffs, Arc::new(Terminals::default())).unwrap();
    Fixture { server, projections }
}

fn start() -> Fixture {
    let fixture = start_with(config());
    populate(&fixture.projections);
    fixture
}

fn board_matches(m: &Mirror, p: &Projections) -> bool {
    fn sorted<T: Clone>(v: &[T], key: impl Fn(&T) -> String) -> Vec<T> {
        let mut v = v.to_vec();
        v.sort_by_key(|x| key(x));
        v
    }
    let l = p.load();
    m.sessions.values().cloned().collect::<Vec<_>>() == sorted(&l.sessions.data, |s| s.id.clone())
        && m.plans.values().cloned().collect::<Vec<_>>() == sorted(&l.plans.data, |s| s.id.clone())
        && m.approvals.values().cloned().collect::<Vec<_>>() == sorted(&l.approvals.data, |s| s.id.clone())
        && m.diffs.values().cloned().collect::<Vec<_>>() == sorted(&l.diffs.data, |s| s.worktree.clone())
}

/// A native client over a real loopback socket.
pub(crate) struct TestClient {
    pub(crate) ws: WebSocket<TcpStream>,
    pub mirror: Mirror,
    pub welcome: Option<Envelope>,
}

impl TestClient {
    pub(crate) fn connect(addr: SocketAddr, token: &str) -> Self {
        Self::try_connect(addr, Some(token), None, true).expect("upgrade")
    }

    pub(crate) fn try_connect(
        addr: SocketAddr,
        token: Option<&str>,
        origin: Option<&str>,
        subprotocol: bool,
    ) -> Result<Self, tungstenite::Error> {
        let stream = TcpStream::connect(addr).map_err(tungstenite::Error::Io)?;
        stream.set_read_timeout(Some(wait(Duration::from_secs(5)))).unwrap();
        let mut request = format!("ws://{addr}").into_client_request().unwrap();
        let headers = request.headers_mut();
        if let Some(token) = token {
            headers.insert("Authorization", format!("Bearer {token}").parse().unwrap());
        }
        if let Some(origin) = origin {
            headers.insert("Origin", origin.parse().unwrap());
        }
        if subprotocol {
            headers.insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
        }
        match tungstenite::client(request, stream) {
            Ok((ws, _)) => Ok(Self { ws, mirror: Mirror::default(), welcome: None }),
            Err(HandshakeError::Failure(e)) => Err(e),
            Err(HandshakeError::Interrupted(_)) => panic!("the upgrade timed out"),
        }
    }

    pub(crate) fn hello(&mut self) {
        let (prior_epoch, prior_cursor) = self.mirror.resume_point();
        self.send(&Envelope::new(Message::Hello {
            client_id: "test".into(),
            versions: vec![VERSION],
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            prior_epoch,
            prior_cursor: prior_cursor.map(Seq),
        }));
    }

    pub(crate) fn send(&mut self, envelope: &Envelope) {
        self.send_text(&serde_json::to_string(envelope).unwrap());
    }

    pub(crate) fn send_text(&mut self, text: &str) {
        self.ws.send(tungstenite::Message::text(text)).expect("send");
    }

    /// The next text frame, or `None` on close, error or timeout.
    pub(crate) fn next(&mut self) -> Option<Envelope> {
        loop {
            match self.ws.read() {
                Ok(tungstenite::Message::Text(t)) => return Some(wire::decode(t.as_str()).expect("the host sends valid frames")),
                Ok(tungstenite::Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
    }

    /// Read one frame and apply it to the mirror.
    pub(crate) fn step(&mut self) -> Option<Envelope> {
        let envelope = self.next()?;
        if matches!(envelope.message, Message::Welcome { .. }) {
            self.welcome = Some(envelope.clone());
        }
        self.mirror.apply(&envelope).unwrap_or_else(|gap| panic!("gap {gap:?} at {:?}", envelope.message));
        Some(envelope)
    }

    pub(crate) fn sync_until(&mut self, done: impl Fn(&Mirror) -> bool) {
        let deadline = Instant::now() + wait(Duration::from_secs(10));
        while !done(&self.mirror) {
            assert!(Instant::now() < deadline, "the client did not catch up: {:?}", self.mirror);
            assert!(self.step().is_some(), "the connection ended early");
        }
    }

    pub(crate) fn resume(&self) -> Option<Resume> {
        match self.welcome.as_ref()?.message {
            Message::Welcome { resume, .. } => Some(resume),
            _ => None,
        }
    }
}

fn status_of(error: tungstenite::Error) -> u16 {
    match error {
        tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

#[test]
fn a_loopback_client_gets_the_board_and_a_diff() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    assert!(f.server.addr().ip().is_loopback());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    assert_eq!(client.resume(), Some(Resume::Reset { cursor: Seq(client.mirror.cursor), reason: ResetReason::NewClient }));
    assert_eq!(client.mirror.diffs["/w/a"].files[0].new_path.as_deref(), Some("a.rs"));

    client.send(
        &Envelope::new(Message::DiffGet { worktree: "/w/a".into(), path: "a.rs".into(), revision: Some(Seq(1)) })
            .reply_to(Some("r1".into())),
    );
    let reply = client.step().unwrap();
    assert_eq!(reply.request_id.as_deref(), Some("r1"));
    let Message::DiffFile { body } = reply.message else { panic!("expected diff_file, got {:?}", reply.message) };
    assert_eq!((body.revision, body.file), (1, file_diff()));

    // A later change arrives as an event and the copy follows it.
    f.projections.publish_sessions(vec![session("a", "fix the parser", AgentStatus::Done)]);
    client.sync_until(|m| board_matches(m, &f.projections));
    assert_eq!(client.mirror.sessions["a"].status, AgentStatus::Done);
}

#[test]
fn a_stale_or_unknown_diff_request_is_an_error_not_silence() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    for (path, revision, code) in [("a.rs", Some(Seq(9)), ErrorCode::StaleRevision), ("nope.rs", None, ErrorCode::NotFound)] {
        client.send(&Envelope::new(Message::DiffGet { worktree: "/w/a".into(), path: path.into(), revision }));
        let Message::Error { code: got, .. } = client.step().unwrap().message else { panic!("expected an error") };
        assert_eq!(got, code);
    }
}

#[test]
fn upgrades_without_the_token_a_good_origin_or_the_subprotocol_fail() {
    let mut cfg = config();
    cfg.allowed_origins = vec!["https://app.example".into()];
    let f = start_with(cfg);
    let (addr, token) = (f.server.addr(), f.server.token().to_string());
    assert_eq!(status_of(TestClient::try_connect(addr, None, None, true).err().unwrap()), 401);
    assert_eq!(status_of(TestClient::try_connect(addr, Some("wrong"), None, true).err().unwrap()), 401);
    assert_eq!(status_of(TestClient::try_connect(addr, Some(&token), Some("https://evil.example"), true).err().unwrap()), 403);
    assert_eq!(status_of(TestClient::try_connect(addr, Some(&token), None, false).err().unwrap()), 400);
    assert!(TestClient::try_connect(addr, Some(&token), Some("https://app.example"), true).is_ok());
    assert!(TestClient::try_connect(addr, Some(&token), None, true).is_ok());
}

#[test]
fn malformed_unknown_and_version_mismatched_frames_get_typed_errors() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    for (frame, code) in [
        ("{not json", ErrorCode::Malformed),
        ("[1,2]", ErrorCode::Malformed),
        (r#"{"v":{"major":1,"minor":0},"kind":"teleport"}"#, ErrorCode::UnknownKind),
        (r#"{"v":{"major":2,"minor":0},"kind":"ping"}"#, ErrorCode::UnsupportedVersion),
        (r#"{"v":{"major":1,"minor":0},"kind":"state_snapshot"}"#, ErrorCode::Malformed),
        (r#"{"v":{"major":1,"minor":0},"kind":"welcome","version":{"major":1,"minor":0},"capabilities":[],"limits":{"max_frame_bytes":1,"max_subscriptions":1,"max_pending_ops":1,"max_queued_frames":1},"role":"controller","resume":{"mode":"replay","after":"0"}}"#, ErrorCode::Malformed),
    ] {
        client.send_text(frame);
        let Message::Error { code: got, .. } = client.step().unwrap().message else { panic!("expected an error for {frame}") };
        assert_eq!(got, code, "{frame}");
    }
    // The connection survives a bad frame and still answers.
    client.send(&Envelope::new(Message::Ping { nonce: Some("n".into()) }));
    assert!(matches!(client.step().unwrap().message, Message::Pong { nonce: Some(n) } if n == "n"));
}

#[test]
fn a_hello_the_host_cannot_serve_is_refused_before_any_state_is_sent() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.send_text(r#"{"v":{"major":9,"minor":0},"kind":"hello","client_id":"c","versions":[{"major":9,"minor":0}]}"#);
    let Message::Error { code, .. } = client.next().unwrap().message else { panic!("expected an error") };
    assert_eq!(code, ErrorCode::UnsupportedVersion);
    assert!(client.next().is_none(), "the host closes after refusing the version");

    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.send(&Envelope::new(Message::Ping { nonce: None }));
    let Message::Error { code, .. } = client.next().unwrap().message else { panic!("expected an error") };
    assert_eq!(code, ErrorCode::Malformed, "the first message must be hello");
    assert!(client.next().is_none());
}

#[test]
fn oversize_and_binary_frames_are_rejected_and_the_server_carries_on() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    client.send_text(&format!(r#"{{"v":{{"major":1,"minor":0}},"kind":"ping","nonce":"{}"}}"#, "x".repeat(wire::MAX_FRAME_BYTES)));
    // The error frame can lose a race with the close; when it arrives it names the limit.
    while let Some(envelope) = client.next() {
        if let Message::Error { code, .. } = envelope.message {
            assert_eq!(code, ErrorCode::Oversize);
        }
    }

    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    client.ws.send(tungstenite::Message::binary(vec![1, 2, 3])).unwrap();
    while let Some(envelope) = client.next() {
        if let Message::Error { code, .. } = envelope.message {
            assert_eq!(code, ErrorCode::Malformed);
        }
    }

    let mut again = TestClient::connect(f.server.addr(), f.server.token());
    again.hello();
    again.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
}

#[test]
fn a_client_that_keeps_sending_garbage_is_disconnected() {
    let mut cfg = config();
    cfg.session.max_errors = 3;
    let f = start_with(cfg);
    populate(&f.projections);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| m.cursor > 0);
    for _ in 0..20 {
        client.send_text("garbage");
    }
    let mut errors = 0;
    while let Some(envelope) = client.next() {
        errors += matches!(envelope.message, Message::Error { .. }) as u32;
    }
    assert!((1..20).contains(&errors), "the host stopped answering after a few errors, got {errors}");
}

#[test]
fn reconnect_replays_what_was_missed() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    let held = client.mirror.clone();
    drop(client);

    f.projections.publish_sessions(vec![
        session("a", "fix the parser", AgentStatus::Done),
        session("b", "second", AgentStatus::Idle),
    ]);
    f.projections.publish_approvals(Vec::new());
    f.projections.publish_diffs(Vec::new());
    f.projections.publish_plans(vec![plan("p1", "ship it, really")]);

    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.mirror = held.clone();
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections));
    assert_eq!(client.resume(), Some(Resume::Replay { after: Seq(held.cursor) }));
    assert!(client.mirror.cursor > held.cursor);
    assert!(client.mirror.diffs.is_empty() && client.mirror.approvals.is_empty());
}

#[test]
fn reconnect_resets_when_replay_is_not_possible() {
    let mut cfg = config();
    cfg.ring = RingLimits { entries: 2, ..RingLimits::default() };
    let f = start_with(cfg);
    populate(&f.projections);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    let held = client.mirror.clone();
    drop(client);

    // Too far behind for a ring of two events.
    for i in 0..6 {
        f.projections.publish_sessions(vec![session("a", &format!("title {i}"), AgentStatus::Working)]);
        std::thread::sleep(Duration::from_millis(30));
    }
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.mirror = held.clone();
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > held.cursor);
    assert!(matches!(client.resume(), Some(Resume::Reset { reason: ResetReason::CursorTooOld, .. })), "{:?}", client.resume());

    // A cursor from the future, and one from another boot.
    for (epoch, cursor, reason) in [
        (client.mirror.epoch.clone().unwrap(), 10_000, ResetReason::CursorAhead),
        ("some-earlier-boot".to_string(), 1, ResetReason::EpochChanged),
    ] {
        let mut other = TestClient::connect(f.server.addr(), f.server.token());
        other.send(&Envelope::new(Message::Hello {
            client_id: "t".into(),
            versions: vec![VERSION],
            capabilities: Vec::new(),
            prior_epoch: Some(epoch),
            prior_cursor: Some(Seq(cursor)),
        }));
        other.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
        assert!(matches!(other.resume(), Some(Resume::Reset { reason: got, .. }) if got == reason));
    }
}

#[test]
fn a_client_can_ask_for_a_resync() {
    let f = start();
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    client.sync_until(|m| board_matches(m, &f.projections) && m.cursor > 0);
    client.mirror.sessions.clear();
    client.send(&Envelope::new(Message::Resync { reason: Some("test".into()), screens: Vec::new() }));
    client.sync_until(|m| board_matches(m, &f.projections));
}

#[test]
fn a_reader_that_stops_reading_is_dropped_without_delaying_anyone() {
    let mut cfg = config();
    cfg.queue_frames = 8;
    cfg.write_timeout = Duration::from_millis(300);
    let f = start_with(cfg);
    populate(&f.projections);
    let addr = f.server.addr();
    let token = f.server.token().to_string();

    let mut slow = TestClient::connect(addr, &token);
    slow.hello();
    slow.sync_until(|m| m.cursor > 0);
    let mut good = TestClient::connect(addr, &token);
    good.hello();
    good.sync_until(|m| m.cursor > 0);
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while f.server.active_clients() < 2 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }

    // The well-behaved client reads on its own thread for the whole run.
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (stop, projections) = (stop.clone(), f.projections.clone());
        std::thread::spawn(move || {
            let until = Instant::now() + wait(Duration::from_secs(60));
            loop {
                let _ = good.ws.get_ref().set_read_timeout(Some(Duration::from_millis(50)));
                if let Ok(tungstenite::Message::Text(t)) = good.ws.read() {
                    let envelope = wire::decode(t.as_str()).unwrap();
                    good.mirror.apply(&envelope).ok();
                }
                if (stop.load(Ordering::Acquire) && board_matches(&good.mirror, &projections)) || Instant::now() > until {
                    return good.mirror;
                }
            }
        })
    };

    // `slow` never reads again. The publisher (standing in for the runtime thread) must not feel it.
    let big = "x".repeat(200_000);
    let (mut worst, mut i) = (Duration::ZERO, 0);
    let deadline = Instant::now() + wait(Duration::from_secs(40));
    while f.server.active_clients() > 1 {
        assert!(Instant::now() < deadline, "the stalled client was never dropped");
        i += 1;
        let started = Instant::now();
        f.projections.publish_sessions(vec![session("a", &format!("{i}{big}"), AgentStatus::Working)]);
        worst = worst.max(started.elapsed());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(worst < wait(Duration::from_millis(500)), "publishing waited {worst:?} on a slow reader");
    assert_eq!(f.server.active_clients(), 1, "only the stalled client is dropped");

    stop.store(true, Ordering::Release);
    let mirror = reader.join().unwrap();
    assert!(board_matches(&mirror, &f.projections), "the healthy client converged");
    drop(slow);
}

fn hub(queue_frames: usize) -> Hub {
    Hub::new("e1".into(), RingLimits::default(), queue_frames, 1 << 20)
}

#[test]
fn a_full_queue_drops_the_backlog_and_resyncs_with_a_snapshot() {
    let p = Projections::default();
    let mut hub = hub(2);
    hub.poll(&p);
    let attached = hub.attach(None, None).unwrap();
    for i in 0..5 {
        p.publish_sessions(vec![session("a", &format!("t{i}"), AgentStatus::Working)]);
        hub.poll(&p);
    }
    assert!(matches!(attached.outbox.take(), Taken::Lagged), "five events into a queue of two");
    hub.resync(&attached.outbox).unwrap();
    let Taken::Frames(frames) = attached.outbox.take() else { panic!("resynced") };
    assert_eq!(frames.len(), 1, "a snapshot replaces the backlog");
    let mut mirror = Mirror::default();
    mirror.apply(&wire::decode(&frames[0]).unwrap()).unwrap();
    assert_eq!((mirror.cursor, mirror.sessions["a"].title.as_str()), (hub.head(), "t4"));

    // Events after the resync are delivered normally again.
    p.publish_sessions(vec![session("a", "t5", AgentStatus::Done)]);
    hub.poll(&p);
    let Taken::Frames(frames) = attached.outbox.take() else { panic!("live again") };
    mirror.apply(&wire::decode(&frames[0]).unwrap()).unwrap();
    assert_eq!(mirror.sessions["a"].title, "t5");
}

#[test]
fn removals_become_events_and_a_removed_diff_is_a_tombstone() {
    let p = Projections::default();
    let mut hub = hub(64);
    populate(&p);
    hub.poll(&p);
    let attached = hub.attach(None, None).unwrap();
    let Taken::Frames(frames) = attached.outbox.take() else { panic!() };
    let mut mirror = Mirror::default();
    mirror.apply(&wire::decode(&frames[0]).unwrap()).unwrap();
    p.publish_sessions(Vec::new());
    p.publish_plans(Vec::new());
    p.publish_approvals(Vec::new());
    p.publish_diffs(Vec::new());
    hub.poll(&p);
    let Taken::Frames(frames) = attached.outbox.take() else { panic!() };
    for frame in frames {
        mirror.apply(&wire::decode(&frame).unwrap()).unwrap();
    }
    assert!(board_matches(&mirror, &p));
    assert_eq!(mirror.cursor, hub.head());
}

#[test]
fn an_idle_client_is_pinged_and_a_silent_one_is_closed() {
    let mut cfg = config();
    cfg.session.ping_interval = Duration::from_millis(100);
    cfg.session.idle_timeout = Duration::from_millis(600);
    let f = start_with(cfg);
    let mut client = TestClient::connect(f.server.addr(), f.server.token());
    client.hello();
    let mut pinged = false;
    while let Some(envelope) = client.next() {
        pinged |= matches!(envelope.message, Message::Ping { .. });
    }
    assert!(pinged, "the host pings an idle client");
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while f.server.active_clients() > 0 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn the_client_limit_is_enforced() {
    let mut cfg = config();
    cfg.max_clients = 1;
    let f = start_with(cfg);
    let first = TestClient::connect(f.server.addr(), f.server.token());
    let deadline = Instant::now() + wait(Duration::from_secs(5));
    while f.server.active_clients() < 1 {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(TestClient::try_connect(f.server.addr(), Some(f.server.token()), None, true).is_err());
    drop(first);
}

#[test]
fn the_advertisement_is_private_and_goes_away_with_the_server() {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::test_support::tempdir().unwrap();
    let path = dir.path().join("state").join("protocol.json");
    let mut f = start_with(ServerConfig { advertise: Some(path.clone()), ..config() });
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    let advert: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(advert["token"], f.server.token());
    assert_eq!(advert["url"], format!("ws://{}", f.server.addr()));
    let addr = f.server.addr();
    f.server.stop();
    assert!(!path.exists());
    assert!(TcpStream::connect(addr).is_err(), "the listener is closed");
}
