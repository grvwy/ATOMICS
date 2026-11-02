use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::diff_data::{FileView, Prepared};
use atomics_core::plan::PlanStatus;
use atomics_core::steer::ReviewComment;

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

    /// The worktrees the host has a diff for.
    fn diff_roots(&self) -> Vec<PathBuf>;

    /// The diff of one worktree.
    fn diff(&self, root: &Path) -> Option<DiffSnapshot>;
}
