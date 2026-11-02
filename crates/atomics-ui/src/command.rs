//! What the user asks for. The draw code records commands on the [`Client`](crate::Client) instead of
//! changing the host's model; after the draw the platform carries them out in the order they were
//! recorded: a host's command through [`Backend::send`](crate::Backend::send), a view command on the UI
//! itself. A plan is addressed like a read, by its position in the last list read; a task by id, or by
//! its position in the plan the board read.
//!
//! Commands are grouped by domain. Approvals, plans, review comments and sessions are the domains of
//! atomics.v1's `approval_decide`, `plan_command`, `review_command` and `session_command`.

use std::path::PathBuf;

use atomics_core::approvals::Decision;
use atomics_core::config::ChiefDock;
use atomics_core::plan::{PlanStatus, TaskStatus};
use atomics_core::steer::ReviewComment;
use atomics_core::store::SessionRecord;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    // ── Approvals ──────────────────────────────────────────────────────────────────────────────────
    /// Answer a permission request of the session at `session` of the last session list read. The request
    /// is named, so the host refuses a decision made for an earlier one.
    Decide {
        session: usize,
        approval: String,
        decision: Decision,
    },

    // ── Plans ────────────────────────────────────────────────────────────────────────────────────────
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
    /// Start a finished task's agent again in its worktree; `task` is its position in the plan.
    ResumeTask {
        plan: usize,
        task: usize,
    },
    /// Dismiss the alert that a task's references moved; `task` is its position in the plan.
    AcknowledgeRefs {
        plan: usize,
        task: usize,
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
    ReopenPlan(usize),
    ArchivePlan(usize),
    /// Close a plan as completed or archived, as the user confirmed.
    ClosePlan {
        plan: usize,
        status: PlanStatus,
    },
    /// Archive a plan that is no longer open; one reopened meanwhile stays open.
    ArchiveFinished(usize),
    /// Preview what a completed plan's cleanup would remove.
    CleanUpPlan(usize),
    /// Read the open cleanup preview again.
    ReloadPlanCleanup,
    /// Remove the branches the user picked in the cleanup preview of the completed plan with this id,
    /// with their worktrees; each by its repository and branch.
    RemovePlanBranches {
        plan: String,
        branches: Vec<(PathBuf, String)>,
    },

    // ── Review comments and the diff ─────────────────────────────────────────────────────────────────
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
    /// Show a file of a worktree in its diff view: its hunks when changed, the whole file otherwise.
    OpenFile {
        root: PathBuf,
        path: String,
    },
    /// The diff view moved to another file or hunk: the host stops showing a file whole there.
    LeaveFile(PathBuf),
    /// Open a worktree's file, or the worktree itself, in the editor; at `line` if given.
    Edit {
        root: PathBuf,
        path: Option<String>,
        line: Option<u32>,
    },

    // ── Sessions ───────────────────────────────────────────────────────────────────────────────────────
    /// Type a message into each of these sessions, by id, as one steer each; a toast says how it went.
    Steer {
        sessions: Vec<String>,
        text: String,
    },
    /// Start a configured agent in a folder.
    StartAgent {
        agent: String,
        folder: PathBuf,
    },
    /// End a session, by id, and close its tab.
    CloseSession(String),
    /// Start a session again from its record.
    Resume(SessionRecord),
    /// Give a session, by id, a new title.
    RenameSession {
        session: String,
        title: String,
    },
    /// Close a project's idle sessions that no plan task uses.
    CloseIdle(PathBuf),
    /// The user looked at a session, by id: a finished one becomes idle.
    SeeSession(String),
    /// Hide a session's banner, by id, about the MCP servers it needs.
    DismissMcp(String),
    /// What the user said, for the session at `session` of the last session list read.
    VoiceToSession {
        session: usize,
        text: String,
    },

    // ── Projects and worktrees ───────────────────────────────────────────────────────────────────────
    OpenProject(PathBuf),
    CloseProject(PathBuf),
    OpenWorkspace(String),
    /// Save the open projects as a workspace.
    SaveWorkspace(String),
    /// Look for merged or abandoned worktrees of a project, with one picked already if given.
    OpenCleanup {
        repo: PathBuf,
        preselect: Option<PathBuf>,
    },
    /// In the open worktree cleanup: `git fetch --prune`, to learn which remote branches are gone.
    CleanupFetch,
    /// In the open worktree cleanup: forget worktrees whose folder no longer exists.
    CleanupPrune,
    /// Read the open worktree cleanup's list again.
    CleanupReload,
    /// Remove these worktrees of the open cleanup, each with its sandboxes, and these sandboxes whose
    /// worktree is gone; with the worktrees' branches if `delete_branches`.
    RemoveWorktrees {
        worktrees: Vec<PathBuf>,
        orphans: Vec<String>,
        delete_branches: bool,
    },

    // ── The chief and settings ─────────────────────────────────────────────────────────────────────────
    /// Show or hide the chief, starting it when it is not running.
    ToggleChief,
    /// Float or dock the chief's window, and remember it.
    SetChiefDock(ChiefDock),
    /// Save the chief's window preferences: its dock and width.
    SaveChiefPreferences,
    /// What the user said, for the chief, which starts with it as its first prompt when it is not running.
    VoiceToChief(String),
    /// Show a theme and save its name to `config.toml`, or to this machine's file with `host`.
    SelectTheme {
        name: String,
        host: bool,
    },
    /// Check again whether agents can run in Docker Sandboxes.
    CheckSandbox,
    /// Read the MCP servers sbx has and ATOMICS knows again.
    RefreshConnections,
    /// Import the MCP servers the agents' own configs name.
    ImportConnections,
    /// Connect an MCP server: sbx's registration, else its definition, else `url`; its environment is
    /// stored first with `store_env`.
    Connect {
        name: String,
        store_env: bool,
        url: Option<String>,
    },
    Disconnect(String),
    /// Add a remote MCP server by name and URL.
    AddConnection {
        name: String,
        url: String,
    },
    /// Apply the chief's config proposal with this id, or reject it, with the user's note for the chief.
    DecideProposal {
        id: String,
        apply: bool,
        note: String,
    },

    // ── Not sent yet: the desktop carries these out itself ──────────────────────────────────────────
    /// A task brief on the board has the keyboard this frame: the host holds off a restart until the edit
    /// is done.
    EditingPlan,
    /// Show a finished task's worktree in the diff panel; `task` is its position in the plan.
    ShowTaskDiff {
        plan: usize,
        task: usize,
    },
    /// Show a task's session, with its diff.
    OpenTaskSession(String),
    /// Hide the diff panel.
    CloseDiff,
    /// Make a session the active one, opening the diff panel if `open_diff`.
    Activate {
        session: String,
        open_diff: bool,
    },
    /// Ask for a session's new title.
    Rename(String),
    /// Ask the platform for a folder to open as a project.
    PickProject,
    /// Ask where to create a new worktree of a project.
    NewWorktree(PathBuf),
    /// Show the diff of a worktree or main checkout no session is active in.
    ShowRoot(PathBuf),
    /// Close a plan's tab: an open plan asks to be closed first, a finished one is archived.
    ClosePlanTab(usize),
    /// Show a finished plan's board, read-only.
    ViewPlanHistory(usize),
    /// Show every project's sessions, or one project's.
    SetView(Option<PathBuf>),
    /// Show the sessions outside any plan.
    ShowUngrouped,
    /// Show an open plan's sessions and board.
    ShowPlan(usize),
    /// Show or hide the diff panel.
    ToggleDiff,
    /// Open the new-session dialog.
    NewSession,
    /// Show or hide the plan board.
    TogglePlan,
}

impl Command {
    /// A task card's `diff`, `resume` or `acknowledge_refs` button, for the task at position `task` of the
    /// plan at `plan`; `None` for any other action.
    pub fn task(plan: usize, task: usize, action: &str) -> Option<Command> {
        match action {
            "diff" => Some(Command::ShowTaskDiff { plan, task }),
            "resume" => Some(Command::ResumeTask { plan, task }),
            "acknowledge_refs" => Some(Command::AcknowledgeRefs { plan, task }),
            _ => None,
        }
    }
}
