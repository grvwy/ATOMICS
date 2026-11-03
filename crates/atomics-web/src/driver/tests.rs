//! The frame's whole life against a fake host: boot, pairing, the board, commands, reconnecting and the
//! ways it stops. The "network" is synchronous: a message sent is received before the next is sent.

use atomics_core::approvals::Decision;
use atomics_core::noise::Access;
use atomics_core::session::AgentStatus;
use atomics_core::wire::{Message, Seq};
use atomics_ui::{Backend, Command};

use super::*;
use crate::fake_host::{self, HOST_ID, WORKTREE};
use crate::world::{BROKER, PARENT, World};

#[test]
fn pairing_shows_the_hosts_code_then_opens_the_board() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    assert_eq!(world.posted, [boot::READY], "the frame asks for its boot at once");
    assert_eq!(world.driver.status(), &Status::Waiting);

    world.boot(Some(&pairing));
    assert_eq!(world.posted, [boot::READY, boot::BOOTED]);
    let Status::Pairing { code: Some(code) } = world.driver.status().clone() else {
        panic!("the code shows: {:?}", world.driver.status())
    };
    assert_eq!(Some(&code), world.host.code.as_ref(), "both screens show the same six digits");
    assert_eq!(world.host.named.as_deref(), Some("Safari on iPhone"), "the host's prompt names the browser");
    assert!(world.store.is_empty(), "nothing is kept before the host accepts the device");
    assert!(world.driver.remote.plan_list().is_empty());

    let answer = world.host.confirm();
    world.deliver(answer);
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.posted.last(), Some(&boot::PAIRED));
    let device = world.store.get(HOST_ID).expect("the device key is kept");
    assert_eq!(device.host_key, world.host.public());
    assert_eq!(world.host.allowed.iter().map(|(key, _)| *key).collect::<Vec<_>>(), [device.secret.public()]);

    // The channel goes on as the session: hello, then the board.
    let Message::Hello { client_id, capabilities, prior_epoch, .. } = &world.sent()[0] else { panic!("hello first") };
    assert_eq!((client_id.as_str(), prior_epoch), ("atomics-web", &None));
    assert!(capabilities.iter().any(|c| c == "mutations") && !capabilities.iter().any(|c| c == "terminals"));
    let plans = world.driver.remote.plan_list();
    assert_eq!(plans.iter().map(|p| p.goal.as_str()).collect::<Vec<_>>(), ["Parse faster"]);
    let welcome = world.driver.remote.welcome().expect("welcomed");
    assert!(welcome.may.contains(&atomics_core::ops::Grant::Approve), "a controller may approve");
}

#[test]
fn the_ticket_travels_only_in_the_subprotocol() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    assert_eq!(world.dials, [vec!["atomics.rendezvous.v1".to_string(), "atomics.ticket.ticket-1".to_string()]]);
    assert_eq!(world.driver.status(), &Status::Live);
    let shown = format!(
        "{:?}",
        Output::Dial { socket: 1, url: BROKER.into(), protocols: vec![Zeroizing::new("atomics.ticket.secret".into())] }
    );
    assert!(!shown.contains("secret"), "{shown}");
    assert!(!format!("{:?}", world.driver.boot()).contains("ticket-1"));
}

#[test]
fn messages_from_elsewhere_are_refused_unread() {
    let mut world = World::paired(fake_host::host());
    let data = world.boot_data(None);
    assert_eq!(world.driver.message("https://evil.example", true, &data), Err(Refused::Origin));
    assert_eq!(world.driver.message(PARENT, false, &data), Err(Refused::Window));
    world.pump();
    assert_eq!(world.driver.status(), &Status::Waiting);
    assert!(world.dials.is_empty());
    // With no parent origin configured, nothing boots the frame and it says nothing.
    let mut orphan = Driver::new(None, "op", "x");
    assert!(orphan.take_outputs().is_empty());
    assert_eq!(orphan.message(PARENT, true, &data), Err(Refused::Origin));
    assert_eq!(orphan.status(), &Status::Stopped(Stop::NoParent));
}

#[test]
fn a_paired_browser_reconnects_with_a_fresh_ticket_and_resumes() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let cursor = world.driver.remote.with_mirror(|m| m.cursor);

    world.drop_socket(close::ABNORMAL);
    assert!(world.driver.remote.welcome().is_none(), "offline: nothing may be sent");
    assert_eq!(world.driver.remote.plan_list().len(), 1, "the board stays on screen while away");
    // The host goes on meanwhile.
    let mut session = fake_host::board().sessions[1].clone();
    session.status = AgentStatus::Working;
    world.host.event(|cursor| Message::SessionUpsert { cursor, session });
    world.host.received.clear();

    // The retry asks the hosting page for a fresh ticket; nothing dials without one.
    world.advance(world.driver.wake_in().expect("a retry is due"));
    assert_eq!(world.posted.last(), Some(&boot::WANTS_TICKET));
    assert_eq!(world.dials.len(), 1);
    world.ticket("ticket-2");
    assert_eq!(world.dials.len(), 2);
    assert_eq!(world.dials[1][1], "atomics.ticket.ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    let Message::Hello { prior_epoch, prior_cursor, .. } = &world.sent()[0] else { panic!("hello") };
    assert_eq!((prior_epoch.as_deref(), *prior_cursor), (Some("epoch-1"), Some(Seq(cursor))), "it resumes");
    assert!(
        !world.sent().iter().any(|m| matches!(m, Message::Resync { .. })),
        "the replay follows on: no snapshot needed"
    );
    let s1 = world.driver.remote.session_list().into_iter().find(|s| s.id == "s1").unwrap();
    assert_eq!(s1.status, AgentStatus::Working, "what happened while away was replayed");
}

#[test]
fn a_host_that_restarted_sends_a_snapshot() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.drop_socket(close::GOING_AWAY);
    world.host.epoch = "epoch-2".into();
    world.host.board.plans[0].goal = "Parse much faster".into();
    world.advance(world.driver.wake_in().unwrap());
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    assert_eq!(world.driver.remote.with_mirror(|m| m.epoch.clone()).as_deref(), Some("epoch-2"));
    assert_eq!(world.driver.remote.plan_list()[0].goal, "Parse much faster");
}

#[test]
fn a_steady_channel_starts_the_waits_over_and_quota_waits_longer() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.drop_socket(close::ABNORMAL);
    let first = world.driver.wake_in().unwrap();
    assert!((FIRST_RETRY_MS / 2..=FIRST_RETRY_MS).contains(&first), "{first}");
    world.advance(first);
    world.ticket("ticket-2");
    world.drop_socket(close::ABNORMAL);
    let second = world.driver.wake_in().unwrap();
    assert!((FIRST_RETRY_MS..=2 * FIRST_RETRY_MS).contains(&second), "the wait doubles: {second}");
    world.advance(second);
    world.ticket("ticket-3");
    world.advance(STEADY_MS);
    world.drop_socket(close::SLOW);
    assert!(world.driver.wake_in().unwrap() <= FIRST_RETRY_MS, "after a steady channel the waits start over");
    world.advance(world.driver.wake_in().unwrap());
    world.ticket("ticket-4");
    world.drop_socket(close::QUOTA);
    assert!(world.driver.wake_in().unwrap() >= QUOTA_RETRY_MS / 2);
    // An unanswered ticket request is asked again.
    world.advance(world.driver.wake_in().unwrap());
    let asked = world.posted.iter().filter(|p| **p == boot::WANTS_TICKET).count();
    world.advance(TICKET_WAIT_MS);
    assert_eq!(world.posted.iter().filter(|p| **p == boot::WANTS_TICKET).count(), asked + 1);
}

#[test]
fn approving_goes_out_where_the_welcome_grants_it() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let remote = &world.driver.remote;
    let index = remote.session_list().iter().position(|s| s.id == "s1").unwrap();
    let approval = remote.session_approval(index).expect("s1 waits on a1");
    assert_eq!((approval.id.as_str(), approval.request.command.as_deref()), ("a1", Some("cargo test")));

    world.send(Command::Decide { session: index, approval: "a1".into(), decision: Decision::Allow });
    let Some(Message::ApprovalDecide(decide)) = world.sent().last().copied() else { panic!("approval_decide") };
    assert_eq!((decide.approval.as_str(), decide.decision), ("a1", Decision::Allow));
    assert!(decide.op.starts_with("op-"));
    assert!(world.driver.remote.session_approval(index).is_none(), "the host's approval_resolved cleared it");

    world.send(Command::ApprovePlan(0));
    let Some(Message::PlanCommand(command)) = world.sent().last().copied() else { panic!("plan_command") };
    assert_eq!((command.plan.as_deref(), command.digest.as_deref()), (Some("p1"), Some("digest-1")));
    assert!(world.driver.remote.board(0).unwrap().plan.approved);
    assert_eq!(world.driver.remote.notice(), None);
}

#[test]
fn a_view_only_device_sends_no_mutation() {
    let mut host = fake_host::host();
    host.answer = Some(Access::View);
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    assert_eq!(world.driver.status(), &Status::Live);
    assert!(world.driver.remote.welcome().unwrap().may.is_empty());
    let index = world.driver.remote.session_list().iter().position(|s| s.id == "s1").unwrap();
    let before = world.host.received.len();
    world.send(Command::Decide { session: index, approval: "a1".into(), decision: Decision::Deny });
    world.send(Command::ApprovePlan(0));
    assert_eq!(world.host.received.len(), before, "nothing reaches the host");
    assert!(world.driver.remote.notice().unwrap().contains("only view"));
    assert_eq!(
        world.driver.remote.take_refused(),
        [("s1".to_string(), "a1".to_string())],
        "the banner may offer it again"
    );
}

#[test]
fn what_the_browser_does_not_offer_yet_says_so() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    let before = world.host.received.len();
    for command in [
        Command::DispatchReady(0),
        Command::Steer { sessions: vec!["s1".into()], text: "go on".into() },
        Command::SendReview(WORKTREE.into()),
        Command::CloseSession("s1".into()),
    ] {
        world.driver.remote.dismiss_notice();
        world.send(command);
        assert!(world.driver.remote.notice().unwrap().contains("not available in the browser"));
    }
    assert_eq!(world.host.received.len(), before);
}

#[test]
fn diffs_are_fetched_page_by_page_while_on_screen() {
    let mut host = fake_host::host();
    host.page_budget = 1024;
    let mut world = World::paired(host);
    world.boot(None);
    let root = std::path::Path::new(WORKTREE);
    let before = world.driver.remote.diff(root).unwrap();
    assert_eq!(before.revision, 4);
    assert!(!before.loading, "nothing is fetched until the diff is on screen");
    let files = &before.prepared.as_ref().unwrap().diff.files;
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f.hunks.is_empty()));

    world.driver.remote.want_diff(root);
    world.driver.flush();
    world.pump();
    let gets = world.sent().iter().filter(|m| matches!(m, Message::DiffGet { .. })).count();
    assert!(gets > 2, "the long file came in several pages ({gets} requests)");
    let after = world.driver.remote.diff(root).unwrap();
    assert!(!after.loading && after.error.is_none());
    let prepared = after.prepared.unwrap();
    let expected = [fake_host::file_diff("src/a.rs", 6), fake_host::file_diff("src/b.rs", 40)];
    for (got, want) in prepared.diff.files.iter().zip(&expected) {
        let lines = |f: &atomics_core::diff::FileDiff| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
        assert_eq!(lines(got), lines(want), "{}: every line arrived", want.path());
        assert_eq!(got.hunks.len(), 1, "a hunk cut between pages is put back together");
    }
    assert_eq!((prepared.additions, prepared.deletions), (12, 4), "the counts are the summary's");

    // A new revision fetches again.
    let mut summary = world.host.board.diffs[0].clone();
    summary.revision = 5;
    world.host.received.clear();
    let messages = world.host.event(|cursor| Message::DiffSummary { cursor, summary });
    world.deliver(messages);
    assert!(world.sent().iter().any(|m| matches!(m, Message::DiffGet { revision: Some(Seq(5)), .. })));
}

#[test]
fn a_gap_asks_for_one_resync() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.host.received.clear();
    let head = world.host.head();
    let session = fake_host::board().sessions[0].clone();
    let skipped = world.host.stray(Message::SessionUpsert { cursor: Seq(head + 5), session: session.clone() });
    let again = world.host.stray(Message::SessionUpsert { cursor: Seq(head + 6), session });
    world.deliver(skipped.into_iter().chain(again).collect());
    let resyncs = world.sent().iter().filter(|m| matches!(m, Message::Resync { .. })).count();
    assert_eq!(resyncs, 1, "one resync for a run of events after a gap");
    assert_eq!(world.driver.remote.with_mirror(|m| m.cursor), head, "the snapshot answered it");
}

#[test]
fn pings_are_answered() {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    world.host.received.clear();
    let ping = world.host.stray(Message::Ping { nonce: Some("n1".into()) });
    world.deliver(ping);
    assert_eq!(world.sent(), [&Message::Pong { nonce: Some("n1".into()) }]);
}

#[test]
fn it_stops_where_trying_again_cannot_help() {
    // Never paired in this browser.
    let mut world = World::new(fake_host::host());
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotHere));
    assert!(world.dials.is_empty());

    // The host forgot this browser: its key goes too.
    let mut world = World::paired(fake_host::host());
    world.host.allowed.clear();
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NotPaired));
    assert!(world.store.is_empty(), "the stored key is forgotten");

    // The host's user refused the device.
    let mut host = fake_host::host();
    host.answer = None;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::PairRefused));
    assert!(world.store.is_empty());

    // A link whose psk the host does not hold.
    let mut host = fake_host::host();
    let stale = host.open_pairing();
    host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&stale));
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::PairFailed));

    // No pairing open on the host.
    let mut host = fake_host::host();
    let used = host.open_pairing();
    host.pairing = None;
    let mut world = World::new(host);
    world.boot(Some(&used));
    assert_eq!(world.driver.status(), &Status::Stopped(Stop::NoPairing));
}
