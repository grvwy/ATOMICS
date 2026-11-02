use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::approvals::ApprovalRequest;
use atomics_core::config::AgentKind;
use atomics_core::diff_data::{FileView, Prepared};
use atomics_core::plan::{Plan, PlanStatus};
use atomics_core::session::AgentStatus;
use atomics_core::steer::ReviewComment;
use atomics_core::tickets::TicketConfig;
use egui::Color32;

use crate::theme::Palette;

/// What a plan is doing, for its tab. Selection is independent of activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanState {
    Running(usize),
    NeedsYou,
    Idle,
    Completed,
    Archived,
}

impl PlanState {
    pub fn urgency(self) -> u8 {
        match self {
            Self::NeedsYou => 2,
            Self::Running(_) => 1,
            _ => 0,
        }
    }

    pub fn label(self, goal: &str) -> String {
        let name: String = goal.chars().take(28).collect();
        match self {
            Self::Running(count) => format!("● {name} · {count} running"),
            Self::NeedsYou => format!("◆ {name} · needs you"),
            Self::Idle => format!("{name} · idle"),
            Self::Completed => format!("✓ {name}"),
            Self::Archived => format!("✓ {name} · archived"),
        }
    }

    /// The color a plan's goal and tab are drawn in.
    pub fn color(self, palette: &Palette) -> Color32 {
        match self {
            Self::Running(_) => palette.accent,
            Self::NeedsYou => palette.warning,
            Self::Idle => palette.foreground,
            Self::Completed | Self::Archived => palette.muted,
        }
    }
}

/// One plan as the tabs and the top bar show it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRow {
    pub id: String,
    pub goal: String,
    /// The tab's name: the plan's ticket with its progress, else the goal.
    pub tab_name: String,
    pub status: PlanStatus,
    pub state: PlanState,
    /// Changed since the user last looked.
    pub changed: bool,
    pub done: usize,
    pub total: usize,
    /// Tasks in review or blocked.
    pub attention: usize,
}

/// One session as the sidebar, the tab strip and the tile headers show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub id: String,
    pub title: String,
    /// The configured agent it runs, by name.
    pub agent: String,
    pub kind: AgentKind,
    pub status: AgentStatus,
    /// The chief, which stays outside the agent fleet.
    pub chief: bool,
    /// The worktree it runs in, if any.
    pub worktree: Option<PathBuf>,
    /// Its worktree's branch; `None` outside a worktree or on a detached HEAD.
    pub branch: Option<String>,
    /// Its worktree, else its folder: where its diff is read.
    pub root: PathBuf,
    /// The open project `root` belongs to, if any.
    pub project: Option<PathBuf>,
    /// Neither the chief nor in any plan's task: the Ungrouped tab shows it.
    pub ungrouped: bool,
    /// It works in a scratch folder of its own, outside any project.
    pub scratch: bool,
    /// The sandbox it runs in, if any.
    pub sandbox: Option<String>,
    /// It runs in a terminal outside ATOMICS, which sees it through its hooks.
    pub outside: bool,
    /// Permission requests waiting for a decision.
    pub pending_approvals: usize,
    /// Messages held back until those requests are decided.
    pub held_messages: usize,
    /// Lines added and removed in the diff of `root`, once the host has read it.
    pub changes: Option<(usize, usize)>,
}

/// One plan as its board shows it: the plan header and the kanban columns. The plan comes whole; the
/// other fields are what the board reads beside it from the rest of the host's model.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardView {
    /// The plan as the host holds it: goal, status, tasks, ticket and stages.
    pub plan: Plan,
    /// What the plan is doing; it colors the goal.
    pub state: PlanState,
    /// Planned tasks whose dependencies are done and integrated, by id, in plan order.
    pub ready: Vec<String>,
    /// Every session's title and status, by session id: a card's session line and its open button.
    pub sessions: HashMap<String, (String, AgentStatus)>,
    /// Why a task's integration is not verified yet, by task id, and whether a check of it runs now.
    pub integration_notes: HashMap<String, (String, bool)>,
    /// An integration freshness check of this plan runs.
    pub checking: bool,
    /// The plan's tasks being integrated now, by id.
    pub integrating: HashSet<String>,
    /// The `[tickets]` in effect, once the host has read it.
    pub ticket_config: Option<Arc<TicketConfig>>,
    /// The tickets on the plan's board, and how many of them no task follows, once the host has read them.
    pub board_counts: Option<(usize, usize)>,
    /// Finished tasks whose worktree is still on the host, by id: their `diff` and `resume` need it.
    pub worktrees: HashSet<String>,
    /// Tasks run by a Codex agent, by id: Codex resumes a session without a saved id.
    pub codex: HashSet<String>,
    /// The host's clock when it read the board, in seconds since the epoch: how old a report or a ticket
    /// read is.
    pub now: u64,
}

/// A permission request a session waits on, as its approval banner shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRow {
    /// The session that asked, by id.
    pub session: String,
    /// The request's id. A decision names it, so the host refuses one made for an earlier request.
    pub id: String,
    pub request: ApprovalRequest,
}

/// A worktree's diff as the host last published it, and the whole file open beside it.
#[derive(Clone, Default)]
pub struct DiffSnapshot {
    pub prepared: Option<Arc<Prepared>>,
    pub error: Option<String>,
    pub loading: bool,
    pub comments: Vec<ReviewComment>,
    /// The file the host was asked to load whole, if any.
    pub viewing: Option<String>,
    pub file: Option<Arc<FileView>>,
    pub file_error: Option<String>,
}

/// Everything the UI reads from a host. Methods take `&self` and return owned or shared data, so an
/// implementation can sit over a lock, a snapshot or a socket. Writes are added as their own slices.
pub trait Backend {
    /// Every plan, in the host's order. The UI addresses a plan by its position in this list.
    fn plan_list(&self) -> Vec<PlanRow>;

    /// One plan of [`plan_list`](Backend::plan_list), without building the others.
    fn plan_row(&self, index: usize) -> Option<PlanRow>;

    /// The board of the plan at `index` of [`plan_list`](Backend::plan_list).
    fn board(&self, index: usize) -> Option<BoardView>;

    /// Every session, in the host's order. The UI addresses a session by its position in this list.
    fn session_list(&self) -> Vec<SessionRow>;

    /// One session of [`session_list`](Backend::session_list), without building the others.
    fn session_row(&self, index: usize) -> Option<SessionRow>;

    /// The first permission request the session at `index` of [`session_list`](Backend::session_list)
    /// waits on: the one its banner shows. Later ones wait behind it.
    fn session_approval(&self, index: usize) -> Option<ApprovalRow>;

    /// The worktrees the host has a diff for.
    fn diff_roots(&self) -> Vec<PathBuf>;

    /// The diff of one worktree.
    fn diff(&self, root: &Path) -> Option<DiffSnapshot>;
}
