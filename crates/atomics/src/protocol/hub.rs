//! The event stream every client follows: it turns published projections into numbered events,
//! keeps a bounded replay ring, and fans frames out to per-client queues.
//!
//! Nothing here does I/O, and nothing here touches the host's `State`. A client's queue is
//! bounded; when it overflows the client is marked lagged, its backlog is dropped, and its own
//! thread sends a fresh snapshot instead. A reader that stops reading can therefore never make the
//! hub (or the runtime thread that publishes) wait. Terminal screens use the same outbox with a
//! slot per screen (`screens.rs`), outside the board's queue, so a busy terminal never pushes an
//! approval out and board events are always sent first. Whatever queues something for a client
//! wakes its thread, so it is sent at once.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use atomics_core::projection::{Loaded, Projections};
use atomics_core::wire::{self, Cursor, Disposition, Envelope, Message, Resume, Seq, WireError};

use super::link::Waker;
use super::screens::Feed;

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
    screens: HashMap<String, Slot>,
    /// Delta bytes waiting across all screen slots.
    screen_bytes: usize,
}

/// One subscribed screen in one client's outbox: deltas that follow what the client was sent, or
/// a note that it needs the current snapshot instead.
struct Slot {
    feed: Feed,
    deltas: Vec<Arc<str>>,
    bytes: usize,
    snapshot: bool,
    /// The generation and revision the client will hold once everything queued is sent.
    at: Option<(u64, u64)>,
}

impl Slot {
    fn supersede(&mut self, total: &mut usize) {
        *total -= self.bytes;
        self.deltas.clear();
        self.bytes = 0;
        self.snapshot = true;
    }
}

/// One client's bounded outbound queue. The hub pushes; the client's own thread drains.
pub struct Outbox {
    queue: Mutex<Queue>,
    max_frames: usize,
    max_bytes: usize,
    closed: AtomicBool,
    /// Ends the client thread's wait for its next frame.
    waker: OnceLock<Arc<Waker>>,
}

impl Outbox {
    pub(super) fn new(max_frames: usize, max_bytes: usize) -> Self {
        Self {
            queue: Mutex::new(Queue::default()),
            max_frames,
            max_bytes,
            closed: AtomicBool::new(false),
            waker: OnceLock::new(),
        }
    }

    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wake the client's thread whenever something is queued for it.
    pub fn set_waker(&self, waker: Arc<Waker>) {
        let _ = self.waker.set(waker);
    }

    /// Never blocks.
    fn wake(&self) {
        if let Some(waker) = self.waker.get() {
            waker.wake();
        }
    }

    /// Never blocks. A full queue drops the backlog and marks the client lagged.
    fn push(&self, frame: &Arc<str>) {
        {
            let mut q = self.queue();
            if q.lagged {
                return;
            }
            if q.frames.len() >= self.max_frames || q.bytes + frame.len() > self.max_bytes {
                q.frames.clear();
                q.bytes = 0;
                q.lagged = true;
            } else {
                q.bytes += frame.len();
                q.frames.push_back(frame.clone());
            }
        }
        self.wake();
    }

    fn mark_lagged(&self) {
        {
            let mut q = self.queue();
            q.frames.clear();
            q.bytes = 0;
            q.lagged = true;
        }
        self.wake();
    }

    /// Everything queued (board events first, then screens), or that the client must be
    /// resynchronised first. A screen that needs a snapshot gets the one its feed holds now; until
    /// the first copy is published it waits.
    pub fn take(&self) -> Taken {
        let mut q = self.queue();
        if q.lagged {
            return Taken::Lagged;
        }
        q.bytes = 0;
        let mut frames: Vec<Arc<str>> = q.frames.drain(..).collect();
        let mut sent = 0;
        for slot in q.screens.values_mut() {
            if !slot.snapshot {
                sent += slot.bytes;
                slot.bytes = 0;
                frames.append(&mut slot.deltas);
                continue;
            }
            let current = slot.feed.read().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(current) = current
                && let Some(snapshot) = current.frames()
            {
                frames.extend(snapshot.iter().cloned());
                slot.snapshot = false;
                slot.at = Some((current.generation, current.revision));
            }
        }
        q.screen_bytes -= sent;
        Taken::Frames(frames)
    }

    /// Follow a screen, starting with a snapshot from `feed` (also when its terminal was replaced).
    pub fn watch_screen(&self, session: &str, feed: &Feed) {
        {
            let mut q = self.queue();
            let q = &mut *q;
            let old = q.screens.insert(
                session.to_string(),
                Slot { feed: feed.clone(), deltas: Vec::new(), bytes: 0, snapshot: true, at: None },
            );
            q.screen_bytes -= old.map_or(0, |s| s.bytes);
        }
        self.wake();
    }

    pub fn unwatch_screen(&self, session: &str) {
        let mut q = self.queue();
        if let Some(slot) = q.screens.remove(session) {
            q.screen_bytes -= slot.bytes;
        }
    }

    /// Replace whatever is queued for this screen with its current snapshot. False when the
    /// client does not follow it.
    pub fn want_snapshot(&self, session: &str) -> bool {
        {
            let mut q = self.queue();
            let q = &mut *q;
            let Some(slot) = q.screens.get_mut(session) else { return false };
            slot.supersede(&mut q.screen_bytes);
        }
        self.wake();
        true
    }

    /// Queue the delta from `base` to `revision` if it follows what this client holds. A delta
    /// that does not follow, or that would overfill the slot, turns the slot into a snapshot.
    pub fn push_screen(
        &self,
        session: &str,
        (generation, base, revision): (u64, u64, u64),
        frame: &Arc<str>,
        max_frames: usize,
        max_bytes: usize,
    ) {
        {
            let mut q = self.queue();
            let q = &mut *q;
            let Some(slot) = q.screens.get_mut(session) else { return };
            if slot.snapshot {
                return; // the snapshot it will get already contains this change
            }
            match slot.at {
                Some(at) if at == (generation, base) => {
                    if slot.deltas.len() >= max_frames || q.screen_bytes + frame.len() > max_bytes {
                        slot.supersede(&mut q.screen_bytes);
                    } else {
                        slot.deltas.push(frame.clone());
                        slot.bytes += frame.len();
                        q.screen_bytes += frame.len();
                        slot.at = Some((generation, revision));
                    }
                }
                // Loaded a snapshot that already includes this delta.
                Some((g, r)) if g == generation && r >= revision => return,
                _ => slot.supersede(&mut q.screen_bytes),
            }
        }
        self.wake();
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    pub fn is_closed(&self) -> bool {
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
