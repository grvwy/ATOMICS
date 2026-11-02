//! The shim between the draw code in atomics-ui and the host's model. That code reads through the
//! in-process [`Backend`] and records what the user asks for as [`Intent`]s; the desktop carries each out
//! right after the draw, with the same posts and view-hint writes the desktop's own draw code makes.
use atomics_ui::Intent;

use super::*;

impl Desktop<'_> {
    /// Draws with atomics-ui code: the client's palette is the host's theme, reads go through the
    /// in-process backend, and the intents the draw recorded run when it returns.
    pub(super) fn draw<R>(&mut self, draw: impl FnOnce(&dyn Backend, &mut atomics_ui::Client) -> R) -> R {
        self.client.ui.palette = self.palette();
        let result = draw(&self.runtime.backend(), &mut self.client.ui);
        self.run_intents();
        result
    }

    fn run_intents(&mut self) {
        for intent in std::mem::take(&mut self.client.ui.intents) {
            match intent {
                Intent::EditingPlan => self.runtime.plan_edit_open = true,
                Intent::EditBrief { plan, task, brief } => self.post_plan(plan, move |state, index| {
                    state.edit_task_brief(index, &task, brief);
                }),
                Intent::StartTask { plan, task } => {
                    self.post_plan(plan, move |state, index| state.dispatch_when_checked(index, Some(task), None))
                }
                Intent::IntegrateTask { plan, task } => {
                    self.post_plan(plan, move |state, index| state.integrate(index, &task, None))
                }
                Intent::TaskAction { plan, task, action } => self.task_action(plan, task, action),
                Intent::SetTaskStatus { plan, task, status } => self.post_plan(plan, move |state, index| {
                    if state.plans[index].status != PlanStatus::Open {
                        return;
                    }
                    let old = state.plans[index].task_mut(&task).map(|task| {
                        let old = task.status;
                        task.set_status(status);
                        old
                    });
                    state.finish_plan_agents();
                    state.save_plan(index);
                    if let Some(old) = old {
                        let author = atomics_core::tickets::Author::User;
                        state.ticket_status_changed(index, &task, old, status, None, author);
                    }
                }),
                Intent::ApprovePlan(plan) => self.post_plan(plan, |state, index| {
                    state.approve_plan(index);
                    state.toast("Plan approved: the orchestrator can dispatch it now");
                }),
                Intent::IntegrateAccepted(plan) => self.post_plan(plan, |state, index| {
                    let ids: Vec<_> = state.plans[index].pending_integrations().iter().map(|t| t.id.clone()).collect();
                    for id in ids {
                        state.integrate(index, &id, None);
                    }
                }),
                Intent::SyncTickets(plan) => self.post_plan(plan, |state, index| {
                    let id = state.plans[index].id.clone();
                    state.resync_tickets(&id);
                }),
                Intent::DispatchReady(plan) => {
                    self.post_plan(plan, |state, index| state.dispatch_when_checked(index, None, None))
                }
                Intent::OpenTaskSession(session) => {
                    self.activate(&session, true);
                    self.runtime.panel = PanelMode::Diff;
                }
                Intent::ReopenPlan(plan) => self.post_plan(plan, |state, index| {
                    if let Err(error) = state.reopen_plan(index) {
                        state.toast(error);
                    }
                }),
                Intent::ArchivePlan(plan) => self.post_plan(plan, |state, index| {
                    if let Err(error) = state.close_plan(index, PlanStatus::Archived) {
                        state.toast(error);
                    }
                }),
                Intent::CleanUpPlan(plan) => self.open_plan_cleanup(plan),
            }
        }
    }
}
