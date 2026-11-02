//! What the user asks the host to do. The draw code records intents on the [`Client`](crate::Client)
//! instead of changing the host's model; the host carries them out right after the draw returns, in the
//! order they were recorded. A plan is addressed like a read, by its position in the last list read; a
//! task by id, or by its position in the plan the board read. Slice S8 turns intents into the protocol's
//! commands.

use std::path::PathBuf;

use atomics_core::approvals::Decision;
use atomics_core::plan::TaskStatus;
use atomics_core::steer::ReviewComment;

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
    /// Answer a permission request of the session at `session` of the last session list read. The request
    /// is named, so the host refuses a decision made for an earlier one.
    Decide {
        session: usize,
        approval: String,
        decision: Decision,
    },
    /// The review comments on a worktree's diff changed from `before` to `after`.
    EditComments {
        root: PathBuf,
        before: Vec<ReviewComment>,
        after: Vec<ReviewComment>,
    },
    /// Send a worktree's review comments to the agent working there.
    SendReview(PathBuf),
    /// Read a worktree's diff again now.
    RefreshDiff(PathBuf),
    /// Hide the diff panel.
    CloseDiff,
    /// Open a worktree's file, or the worktree itself, in the editor; at `line` if given.
    Edit {
        root: PathBuf,
        path: Option<String>,
        line: Option<u32>,
    },
}
