//! What the user asks the host to do. The draw code records intents on the [`Client`](crate::Client)
//! instead of changing the host's model; the host carries them out right after the draw returns, in the
//! order they were recorded. A plan is addressed like a read, by its position in the last list read; a
//! task by id, or by its position in the plan the board read. Slice S8 turns intents into the protocol's
//! commands.

use atomics_core::plan::TaskStatus;

#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// A task brief on the board has the keyboard this frame: the host holds off a restart until the edit
    /// is done.
    EditingPlan,
    /// A planned task's brief, as edited on its card.
    EditBrief {
        plan: usize,
        task: String,
        brief: String,
    },
    /// Start a ready task.
    StartTask {
        plan: usize,
        task: String,
    },
    /// Merge a task into the plan's integration branch.
    IntegrateTask {
        plan: usize,
        task: String,
    },
    /// A task card's `diff`, `resume` or `acknowledge_refs` button; `task` is its position in the plan.
    TaskAction {
        plan: usize,
        task: usize,
        action: &'static str,
    },
    /// Move a task to another column.
    SetTaskStatus {
        plan: usize,
        task: String,
        status: TaskStatus,
    },
    ApprovePlan(usize),
    /// Integrate every accepted task that is not integrated yet.
    IntegrateAccepted(usize),
    /// Read the plan's tickets again now.
    SyncTickets(usize),
    /// Start every task whose dependencies are done.
    DispatchReady(usize),
    /// Show a task's session, with its diff.
    OpenTaskSession(String),
    ReopenPlan(usize),
    ArchivePlan(usize),
    /// Preview what a completed plan's cleanup would remove.
    CleanUpPlan(usize),
}
