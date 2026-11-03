//! A host for the tests: the Noise responder of `atomics_core::noise` and a small `atomics.v1` server over
//! one browser channel at a time, holding a board, its event log (for replay) and file diffs. It speaks
//! the protocol the real host does, as far as the web client uses it. It uses atomics-core alone, so the
//! headless-browser test (`tests/browser.rs`) serves the real bundle from it too.

use std::collections::HashMap;

use atomics_core::diff::FileDiff;
use atomics_core::noise::{Access, Channel, Kind, PairAnswer, PairRequest, Pairing, PublicKey, Responder, Secret};
use atomics_core::ops::{CommandResult, Outcome, PlanAction};
use atomics_core::projection::{ApprovalInfo, DiffFileBody, DiffSummary, PlanInfo, SessionInfo};
use atomics_core::tunnel::close;
use atomics_core::wire::{
    self, CAPABILITIES, Disposition, Envelope, ErrorCode, Limits, Message, Resume, Revisions, Seq, VERSION, WireError,
};

pub const HOST_ID: &str = "6f1c2a7e-0b7d-4c55-9f43-3d2a1b0c9e8f";

enum Stage {
    /// Nothing read on this channel yet.
    First,
    /// Pairing: waiting for the browser's `PairRequest`.
    Named(Channel),
    /// Pairing: the request is in; the host's user compares the codes ([`FakeHost::confirm`]).
    Comparing(Channel),
    Open(Channel),
}

#[derive(Default, Clone)]
pub struct Board {
    pub sessions: Vec<SessionInfo>,
    pub plans: Vec<PlanInfo>,
    pub approvals: Vec<ApprovalInfo>,
    pub diffs: Vec<DiffSummary>,
}

pub struct FakeHost {
    secret: Secret,
    pub allowed: Vec<(PublicKey, Access)>,
    pub pairing: Option<Pairing>,
    /// What a pairing's user answers: the device's access, or `None` to refuse it.
    pub answer: Option<Access>,
    /// The code this host showed for the last pairing.
    pub code: Option<String>,
    /// The device name the last pairing asked with.
    pub named: Option<String>,
    /// Wait for [`FakeHost::confirm`] before answering a pairing.
    pub hold: bool,
    stage: Option<Stage>,
    access: Access,
    pub epoch: String,
    pub board: Board,
    /// Every event, by cursor (index + 1).
    pub events: Vec<Envelope>,
    pub files: HashMap<(String, String), FileDiff>,
    /// Bytes of one `diff_file` page.
    pub page_budget: usize,
    /// Every message the browser sent on an open channel.
    pub received: Vec<Message>,
}

impl FakeHost {
    pub fn new(board: Board) -> Self {
        Self {
            secret: Secret::generate().unwrap(),
            allowed: Vec::new(),
            pairing: None,
            answer: Some(Access::Control),
            code: None,
            named: None,
            hold: false,
            stage: None,
            access: Access::View,
            epoch: "epoch-1".into(),
            board,
            events: Vec::new(),
            files: HashMap::new(),
            page_budget: wire::MAX_FRAME_BYTES,
            received: Vec::new(),
        }
    }

    pub fn public(&self) -> PublicKey {
        self.secret.public()
    }

    /// Offer a pairing; its link goes into the boot.
    pub fn open_pairing(&mut self) -> Pairing {
        let pairing = Pairing::new(HOST_ID, self.public()).unwrap();
        self.pairing = Some(pairing.clone());
        pairing
    }

    pub fn head(&self) -> u64 {
        self.events.len() as u64
    }

    pub fn join(&mut self) {
        self.stage = Some(Stage::First);
    }

    pub fn left(&mut self) {
        self.stage = None;
    }

    fn seal(channel: &mut Channel, envelopes: &[Envelope]) -> Vec<Vec<u8>> {
        envelopes.iter().flat_map(|e| channel.seal(wire::encode(e).unwrap().as_bytes()).unwrap()).collect()
    }

    /// Record an event at the next cursor, change the board with it, and return what an open channel sends.
    pub fn event(&mut self, message: impl FnOnce(Seq) -> Message) -> Vec<Vec<u8>> {
        let message = message(Seq(self.head() + 1));
        match &message {
            Message::SessionUpsert { session, .. } => {
                self.board.sessions.retain(|s| s.id != session.id);
                self.board.sessions.push(session.clone());
            }
            Message::PlanUpsert { plan, .. } => {
                self.board.plans.retain(|p| p.id != plan.id);
                self.board.plans.push(plan.clone());
            }
            Message::ApprovalPending { approval, .. } => self.board.approvals.push(approval.clone()),
            Message::ApprovalResolved { id, .. } => self.board.approvals.retain(|a| a.id != *id),
            Message::DiffSummary { summary, .. } => {
                self.board.diffs.retain(|d| d.worktree != summary.worktree);
                self.board.diffs.push(summary.clone());
            }
            _ => {}
        }
        let envelope = Envelope::host(&self.epoch, message);
        self.events.push(envelope.clone());
        match &mut self.stage {
            Some(Stage::Open(channel)) => Self::seal(channel, &[envelope]),
            _ => Vec::new(),
        }
    }

    /// An event's frame without recording it: a gap or a duplicate, for the client to notice.
    pub fn stray(&mut self, message: Message) -> Vec<Vec<u8>> {
        let envelope = Envelope::host(&self.epoch, message);
        match &mut self.stage {
            Some(Stage::Open(channel)) => Self::seal(channel, &[envelope]),
            _ => Vec::new(),
        }
    }

    fn snapshot(&self) -> Envelope {
        let rev = Seq(self.head());
        Envelope::host(
            &self.epoch,
            Message::StateSnapshot {
                cursor: Seq(self.head()),
                revisions: Revisions { sessions: rev, plans: rev, approvals: rev, diffs: rev },
                sessions: self.board.sessions.clone(),
                plans: self.board.plans.clone(),
                approvals: self.board.approvals.clone(),
                diffs: self.board.diffs.clone(),
                part: None,
            },
        )
    }

    /// One binary message from the browser: the messages to answer with, or the code the channel closes with.
    pub fn receive(&mut self, message: &[u8]) -> Result<Vec<Vec<u8>>, u16> {
        match self.stage.take() {
            None => Err(close::REFUSED),
            Some(Stage::First) => {
                let kind = Kind::of(message).map_err(|_| close::REFUSED)?;
                let psk = match kind {
                    Kind::Pair => Some(self.pairing.as_ref().ok_or(close::NO_PAIRING)?.psk().clone()),
                    Kind::Session => None,
                };
                let responder =
                    Responder::read(message, HOST_ID, &self.secret, psk.as_ref()).map_err(|_| close::REFUSED)?;
                let device = responder.device();
                if kind == Kind::Session {
                    let (_, access) = *self.allowed.iter().find(|(key, _)| *key == device).ok_or(close::NOT_PAIRED)?;
                    self.access = access;
                }
                let (answer, done) = responder.answer().map_err(|_| close::REFUSED)?;
                self.stage = Some(match kind {
                    Kind::Pair => {
                        self.code = Some(done.code);
                        self.pairing = None; // single use
                        self.access = self.answer.unwrap_or(Access::View);
                        self.allowed.push((device, self.access));
                        Stage::Named(done.channel)
                    }
                    Kind::Session => Stage::Open(done.channel),
                });
                Ok(vec![answer])
            }
            Some(Stage::Named(mut channel)) => {
                let frame = channel.open(message).map_err(|_| close::REFUSED)?.ok_or(close::REFUSED)?;
                let request: PairRequest = serde_json::from_slice(&frame).map_err(|_| close::REFUSED)?;
                self.named = Some(request.name);
                self.stage = Some(Stage::Comparing(channel));
                Ok(if self.hold { Vec::new() } else { self.confirm() })
            }
            Some(Stage::Comparing(channel)) => {
                self.stage = Some(Stage::Comparing(channel));
                Err(close::REFUSED)
            }
            Some(Stage::Open(mut channel)) => {
                let Some(frame) = channel.open(message).map_err(|_| close::REFUSED)? else {
                    self.stage = Some(Stage::Open(channel));
                    return Ok(Vec::new());
                };
                let envelope = wire::decode(std::str::from_utf8(&frame).unwrap()).map_err(|_| close::REFUSED)?;
                self.received.push(envelope.message.clone());
                let replies = self.answer_frame(envelope);
                let sealed = Self::seal(&mut channel, &replies);
                self.stage = Some(Stage::Open(channel));
                Ok(sealed)
            }
        }
    }

    /// The host's user answers the pairing with [`FakeHost::answer`]: the messages to send.
    pub fn confirm(&mut self) -> Vec<Vec<u8>> {
        let Some(Stage::Comparing(mut channel)) = self.stage.take() else { return Vec::new() };
        let answer = PairAnswer { paired: self.answer.is_some(), may: self.answer };
        let reply = channel.seal(&serde_json::to_vec(&answer).unwrap()).unwrap();
        if self.answer.is_some() {
            self.stage = Some(Stage::Open(channel));
        } else {
            self.allowed.pop();
        }
        reply
    }

    fn answer_frame(&mut self, envelope: Envelope) -> Vec<Envelope> {
        let id = envelope.request_id.clone();
        let epoch = self.epoch.clone();
        let host = move |m| Envelope::host(&epoch, m);
        match envelope.message {
            Message::Hello { capabilities, prior_epoch, prior_cursor, .. } => {
                let capabilities = wire::common_capabilities(&capabilities, CAPABILITIES);
                let role = self.access.role();
                let resume =
                    wire::decide_resume(&self.epoch, Seq(self.head()), Seq(1), prior_epoch.as_deref(), prior_cursor);
                let may = role.may(&capabilities);
                let welcome = host(Message::Welcome {
                    version: VERSION,
                    capabilities,
                    limits: Limits::default(),
                    role,
                    resume,
                    may,
                });
                let mut out = vec![welcome];
                match resume {
                    Resume::Reset { .. } => out.push(self.snapshot()),
                    Resume::Replay { after } => out.extend(self.events.iter().skip(after.0 as usize).cloned()),
                }
                out
            }
            Message::Resync { .. } => vec![self.snapshot()],
            Message::Ping { nonce } => vec![host(Message::Pong { nonce }).reply_to(id)],
            Message::DiffGet { worktree, path, revision, offset } => {
                let summary = self.board.diffs.iter().find(|d| d.worktree == worktree);
                let file = self.files.get(&(worktree.clone(), path.clone()));
                let (Some(summary), Some(file)) = (summary, file) else {
                    return vec![
                        WireError::new(ErrorCode::NotFound, "no such file").into_envelope(id, Some(self.epoch.clone())),
                    ];
                };
                if revision.is_some_and(|r| r.0 != summary.revision) {
                    return vec![
                        WireError::new(ErrorCode::StaleRevision, "the diff moved on")
                            .into_envelope(id, Some(self.epoch.clone())),
                    ];
                }
                let (page, next) = wire::diff_page(file, offset.unwrap_or(0) as usize, self.page_budget).unwrap();
                let body = DiffFileBody { worktree, revision: summary.revision, file: page };
                vec![
                    host(Message::DiffFile { body, truncated: next.is_some(), next_offset: next.map(|n| n as u32) })
                        .reply_to(id),
                ]
            }
            Message::ApprovalDecide(decide) => {
                let mut out = Vec::new();
                let outcome = if self.board.approvals.iter().any(|a| a.id == decide.approval) {
                    let approval = decide.approval.clone();
                    let _ = self.event(|cursor| Message::ApprovalResolved {
                        cursor,
                        id: approval,
                        disposition: Disposition::Allowed,
                    });
                    out.push(self.events.last().cloned().expect("just recorded"));
                    Outcome::done(serde_json::Value::Null)
                } else {
                    Outcome::refused(WireError::new(ErrorCode::AlreadyResolved, "answered on the desktop"))
                };
                out.push(host(Message::CommandResult(CommandResult { op: decide.op, outcome })));
                out
            }
            Message::PlanCommand(command) => {
                let plan = self.board.plans.iter().find(|p| Some(&p.id) == command.plan.as_ref()).cloned();
                let outcome = match (plan, command.action) {
                    (Some(mut plan), PlanAction::Approve)
                        if command.digest.as_deref() == Some(plan.digest.as_str()) =>
                    {
                        plan.approved = true;
                        let _ = self.event(|cursor| Message::PlanUpsert { cursor, plan });
                        Outcome::done(serde_json::Value::Null)
                    }
                    (Some(_), PlanAction::Approve) => {
                        Outcome::refused(WireError::new(ErrorCode::StaleRevision, "the plan changed"))
                    }
                    _ => Outcome::refused(WireError::new(ErrorCode::Rejected, "not in this fake")),
                };
                let mut out = Vec::new();
                if matches!(outcome, Outcome::Done { .. }) {
                    out.push(self.events.last().cloned().unwrap());
                }
                out.push(host(Message::CommandResult(CommandResult { op: command.op, outcome })));
                out
            }
            _ => Vec::new(),
        }
    }
}

pub const WORKTREE: &str = "/w/repo-t1";

/// A plan with a running task and a planned one after it, its agent waiting on an approval, the chief,
/// and the task's worktree diff of two files.
pub fn board() -> Board {
    use atomics_core::diff::FileStatus;
    use atomics_core::plan::{PlanStatus, TaskStatus};
    use atomics_core::projection::{DiffFileSummary, TaskInfo, TaskLink};
    use atomics_core::session::AgentStatus;
    let session = |id: &str, title: &str, chief: bool| SessionInfo {
        id: id.into(),
        agent: "claude".into(),
        title: title.into(),
        cwd: "/w/repo".into(),
        worktree: (!chief).then(|| WORKTREE.into()),
        branch: (!chief).then(|| "atomics/t1".into()),
        status: if chief { AgentStatus::Idle } else { AgentStatus::NeedsInput },
        chief,
        task: (!chief).then(|| TaskLink { plan: "p1".into(), task: "t1".into() }),
        pending_approvals: u32::from(!chief),
    };
    let task = |id: &str, status, depends_on: &[&str]| TaskInfo {
        id: id.into(),
        title: format!("Task {id}"),
        status,
        agent: "claude".into(),
        depends_on: depends_on.iter().map(|d| d.to_string()).collect(),
        session: (id == "t1").then(|| "s1".into()),
        branch: (id == "t1").then(|| "atomics/t1".into()),
        integrated: None,
        additions: 3,
        deletions: 1,
        report: None,
        ticket: (id == "t1").then(|| "ENG-41".into()),
    };
    let file = |path: &str| DiffFileSummary {
        old_path: Some(path.into()),
        new_path: Some(path.into()),
        status: FileStatus::Modified,
        binary: false,
        additions: 6,
        deletions: 2,
        omitted: 0,
    };
    Board {
        sessions: vec![session("chief", "Chief", true), session("s1", "Fix the parser", false)],
        plans: vec![PlanInfo {
            id: "p1".into(),
            status: PlanStatus::Open,
            goal: "Parse faster".into(),
            project: Some("/w/repo".into()),
            approved: false,
            integration: None,
            tasks: vec![task("t1", TaskStatus::Running, &[]), task("t2", TaskStatus::Planned, &["t1"])],
            digest: "digest-1".into(),
            ticket: Some("ENG-40".into()),
        }],
        approvals: vec![ApprovalInfo {
            id: "a1".into(),
            session: "s1".into(),
            tool: "Bash".into(),
            command: Some("cargo test".into()),
            cwd: WORKTREE.into(),
            reason: Some("Run the tests".into()),
        }],
        diffs: vec![DiffSummary {
            worktree: WORKTREE.into(),
            revision: 4,
            base_ref: "main".into(),
            base: "abc123".into(),
            partial: false,
            files: vec![file("src/a.rs"), file("src/b.rs")],
            comments: Vec::new(),
        }],
    }
}

/// A file's diff: one hunk of `lines` added lines after a context line.
pub fn file_diff(path: &str, lines: u32) -> FileDiff {
    use atomics_core::diff::{DiffLine, FileStatus, Hunk, LineKind};
    let line = |kind, old_no, new_no, text: String| DiffLine { kind, old_no, new_no, text, emphasis: Vec::new() };
    let mut body = vec![line(LineKind::Context, Some(1), Some(1), "fn main() {".into())];
    body.extend(
        (0..lines).map(|n| line(LineKind::Added, None, Some(n + 2), format!("    step({n}); // {}", "x".repeat(40)))),
    );
    FileDiff {
        old_path: Some(path.into()),
        new_path: Some(path.into()),
        status: FileStatus::Modified,
        binary: false,
        hunks: vec![Hunk { header: "@@ -1,1 +1,9 @@".into(), old_start: 1, new_start: 1, lines: body }],
        omitted: 0,
    }
}

/// The board's host with its two files' diffs.
pub fn host() -> FakeHost {
    let mut host = FakeHost::new(board());
    for (path, lines) in [("src/a.rs", 6), ("src/b.rs", 40)] {
        host.files.insert((WORKTREE.into(), path.into()), file_diff(path, lines));
    }
    host
}
