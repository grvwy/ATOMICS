//! What one UI holds for itself, apart from the host: the palette it draws with, the cards it has open,
//! what waits for the user's confirmation, and the intents its last draw recorded.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use atomics_core::layout::DiffSplit;
use atomics_core::plan::PlanStatus;

use crate::Intent;
use crate::diff_view::DiffState;
use crate::theme::{Fonts, Palette};

/// One UI's own state: the desktop keeps one per window, a browser one per page. Fields arrive here from
/// the desktop's client as the draw code that uses them moves into this crate.
pub struct Client {
    /// The colors and fonts to draw with, made by the platform from the host's theme.
    pub palette: Arc<Palette>,
    /// What the user asked the host to do while this UI drew, oldest first. The platform takes them
    /// after each draw.
    pub intents: Vec<Intent>,
    /// The task cards open on boards, by plan and task id.
    pub expanded_tasks: HashSet<(String, String)>,
    /// A plan waiting for the user to confirm closing it, and the status it would close with.
    pub confirm_plan_close: Option<(String, PlanStatus)>,
    /// Permission requests decided here that the host has not answered yet, by session and request id.
    pub approval_decisions: HashSet<(String, String)>,
    /// Each worktree's diff view: scroll, focus, folds, the file open whole and review comments, by root.
    pub diff_views: HashMap<PathBuf, DiffState>,
    /// Side by side or unified, picked in the diff header; wins over the config until the UI closes.
    pub diff_split: Option<DiffSplit>,
}

impl Default for Client {
    /// A client drawing with the default built-in theme until the platform gives it the host's.
    fn default() -> Self {
        let tokens = atomics_core::theme::builtin(atomics_core::theme::DEFAULT).expect("the default theme is built in");
        Self {
            palette: Arc::new(Palette::new(&tokens, Fonts::default())),
            intents: Vec::new(),
            expanded_tasks: HashSet::new(),
            confirm_plan_close: None,
            approval_decisions: HashSet::new(),
            diff_views: HashMap::new(),
            diff_split: None,
        }
    }
}

impl Client {
    /// Records what the user asked for; the host carries it out after the draw.
    pub fn act(&mut self, intent: Intent) {
        self.intents.push(intent);
    }
}
