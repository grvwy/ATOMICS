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
                Intent::Decide { session, approval, decision } => self.approve_request(session, approval, decision),
                Intent::EditComments { root, before, after } => self.post(move |runtime| {
                    runtime.diffs.entry(root).or_default().edit_comments(before, after);
                }),
                Intent::SendReview(root) => self.send_review(&root),
                Intent::RefreshDiff(root) => {
                    self.post(move |runtime| state_refresh(runtime.diffs.entry(root).or_default()))
                }
                Intent::CloseDiff => self.runtime.diff_open = false,
                Intent::Edit { root, path, line } => {
                    let ctx = self.client.ctx.clone();
                    self.edit(&ctx, &root, path.as_deref(), line);
                }
                Intent::Activate { session, open_diff } => self.activate(&session, open_diff),
                Intent::Rename(session) => self.begin_rename(&session),
                Intent::Resume(record) => self.post(move |state| state.resume.push(record)),
                Intent::PickProject => self.pick_project(),
                Intent::StartAgent { agent, folder } => self.runtime.pending_open.push((agent, folder)),
                Intent::NewWorktree(root) => {
                    self.open_new_dialog(Some(root.clone()));
                    if let Some(dialog) = &mut self.client.new_dialog {
                        dialog.place = Place::Worktree;
                        dialog.repo = paths::display(&root);
                    }
                }
                Intent::CloseProject(root) => self.close_project(&root),
                Intent::ShowRoot(root) => {
                    self.runtime.active = None;
                    self.runtime.diff_root = Some(root);
                    self.runtime.diff_open = true;
                }
                Intent::OpenCleanup { repo, preselect } => self.open_cleanup(repo, preselect),
                Intent::OpenFile { root, path } => self.open_file(&root, &path),
                Intent::ClosePlanTab(plan) => self.close_plan_tab(plan),
                Intent::ViewPlanHistory(plan) => self.view_plan_history(plan),
                Intent::SetView(view) => self.runtime.set_view(view),
                Intent::ShowUngrouped => self.runtime.show_ungrouped(),
                Intent::ShowPlan(plan) => {
                    self.runtime.selected_plan = plan;
                    self.runtime.show_plan_fleet();
                }
                Intent::CloseIdle(root) => self.post(move |state| {
                    let closed = state.close_idle(&root);
                    state.toast(format!("Closed {closed} idle session{}", if closed == 1 { "" } else { "s" }));
                }),
                Intent::ToggleDiff => self.runtime.diff_open = !self.runtime.diff_open,
                Intent::NewSession => self.open_new_dialog(None),
                Intent::TogglePlan => self.runtime.toggle_plan(),
                Intent::OpenProject(root) => self.open_project(&root),
                Intent::OpenWorkspace(name) => self.open_workspace(&name),
                Intent::SaveWorkspace(name) => self.save_workspace(&name),
            }
        }
    }
}
