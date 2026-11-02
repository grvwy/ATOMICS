//! The event stream every client follows: it turns published projections into numbered events,
//! keeps a bounded replay ring, and fans frames out to per-client queues.
//!
//! Nothing here does I/O, and nothing here touches the host's `State`. A client's queue is
//! bounded; when it overflows the client is marked lagged, its backlog is dropped, and its own
//! thread sends a fresh snapshot instead. A reader that stops reading can therefore never make the
//! hub (or the runtime thread that publishes) wait.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use atomics_core::projection::{Loaded, Projections};
use atomics_core::wire::{self, Cursor, Disposition, Envelope, Message, Resume, Seq, WireError};

/// Replay ring bounds: entries, bytes and age. It is an optimisation, not persistence.
#[derive(Debug, Clone)]
pub struct RingLimits {
    pub entries: usize,
    pub bytes: usize,
    pub age: Duration,
}

impl Default for RingLimits {
    fn default() -> Self {
        Self { entries: 200, bytes: 8 << 20, age: Duration::from_secs(300) }
    }
}

#[derive(Default)]
struct Queue {
    frames: VecDeque<Arc<str>>,
    bytes: usize,
    lagged: bool,
}

/// One client's bounded outbound queue. The hub pushes; the client's own thread drains.
pub struct Outbox {
    queue: Mutex<Queue>,
    max_frames: usize,
    max_bytes: usize,
    closed: AtomicBool,
}

impl Outbox {
    fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self { queue: Mutex::new(Queue::default()), max_frames, max_bytes, closed: AtomicBool::new(false) }
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Never blocks. A full queue drops the backlog and marks the client lagged.
    fn push(&self, frame: &Arc<str>) {
        let mut q = self.queue();
        if q.lagged {
            return;
        }
        if q.frames.len() >= self.max_frames || q.bytes + frame.len() > self.max_bytes {
            q.frames.clear();
            q.bytes = 0;
            q.lagged = true;
            return;
        }
        q.bytes += frame.len();
        q.frames.push_back(frame.clone());
    }

    fn mark_lagged(&self) {
        let mut q = self.queue();
        q.frames.clear();
        q.bytes = 0;
        q.lagged = true;
    }

    /// Everything queued, and whether the client must be resynchronised instead.
    pub fn take(&self) -> Taken {
        let mut q = self.queue();
        if q.lagged {
            return Taken::Lagged;
        }
        q.bytes = 0;
        Taken::Frames(q.frames.drain(..).collect())
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

pub enum Taken {
    Frames(Vec<Arc<str>>),
    Lagged,
}

pub struct Hub {
    epoch: String,
    head: u64,
    /// What the events so far add up to; its `cursor` is `head`.
    state: Loaded,
    seen: u64,
    ring: VecDeque<(u64, Instant, Arc<str>)>,
    ring_bytes: usize,
    limits: RingLimits,
    snapshot: Option<Arc<str>>,
    clients: Vec<Arc<Outbox>>,
    queue_frames: usize,
    queue_bytes: usize,
}

pub struct Attached {
    pub resume: Resume,
    pub outbox: Arc<Outbox>,
    pub epoch: String,
}

impl Hub {
    pub fn new(epoch: String, limits: RingLimits, queue_frames: usize, queue_bytes: usize) -> Self {
        let state = Projections::default().load();
        Self {
            epoch,
            head: 0,
            state,
            seen: u64::MAX,
            ring: VecDeque::new(),
            ring_bytes: 0,
            limits,
            snapshot: None,
            clients: Vec::new(),
            queue_frames,
            queue_bytes,
        }
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn head(&self) -> u64 {
        self.head
    }

    fn oldest_retained(&self) -> u64 {
        self.ring.front().map_or(self.head + 1, |(c, ..)| *c)
    }

    /// Read the published projections and turn what changed into events for every client.
    pub fn poll(&mut self, projections: &Projections) {
        self.expire_ring();
        self.clients.retain(|c| !c.is_closed());
        if projections.cursor() == self.seen {
            return;
        }
        let loaded = projections.load();
        self.seen = loaded.cursor;
        let events = self.diff(&loaded);
        if events.is_empty() {
            return;
        }
        self.snapshot = None;
        for message in events {
            self.head += 1;
            let envelope = Envelope::host(&self.epoch, with_cursor(message, Seq(self.head)));
            match wire::encode(&envelope) {
                Ok(frame) => self.publish(self.head, frame.into()),
                Err(_) => {
                    // An event too large to send cannot be replayed either: everyone resyncs.
                    self.ring.clear();
                    self.ring_bytes = 0;
                    for client in &self.clients {
                        client.mark_lagged();
                    }
                }
            }
        }
        self.state = Loaded { cursor: self.head, ..loaded };
    }

    fn publish(&mut self, cursor: u64, frame: Arc<str>) {
        for client in &self.clients {
            client.push(&frame);
        }
        self.ring_bytes += frame.len();
        self.ring.push_back((cursor, Instant::now(), frame));
        while self.ring.len() > self.limits.entries || self.ring_bytes > self.limits.bytes {
            self.drop_oldest();
        }
    }

    fn drop_oldest(&mut self) {
        if let Some((_, _, frame)) = self.ring.pop_front() {
            self.ring_bytes -= frame.len();
        }
    }

    fn expire_ring(&mut self) {
        while self.ring.front().is_some_and(|(_, at, _)| at.elapsed() > self.limits.age) {
            self.drop_oldest();
        }
    }

    /// Events (cursor placeholders, filled in by the caller) for the change from `self.state` to `new`.
    fn diff(&self, new: &Loaded) -> Vec<Message> {
        let old = &self.state;
        let mut out = Vec::new();
        let zero = Seq(0);
        if new.sessions.revision != old.sessions.revision {
            let before: BTreeMap<_, _> = old.sessions.data.iter().map(|s| (&s.id, s)).collect();
            let after: BTreeMap<_, _> = new.sessions.data.iter().map(|s| (&s.id, s)).collect();
            for (id, s) in &after {
                if before.get(id) != Some(s) {
                    out.push(Message::SessionUpsert { cursor: zero, session: (*s).clone() });
                }
            }
            for id in before.keys().filter(|id| !after.contains_key(*id)) {
                out.push(Message::SessionRemoved { cursor: zero, id: (*id).clone() });
            }
        }
        if new.plans.revision != old.plans.revision {
            let before: BTreeMap<_, _> = old.plans.data.iter().map(|p| (&p.id, p)).collect();
            let after: BTreeMap<_, _> = new.plans.data.iter().map(|p| (&p.id, p)).collect();
            for (id, p) in &after {
                if before.get(id) != Some(p) {
                    out.push(Message::PlanUpsert { cursor: zero, plan: (*p).clone() });
                }
            }
            for id in before.keys().filter(|id| !after.contains_key(*id)) {
                out.push(Message::PlanRemoved { cursor: zero, id: (*id).clone() });
            }
        }
        if new.approvals.revision != old.approvals.revision {
            let before: BTreeMap<_, _> = old.approvals.data.iter().map(|a| (&a.id, a)).collect();
            let after: BTreeMap<_, _> = new.approvals.data.iter().map(|a| (&a.id, a)).collect();
            for (id, a) in &after {
                if before.get(id) != Some(a) {
                    out.push(Message::ApprovalPending { cursor: zero, approval: (*a).clone() });
                }
            }
            // The projection says an approval is gone, not how it ended; H4 carries the real outcome.
            for id in before.keys().filter(|id| !after.contains_key(*id)) {
                out.push(Message::ApprovalResolved { cursor: zero, id: (*id).clone(), disposition: Disposition::Withdrawn });
            }
        }
        if new.diffs.revision != old.diffs.revision {
            let before: BTreeMap<_, _> = old.diffs.data.iter().map(|d| (&d.worktree, d)).collect();
            let after: BTreeMap<_, _> = new.diffs.data.iter().map(|d| (&d.worktree, d)).collect();
            for (id, d) in &after {
                if before.get(id) != Some(d) {
                    out.push(Message::DiffSummary { cursor: zero, summary: (*d).clone() });
                }
            }
            for (id, d) in before.iter().filter(|(id, _)| !after.contains_key(**id)) {
                let tombstone = atomics_core::projection::DiffSummary::tombstone(id, d.revision + 1);
                out.push(Message::DiffSummary { cursor: zero, summary: tombstone });
            }
        }
        out
    }

    /// The current state as one `state_snapshot` frame, shared between clients at the same cursor.
    /// `Err` when the board no longer fits in one frame (chunked transfer is not built yet).
    pub fn snapshot_frame(&mut self) -> Result<Arc<str>, WireError> {
        if let Some(frame) = &self.snapshot {
            return Ok(frame.clone());
        }
        let frame: Arc<str> = wire::encode(&Envelope::snapshot(&self.epoch, &self.state))?.into();
        self.snapshot = Some(frame.clone());
        Ok(frame)
    }

    /// Register a client. In one step: decide replay or reset, queue the frames it needs first, and
    /// start feeding it live events, so nothing published in between is lost or repeated.
    pub fn attach(&mut self, prior_epoch: Option<&str>, prior_cursor: Option<Cursor>) -> Result<Attached, WireError> {
        self.expire_ring();
        let resume = wire::decide_resume(
            &self.epoch,
            Seq(self.head),
            Seq(self.oldest_retained()),
            prior_epoch,
            prior_cursor,
        );
        let outbox = Arc::new(Outbox::new(self.queue_frames, self.queue_bytes));
        match resume {
            Resume::Replay { after } => {
                let mut q = outbox.queue();
                for (_, _, frame) in self.ring.iter().filter(|(c, ..)| *c > after.0) {
                    q.bytes += frame.len();
                    q.frames.push_back(frame.clone());
                }
            }
            Resume::Reset { .. } => {
                let frame = self.snapshot_frame()?;
                let mut q = outbox.queue();
                q.bytes += frame.len();
                q.frames.push_back(frame);
            }
        }
        self.clients.push(outbox.clone());
        Ok(Attached { resume, outbox, epoch: self.epoch.clone() })
    }

    /// For a lagged or resyncing client: drop its backlog and queue the current snapshot, which
    /// already includes everything published so far.
    pub fn resync(&mut self, outbox: &Outbox) -> Result<(), WireError> {
        let frame = self.snapshot_frame()?;
        let mut q = outbox.queue();
        q.frames.clear();
        q.lagged = false;
        q.bytes = frame.len();
        q.frames.push_back(frame);
        Ok(())
    }
}

fn with_cursor(mut message: Message, to: Cursor) -> Message {
    match &mut message {
        Message::SessionUpsert { cursor, .. }
        | Message::SessionRemoved { cursor, .. }
        | Message::PlanUpsert { cursor, .. }
        | Message::PlanRemoved { cursor, .. }
        | Message::ApprovalPending { cursor, .. }
        | Message::ApprovalResolved { cursor, .. }
        | Message::DiffSummary { cursor, .. } => *cursor = to,
        _ => {}
    }
    message
}
