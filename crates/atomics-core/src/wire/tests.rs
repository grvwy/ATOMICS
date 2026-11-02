use super::*;
use crate::diff::{FileDiff, FileStatus};
use crate::plan::{PlanStatus, TaskStatus};
use crate::projection::{DiffFileSummary, Projections, TaskInfo, TaskLink};
use crate::session::AgentStatus;

fn session() -> SessionInfo {
    SessionInfo {
        id: "s1".into(),
        agent: "claude".into(),
        title: "fix".into(),
        cwd: "/w/a".into(),
        worktree: Some("/w/a".into()),
        branch: Some("fix/a".into()),
        status: AgentStatus::NeedsInput,
        chief: false,
        task: Some(TaskLink { plan: "p".into(), task: "t".into() }),
        pending_approvals: 1,
    }
}

fn plan() -> PlanInfo {
    PlanInfo {
        id: "p".into(),
        status: PlanStatus::Open,
        goal: "ship".into(),
        project: Some("/w".into()),
        approved: true,
        integration: None,
        tasks: vec![TaskInfo {
            id: "t".into(),
            title: "do".into(),
            status: TaskStatus::Review,
            agent: "claude".into(),
            depends_on: vec![],
            session: Some("s1".into()),
            branch: None,
            integrated: None,
            additions: 3,
            deletions: 1,
            report: Some(report()),
        }],
    }
}

fn report() -> ReportInfo {
    ReportInfo { status: "done".into(), summary: "ok".into(), at: 9 }
}

fn approval() -> ApprovalInfo {
    ApprovalInfo {
        id: "a1".into(),
        session: "s1".into(),
        tool: "Bash".into(),
        command: Some("ls".into()),
        cwd: "/w/a".into(),
        reason: None,
    }
}

fn summary() -> DiffSummary {
    DiffSummary {
        worktree: "/w/a".into(),
        revision: 4,
        base_ref: "main".into(),
        base: "abc".into(),
        partial: false,
        files: vec![DiffFileSummary {
            old_path: None,
            new_path: Some("x.rs".into()),
            status: FileStatus::Added,
            binary: false,
            additions: 2,
            deletions: 0,
            omitted: 0,
        }],
    }
}

fn file_body() -> DiffFileBody {
    DiffFileBody {
        worktree: "/w/a".into(),
        revision: 4,
        file: FileDiff {
            old_path: None,
            new_path: Some("x.rs".into()),
            status: FileStatus::Added,
            binary: false,
            hunks: vec![],
            omitted: 0,
        },
    }
}

fn every_message() -> Vec<Message> {
    let topics = vec![Topic::Diff { worktree: "/w/a".into() }, Topic::Plan { id: "p".into() }];
    vec![
        Message::Hello {
            client_id: "c".into(),
            versions: vec![VERSION],
            capabilities: vec!["board".into()],
            prior_epoch: Some("e".into()),
            prior_cursor: Some(Seq(u64::MAX)),
        },
        Message::Welcome {
            version: VERSION,
            capabilities: vec!["board".into()],
            limits: Limits::default(),
            role: Role::Controller,
            resume: Resume::Reset { cursor: Seq(7), reason: ResetReason::NewClient },
        },
        Message::Welcome {
            version: VERSION,
            capabilities: vec![],
            limits: Limits::default(),
            role: Role::Viewer,
            resume: Resume::Replay { after: Seq(5) },
        },
        Message::Subscribe { topics: topics.clone() },
        Message::Unsubscribe { topics },
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(2), approvals: Seq(3), diffs: Seq(4) },
            sessions: vec![session()],
            plans: vec![plan()],
            approvals: vec![approval()],
            diffs: vec![summary()],
        },
        Message::SessionUpsert { cursor: Seq(4), session: session() },
        Message::SessionRemoved { cursor: Seq(5), id: "s1".into() },
        Message::PlanUpsert { cursor: Seq(6), plan: plan() },
        Message::PlanRemoved { cursor: Seq(7), id: "p".into() },
        Message::TaskReport { cursor: Seq(8), plan: "p".into(), task: "t".into(), report: report() },
        Message::ApprovalPending { cursor: Seq(9), approval: approval() },
        Message::ApprovalResolved { cursor: Seq(10), id: "a1".into(), disposition: Disposition::AllowedAlways },
        Message::DiffSummary { cursor: Seq(11), summary: summary() },
        Message::DiffGet { worktree: "/w/a".into(), path: "x.rs".into(), revision: Some(Seq(4)) },
        Message::DiffFile { body: file_body() },
        Message::FileGet { worktree: "/w/a".into(), path: "x.rs".into() },
        Message::FileContent {
            worktree: "/w/a".into(),
            path: "x.rs".into(),
            content: "fn a() {}\n".into(),
            truncated: false,
        },
        Message::Ack { cursor: Seq(11) },
        Message::Resync { reason: Some("gap".into()) },
        Message::Ping { nonce: Some("n".into()) },
        Message::Pong { nonce: None },
        Message::Error { code: ErrorCode::StaleRevision, message: "stale".into() },
    ]
}

#[test]
fn every_message_round_trips_through_its_frame() {
    let messages = every_message();
    let kinds: std::collections::BTreeSet<_> =
        messages.iter().map(|m| serde_json::to_value(m).unwrap()["kind"].as_str().unwrap().to_string()).collect();
    assert_eq!(kinds.len(), KINDS.len(), "a kind has no round-trip case");
    assert!(KINDS.iter().all(|k| kinds.contains(*k)));
    for message in messages {
        let envelope = Envelope { v: VERSION, epoch: Some("e1".into()), request_id: Some("r1".into()), message };
        let frame = encode(&envelope).unwrap();
        assert_eq!(decode(&frame).unwrap(), envelope, "{frame}");
    }
}

#[test]
fn cursors_travel_as_strings_and_numbers_are_refused() {
    let frame = encode(&Envelope::new(Message::Ack { cursor: Seq(u64::MAX) })).unwrap();
    assert!(frame.contains(r#""cursor":"18446744073709551615""#), "{frame}");
    for bad in [
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":5}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":""}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":"-1"}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":"1.5"}"#,
        r#"{"v":{"major":1,"minor":0},"kind":"ack","cursor":"18446744073709551616"}"#,
    ] {
        assert_eq!(decode(bad).unwrap_err().code, ErrorCode::Malformed, "{bad}");
    }
}

#[test]
fn malformed_frames_are_rejected_with_a_typed_error() {
    let v = r#""v":{"major":1,"minor":0}"#;
    let cases = [
        "".to_string(),
        "not json".into(),
        "[]".into(),
        "null".into(),
        "42".into(),
        "{".into(),
        format!("{{{v}}}"),
        format!("{{{v},\"kind\":7}}"),
        r#"{"kind":"ping"}"#.into(),
        r#"{"v":"1.0","kind":"ping"}"#.into(),
        r#"{"v":{"major":-1,"minor":0},"kind":"ping"}"#.into(),
        // Right kind, wrong or missing fields.
        format!("{{{v},\"kind\":\"ack\"}}"),
        format!("{{{v},\"kind\":\"session_removed\",\"cursor\":\"1\",\"id\":5}}"),
        format!("{{{v},\"kind\":\"subscribe\",\"topics\":[{{\"topic\":\"nope\"}}]}}"),
        format!("{{{v},\"kind\":\"hello\",\"client_id\":\"c\",\"versions\":[]}}"),
        format!("{{{v},\"kind\":\"error\",\"code\":\"made_up\",\"message\":\"x\"}}"),
    ];
    for case in cases {
        let error = decode(&case).unwrap_err();
        assert_eq!(error.code, ErrorCode::Malformed, "{case}: {error}");
    }
}

#[test]
fn deeply_nested_input_is_an_error_not_a_stack_overflow() {
    let frame = format!("{}{}", "[".repeat(10_000), "]".repeat(10_000));
    assert_eq!(decode(&frame).unwrap_err().code, ErrorCode::Malformed);
}

#[test]
fn truncating_any_valid_frame_never_panics_and_never_decodes() {
    for message in every_message() {
        let frame = encode(&Envelope::new(message)).unwrap();
        for end in (0..frame.len()).filter(|i| frame.is_char_boundary(*i)) {
            assert!(decode(&frame[..end]).is_err(), "{}", &frame[..end]);
        }
    }
}

#[test]
fn an_unsupported_major_is_rejected_before_the_body_is_read() {
    // The body here would also be malformed; the version is what is reported.
    let frame = r#"{"v":{"major":2,"minor":0},"kind":"ack","cursor":"not a cursor"}"#;
    assert_eq!(decode(frame).unwrap_err().code, ErrorCode::UnsupportedVersion);
    let frame = r#"{"v":{"major":0,"minor":9},"kind":"ping"}"#;
    assert_eq!(decode(frame).unwrap_err().code, ErrorCode::UnsupportedVersion);
}

#[test]
fn a_newer_minor_decodes_and_unknown_fields_are_ignored() {
    let frame = r#"{"v":{"major":1,"minor":7,"extra":1},"kind":"ping","nonce":"n","future":{"a":[1]}}"#;
    let envelope = decode(frame).unwrap();
    assert_eq!(envelope.message, Message::Ping { nonce: Some("n".into()) });
    assert_eq!(envelope.v, Version { major: 1, minor: 7 });
}

#[test]
fn an_unknown_kind_fails_explicitly() {
    let frame = r#"{"v":{"major":1,"minor":0},"kind":"approval_decide","id":"a1"}"#;
    let error = decode(frame).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnknownKind);
    assert!(error.message.contains("approval_decide"));
}

#[test]
fn hello_with_another_major_decodes_so_it_can_be_negotiated() {
    let frame = r#"{"v":{"major":2,"minor":0},"kind":"hello","client_id":"c","versions":[{"major":2,"minor":0},{"major":1,"minor":3}]}"#;
    let Message::Hello { versions, .. } = decode(frame).unwrap().message else { panic!("hello") };
    assert_eq!(negotiate(&versions, &[VERSION]).unwrap(), VERSION, "minor is clamped to the host's");
    let only_future = [Version { major: 2, minor: 0 }];
    let error = negotiate(&only_future, &[VERSION]).unwrap_err();
    assert_eq!(error.code, ErrorCode::UnsupportedVersion);
    assert!(error.message.contains("1.0"));
    // The refusal is itself a frame the client can read.
    let reply = encode(&error.into_envelope(Some("r".into()), Some("e".into()))).unwrap();
    assert_eq!(decode(&reply).unwrap().request_id.as_deref(), Some("r"));
}

#[test]
fn negotiation_takes_the_highest_shared_major_and_the_lower_minor() {
    let host = [Version { major: 1, minor: 4 }, Version { major: 2, minor: 1 }];
    let pick = |c: &[Version]| negotiate(c, &host).unwrap();
    assert_eq!(pick(&[Version { major: 1, minor: 9 }]), Version { major: 1, minor: 4 });
    assert_eq!(pick(&[Version { major: 1, minor: 2 }]), Version { major: 1, minor: 2 });
    assert_eq!(pick(&[Version { major: 1, minor: 2 }, Version { major: 2, minor: 0 }]), Version { major: 2, minor: 0 });
    assert_eq!(common_capabilities(&["x".into(), "diffs".into(), "board".into()], CAPABILITIES), ["board", "diffs"]);
}

#[test]
fn oversize_frames_are_rejected_before_parsing_and_never_encoded() {
    let big = "x".repeat(MAX_FRAME_BYTES);
    let frame = format!(r#"{{"v":{{"major":1,"minor":0}},"kind":"ping","nonce":"{big}"}}"#);
    assert!(frame.len() > MAX_FRAME_BYTES);
    assert_eq!(decode(&frame).unwrap_err().code, ErrorCode::Oversize);
    // Oversize wins even over invalid JSON: nothing is parsed.
    assert_eq!(decode(&"{".repeat(MAX_FRAME_BYTES + 1)).unwrap_err().code, ErrorCode::Oversize);
    let message = Message::FileContent { worktree: "w".into(), path: "p".into(), content: big, truncated: false };
    assert_eq!(encode(&Envelope::new(message)).unwrap_err().code, ErrorCode::Oversize);
    // A frame exactly at the limit is allowed.
    let head = r#"{"v":{"major":1,"minor":0},"kind":"ping","nonce":""#;
    let exact = format!("{head}{}\"}}", "y".repeat(MAX_FRAME_BYTES - head.len() - 2));
    assert_eq!(exact.len(), MAX_FRAME_BYTES);
    assert!(decode(&exact).is_ok());
}

#[test]
fn collection_and_identifier_bounds_are_enforced() {
    let many = vec![Topic::Plan { id: "p".into() }; MAX_SUBSCRIPTIONS + 1];
    let frame = serde_json::to_string(&Envelope::new(Message::Subscribe { topics: many })).unwrap();
    assert_eq!(decode(&frame).unwrap_err().code, ErrorCode::TooMany);
    let ok = vec![Topic::Plan { id: "p".into() }; MAX_SUBSCRIPTIONS];
    assert!(decode(&serde_json::to_string(&Envelope::new(Message::Subscribe { topics: ok })).unwrap()).is_ok());
    let long = Message::DiffGet { worktree: "w".into(), path: "p".repeat(MAX_ID_BYTES + 1), revision: None };
    assert_eq!(decode(&serde_json::to_string(&Envelope::new(long)).unwrap()).unwrap_err().code, ErrorCode::Oversize);
    let mut envelope = Envelope::new(Message::Ping { nonce: None });
    envelope.request_id = Some("r".repeat(MAX_ID_BYTES + 1));
    assert_eq!(decode(&serde_json::to_string(&envelope).unwrap()).unwrap_err().code, ErrorCode::Oversize);
    let hello = Message::Hello {
        client_id: "c".into(),
        versions: vec![VERSION; MAX_VERSIONS + 1],
        capabilities: vec![],
        prior_epoch: None,
        prior_cursor: None,
    };
    assert_eq!(decode(&serde_json::to_string(&Envelope::new(hello)).unwrap()).unwrap_err().code, ErrorCode::TooMany);
}

#[test]
fn resume_replays_inside_the_ring_and_resets_otherwise() {
    let (e, head, oldest) = ("e2", Seq(100), Seq(60));
    let decide = |epoch: Option<&str>, cursor: Option<u64>| decide_resume(e, head, oldest, epoch, cursor.map(Seq));
    assert_eq!(decide(Some("e2"), Some(100)), Resume::Replay { after: Seq(100) }, "caught up");
    assert_eq!(decide(Some("e2"), Some(60)), Resume::Replay { after: Seq(60) });
    assert_eq!(decide(Some("e2"), Some(59)), Resume::Replay { after: Seq(59) }, "event 60 is the next one needed");
    let reset = |reason| Resume::Reset { cursor: head, reason };
    assert_eq!(decide(Some("e2"), Some(58)), reset(ResetReason::CursorTooOld));
    assert_eq!(decide(Some("e2"), Some(101)), reset(ResetReason::CursorAhead));
    assert_eq!(decide(Some("e1"), Some(90)), reset(ResetReason::EpochChanged), "host restarted");
    assert_eq!(decide(None, None), reset(ResetReason::NewClient));
    assert_eq!(decide(Some("e2"), None), reset(ResetReason::NewClient));
    // An empty ring (oldest = head + 1) replays only for a client that is caught up.
    assert_eq!(decide_resume(e, Seq(5), Seq(6), Some(e), Some(Seq(5))), Resume::Replay { after: Seq(5) });
    assert!(matches!(decide_resume(e, Seq(5), Seq(6), Some(e), Some(Seq(3))), Resume::Reset { .. }));
}

#[test]
fn a_state_snapshot_carries_the_cursor_and_revisions_it_was_loaded_at() {
    let p = Projections::default();
    p.publish_sessions(vec![session()]);
    p.publish_plans(vec![plan()]);
    p.publish_approvals(vec![approval()]);
    p.publish_diffs(vec![summary()]);
    p.publish_sessions(vec![]);
    let envelope = Envelope::snapshot("boot", &p.load());
    let decoded = decode(&encode(&envelope).unwrap()).unwrap();
    assert_eq!(decoded.epoch.as_deref(), Some("boot"));
    let Message::StateSnapshot { cursor, revisions, sessions, plans, approvals, diffs } = decoded.message else {
        panic!("snapshot")
    };
    assert_eq!(cursor, Seq(5));
    assert_eq!(revisions, Revisions { sessions: Seq(2), plans: Seq(1), approvals: Seq(1), diffs: Seq(1) });
    assert_eq!((sessions.len(), plans.len(), approvals.len(), diffs.len()), (0, 1, 1, 1));
}

#[test]
fn errors_are_frames_that_echo_the_request_id() {
    let error = decode("{").unwrap_err();
    let frame = encode(&error.into_envelope(Some("r9".into()), None)).unwrap();
    let envelope = decode(&frame).unwrap();
    assert_eq!(envelope.request_id.as_deref(), Some("r9"));
    assert!(matches!(envelope.message, Message::Error { code: ErrorCode::Malformed, .. }));
}

/// A stand-in for the fuzz target (`fuzz/fuzz_targets/envelope_decode.rs`) that runs with the suite:
/// mutate valid frames at random and require that decoding never panics and that whatever it
/// accepts survives a round trip.
#[test]
fn mutated_frames_never_panic_and_accepted_ones_round_trip() {
    let p = Projections::default();
    p.publish_sessions(vec![session()]);
    p.publish_plans(vec![plan()]);
    let seeds: Vec<String> = [
        Envelope::snapshot("boot", &p.load()),
        Envelope::new(Message::Hello {
            client_id: "c".into(),
            versions: vec![VERSION],
            capabilities: vec!["board".into()],
            prior_epoch: Some("boot".into()),
            prior_cursor: Some(Seq(3)),
        }),
        Envelope::new(Message::DiffGet { worktree: "/w".into(), path: "a.rs".into(), revision: Some(Seq(2)) }),
        Envelope::new(Message::Subscribe { topics: vec![Topic::Diff { worktree: "/w".into() }, Topic::Plan { id: "p".into() }] }),
        Envelope::new(Message::Ping { nonce: Some("n".into()) }).reply_to(Some("r".into())),
    ]
    .iter()
    .map(|e| encode(e).unwrap())
    .collect();

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % n as u64) as usize
    };
    let mut accepted = 0;
    for round in 0..4000 {
        let mut bytes = seeds[round % seeds.len()].clone().into_bytes();
        for _ in 0..=next(4) {
            match next(5) {
                0 if !bytes.is_empty() => {
                    let i = next(bytes.len());
                    bytes[i] ^= 1 << next(8);
                }
                1 if !bytes.is_empty() => bytes.truncate(next(bytes.len())),
                2 if !bytes.is_empty() => {
                    let (a, b) = (next(bytes.len()), next(bytes.len()));
                    let chunk = bytes[a.min(b)..a.max(b)].to_vec();
                    bytes.splice(a.min(b)..a.min(b), chunk);
                }
                3 => bytes.insert(next(bytes.len() + 1), b"{}[]\":,\\0123456789"[next(18)]),
                _ => {}
            }
        }
        let frame = String::from_utf8_lossy(&bytes);
        if let Ok(envelope) = decode(&frame) {
            accepted += 1;
            if let Ok(again) = encode(&envelope) {
                assert_eq!(decode(&again).as_ref(), Ok(&envelope), "{frame}");
            }
        }
    }
    assert!(accepted > 0, "some mutations stay valid");
}
