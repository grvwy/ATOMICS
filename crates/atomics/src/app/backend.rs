//! The in-process [`Backend`]: reads the host model directly. The desktop's draw code reads through it,
//! so a loopback or browser client can read the same things over the protocol.
use atomics_ui::{Backend, DiffSnapshot, PlanRow};

use super::*;

/// A short-lived read of the model; build it with [`State::backend`].
pub(super) struct Local<'a>(&'a State);

impl State {
    pub(super) fn backend(&self) -> Local<'_> {
        Local(self)
    }
}

impl Local<'_> {
    fn row(&self, index: usize, plan: &Plan) -> PlanRow {
        PlanRow {
            id: plan.id.clone(),
            goal: plan.goal.clone(),
            tab_name: super::sidebar::plan_tab_name(plan),
            status: plan.status,
            state: self.0.plan_state(index),
            changed: self.0.changed_plans.contains(&plan.id),
            done: plan.count(TaskStatus::Done),
            total: plan.tasks.len(),
            attention: plan.count(TaskStatus::Review) + plan.count(TaskStatus::Blocked),
        }
    }
}

impl Backend for Local<'_> {
    fn plan_list(&self) -> Vec<PlanRow> {
        self.0.plans.iter().enumerate().map(|(index, plan)| self.row(index, plan)).collect()
    }

    fn plan_row(&self, index: usize) -> Option<PlanRow> {
        self.0.plans.get(index).map(|plan| self.row(index, plan))
    }

    fn diff_roots(&self) -> Vec<PathBuf> {
        self.0.diffs.keys().cloned().collect()
    }

    fn diff(&self, root: &Path) -> Option<DiffSnapshot> {
        let data = self.0.diffs.get(root)?;
        Some(DiffSnapshot {
            prepared: data.prepared.clone(),
            error: data.error.clone(),
            loading: data.loading,
            comments: data.comments.clone(),
            viewing: data.viewing.clone(),
            file: data.file.clone(),
            file_error: data.file_error.clone(),
        })
    }
}
