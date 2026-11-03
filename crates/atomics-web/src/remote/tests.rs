//! The remote Backend's reads and writes, fed frames directly.

use atomics_core::approvals::Decision;
use atomics_core::ops::{CommandResult, Outcome};
use atomics_core::wire::{Limits, ResetReason, Resume, Revisions, WireError};

use super::*;
use crate::fake_host::{self, WORKTREE};

const EPOCH: &str = "e1";

fn welcome(role: Role) -> Envelope {
    let capabilities: Vec<String> = CAPABILITIES.iter().map(|c| c.to_string()).collect();
    let may = role.may(&capabilities);
    let resume = Resume::Reset { cursor: Seq(3), reason: ResetReason::NewClient };
    Envelope::host(
        EPOCH,
        Message::Welcome { version: VERSION, capabilities, limits: Limits::default(), role, resume, may },
    )
}

fn snapshot() -> Envelope {
    let board = fake_host::board();
    Envelope::host(
        EPOCH,
        Message::StateSnapshot {
            cursor: Seq(3),
            revisions: Revisions { sessions: Seq(1), plans: Seq(1), approvals: Seq(1), diffs: Seq(1) },
            sessions: board.sessions,
            plans: board.plans,
            approvals: board.approvals,
            diffs: board.diffs,
            part: None,
        },
    )
}

fn connected(role: Role) -> RemoteBackend {
    let remote = RemoteBackend::new("op");
    remote.set_now(10_000);
    remote.receive(welcome(role));
    remote.receive(snapshot());
    remote
}

#[test]
fn the_board_reads_as_the_desktop_draws_it() {
    let remote = connected(Role::Controller);
    let plans = remote.plan_list();
    assert_eq!(plans.len(), 1);
    let row = &plans[0];
    assert_eq!((row.id.as_str(), row.tab_name.as_str(), row.done, row.total), ("p1", "Parse faster", 0, 2));
    assert_eq!(row.state, PlanState::NeedsYou, "its agent waits on an approval");
    assert_eq!(remote.plan_row(0), Some(row.clone()));
    assert_eq!(remote.plan_digest(0).as_deref(), Some("digest-1"));

    let board = remote.board(0).expect("a board");
    assert_eq!(board.plan.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t1", "t2"]);
    assert!(board.ready.is_empty(), "t2 waits for t1");
    assert_eq!(board.sessions.get("s1").map(|(title, _)| title.as_str()), Some("Fix the parser"));
    assert_eq!(board.now, 10, "the board's clock is this client's");

    let sessions = remote.session_list();
    assert_eq!(sessions.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["chief", "s1"]);
    let s1 = &sessions[1];
    assert_eq!((s1.root.to_str(), s1.ungrouped, s1.changes), (Some(WORKTREE), false, Some((12, 4))));
    assert!(sessions[0].chief && remote.session_approval(0).is_none());
    let approval = remote.session_approval(1).expect("s1's approval");
    assert_eq!((approval.session.as_str(), approval.id.as_str(), approval.request.tool.as_str()), ("s1", "a1", "Bash"));

    assert_eq!(remote.diff_roots(), [PathBuf::from(WORKTREE)]);
    let heading = remote.diff_heading(Path::new(WORKTREE));
    assert_eq!(
        (heading.title.as_str(), heading.branch.as_deref(), heading.can_send),
        ("Fix the parser", Some("atomics/t1"), false)
    );
    assert_eq!(remote.diff_heading(Path::new("/w/other")).title, "other");
    assert_eq!(remote.visible_sessions(), [0, 1]);
}

#[test]
fn commands_become_the_mutations_welcome_allows() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Deny });
    remote.send(Command::ApprovePlan(0));
    let sent: Vec<Message> = remote.take_outgoing().into_iter().map(|e| e.message).collect();
    let [Message::ApprovalDecide(decide), Message::PlanCommand(approve)] = sent.as_slice() else { panic!("{sent:?}") };
    assert_eq!((decide.op.as_str(), decide.approval.as_str(), decide.decision), ("op-1", "a1", Decision::Deny));
    assert_eq!(
        (approve.op.as_str(), approve.plan.as_deref(), approve.digest.as_deref()),
        ("op-2", Some("p1"), Some("digest-1"))
    );

    // The host refused the decision: the banner may offer it again, and the notice says why.
    let refused = Outcome::refused(WireError::new(ErrorCode::AlreadyResolved, "allowed on the desktop"));
    remote
        .receive(Envelope::host(EPOCH, Message::CommandResult(CommandResult { op: "op-1".into(), outcome: refused })));
    assert_eq!(remote.take_refused(), [("s1".to_string(), "a1".to_string())]);
    assert!(remote.notice().unwrap().contains("already answered"));
    let done = Outcome::done(serde_json::Value::Null);
    remote.receive(Envelope::host(EPOCH, Message::CommandResult(CommandResult { op: "op-2".into(), outcome: done })));
    assert!(remote.take_refused().is_empty());
}

#[test]
fn nothing_is_sent_offline_or_without_a_grant() {
    let offline = RemoteBackend::new("op");
    offline.send(Command::ApprovePlan(0));
    assert!(offline.take_outgoing().is_empty());
    assert!(offline.notice().unwrap().contains("Not connected"));

    let viewer = connected(Role::Viewer);
    viewer.take_outgoing();
    viewer.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    assert!(viewer.take_outgoing().is_empty());
    assert_eq!(viewer.take_refused(), [("s1".to_string(), "a1".to_string())]);
    assert!(!viewer.may(Grant::Approve));
}

#[test]
fn operations_waiting_for_the_host_are_bounded() {
    let remote = connected(Role::Controller);
    for _ in 0..wire::MAX_PENDING_OPS {
        remote.send(Command::ApprovePlan(0));
    }
    assert_eq!(remote.take_outgoing().len(), wire::MAX_PENDING_OPS);
    assert_eq!(remote.notice(), None);
    remote.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    assert!(remote.take_outgoing().is_empty(), "no more than the host takes at once");
    assert!(remote.notice().unwrap().contains("Too many changes"));
    assert_eq!(remote.take_refused(), [("s1".to_string(), "a1".to_string())], "the banner may offer it again");
    // An answer makes room.
    let done = Outcome::done(serde_json::Value::Null);
    remote.receive(Envelope::host(EPOCH, Message::CommandResult(CommandResult { op: "op-1".into(), outcome: done })));
    remote.send(Command::ApprovePlan(0));
    assert_eq!(remote.take_outgoing().len(), 1);
}

#[test]
fn operations_in_flight_are_sent_again_after_a_reconnect_while_young() {
    let remote = connected(Role::Controller);
    remote.send(Command::ApprovePlan(0));
    remote.disconnected();
    assert!(remote.take_outgoing().is_empty(), "nothing waits to go out on a closed channel");
    let hello = remote.hello("atomics-web");
    let Message::Hello { prior_epoch, prior_cursor, .. } = hello.message else { panic!("hello") };
    assert_eq!((prior_epoch.as_deref(), prior_cursor), (Some(EPOCH), Some(Seq(3))));

    remote.receive(welcome(Role::Controller));
    let again: Vec<Message> = remote.take_outgoing().into_iter().map(|e| e.message).collect();
    assert!(matches!(again.as_slice(), [Message::PlanCommand(c), ..] if c.op == "op-1"), "the same op: {again:?}");

    remote.receive(snapshot());

    // One too old to send again is the user's to check.
    remote.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    remote.disconnected();
    remote.set_now(10_000 + u64::from(ops::OP_TTL_SECS) * 1000);
    remote.receive(welcome(Role::Controller));
    assert!(remote.take_outgoing().iter().all(|e| !matches!(e.message, Message::ApprovalDecide(_))));
    assert!(remote.notice().unwrap().contains("may not have reached"));
    assert_eq!(remote.take_refused(), [("s1".to_string(), "a1".to_string())]);
}

#[test]
fn a_file_the_host_cannot_read_shows_as_an_error() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.want_diff(Path::new(WORKTREE));
    let asked: Vec<Envelope> = remote.take_outgoing();
    assert_eq!(asked.len(), 2);
    let id = asked[0].request_id.clone();
    remote.receive(WireError::new(ErrorCode::NotFound, "gone").into_envelope(id, Some(EPOCH.into())));
    let diff = remote.diff(Path::new(WORKTREE)).unwrap();
    assert!(diff.loading, "the other file is still on its way");
    assert_eq!(diff.error.as_deref(), Some("1 file(s) could not be read from the host"));
    assert_eq!(remote.notice(), None, "a diff error is the diff's, not a notice");
}

#[test]
fn refresh_fetches_again_and_other_commands_are_declined() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    remote.send(Command::RefreshDiff(WORKTREE.into()));
    assert_eq!(remote.take_outgoing().len(), 2, "refresh reads the files again");
    remote.send(Command::IntegrateAccepted(0));
    assert!(remote.take_outgoing().is_empty());
    assert!(remote.notice().unwrap().contains("not available"));
}

#[test]
fn archive_finished_is_not_sent_as_unconditional_archive() {
    let remote = connected(Role::Controller);
    remote.take_outgoing();
    assert_eq!(Command::ArchiveFinished(0).remote(&remote, &mut || "op".into()), atomics_ui::Remote::Unsupported);
    remote.send(Command::ArchiveFinished(0));
    assert!(remote.take_outgoing().is_empty());
    assert!(remote.notice().unwrap().contains("not available"));
}
