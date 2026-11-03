//! The frame's screens: a top bar with the connection's state and four views (Board, Plans, Approvals and
//! Diffs), drawn by atomics-ui's draw functions over the remote backend. After each draw the commands the
//! user gave are carried out: a view command here, any other through the backend. A minimal layout; the
//! phone layout comes with the terminal client.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use atomics_core::noise;
use atomics_core::ops::Grant;
use atomics_core::wire::Role;
use atomics_ui::theme::{FontHook, Fonts, Palette, definitions};
use atomics_ui::{Backend, Client, Command, PanelMode, ViewHints, approvals, board, diff_panel};
use egui::{Align, Layout, Rect, RichText, Sense, pos2, vec2};

use crate::boot::Boot;
use crate::driver::{Driver, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Board,
    Plans,
    Approvals,
    Diffs,
}

impl Tab {
    /// The tab a boot's `view` names; anything else opens on the board.
    pub fn named(view: &str) -> Tab {
        match view {
            "plans" => Tab::Plans,
            "approvals" => Tab::Approvals,
            "diffs" => Tab::Diffs,
            _ => Tab::Board,
        }
    }
}

/// What this frame shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub tab: Tab,
    /// The board's plan, by position in the plan list.
    pub plan: usize,
    /// The worktree whose diff is open.
    pub diff: Option<PathBuf>,
}

impl Default for View {
    fn default() -> Self {
        Self { tab: Tab::Board, plan: 0, diff: None }
    }
}

/// egui's bundled fonts: a browser has no font lookup, and a theme's named fonts are the desktop's.
pub struct BundledFonts;

impl FontHook for BundledFonts {
    fn apply(&self, ctx: &egui::Context, _fonts: &Fonts) {
        ctx.set_fonts(definitions(None, None, |_| None));
    }
}

/// Look as the hosting page does: its theme, or a built-in one of its scheme.
pub fn apply_theme(ctx: &egui::Context, client: &mut Client, boot: &Boot) {
    let palette = Arc::new(Palette::new(&boot.theme_tokens(), Fonts::default()));
    palette.apply(ctx, &BundledFonts);
    client.palette = palette;
}

/// One frame, and then what the user asked for in it.
pub fn draw(ui: &mut egui::Ui, driver: &mut Driver, client: &mut Client, view: &mut View) {
    sync(driver, client, view);
    let p = client.palette.clone();
    egui::Panel::top("atomics-top")
        .frame(egui::Frame::new().fill(p.topbar).inner_margin(egui::Margin::symmetric(12, 6)))
        .show(ui, |ui| top(ui, driver, view, &p));
    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(p.background).inner_margin(egui::Margin::same(8)))
        .show(ui, |ui| body(ui, driver, client, view));
    carry_out(driver, client, view);
}

/// Bring the client's own state in line with the backend before drawing.
fn sync(driver: &mut Driver, client: &mut Client, view: &View) {
    let remote = &driver.remote;
    for refused in remote.take_refused() {
        client.approval_decisions.remove(&refused);
    }
    let pending: HashSet<(String, String)> =
        remote.with_mirror(|m| m.approvals.values().map(|a| (a.session.clone(), a.id.clone())).collect());
    client.approval_decisions.retain(|decision| pending.contains(decision));
    if let Some(root) = &view.diff
        && let Some(data) = remote.diff(root)
    {
        let state = client.diff_views.entry(root.clone()).or_default();
        state.prepared = data.prepared;
        state.error = data.error;
        state.loading = data.loading;
        state.comments = data.comments;
    }
    remote.set_view(ViewHints {
        active: None,
        diff_target: view.diff.clone(),
        plan_view: view.tab == Tab::Board,
        ungrouped: false,
        project: None,
        selected_plan: view.plan,
        diff_open: view.diff.is_some(),
        panel: if view.tab == Tab::Diffs { PanelMode::Diff } else { PanelMode::Plan },
    });
}

fn status_line(driver: &Driver) -> (String, Option<bool>) {
    match driver.status() {
        Status::Waiting => ("Waiting for the page".into(), None),
        Status::Connecting => ("Connecting".into(), None),
        Status::Pairing { .. } => ("Pairing".into(), None),
        Status::Live => match driver.remote.welcome() {
            Some(welcome) if welcome.role == Role::Viewer => ("Connected · view only".into(), Some(true)),
            Some(_) => ("Connected".into(), Some(true)),
            None => ("Connecting".into(), None),
        },
        Status::Reconnecting { .. } => ("Reconnecting".into(), Some(false)),
        Status::Stopped(_) => ("Stopped".into(), Some(false)),
    }
}

fn top(ui: &mut egui::Ui, driver: &mut Driver, view: &mut View, p: &Palette) {
    let remote = &driver.remote;
    let (plans, approvals, diffs) = remote.with_mirror(|m| (m.plans.len(), m.approvals.len(), m.diffs.len()));
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("ATOMICS").color(p.accent).strong());
        let (text, good) = status_line(driver);
        let color = match good {
            Some(true) => p.success,
            Some(false) => p.warning,
            None => p.muted,
        };
        ui.label(RichText::new(text).color(color).small());
        ui.separator();
        let mut tab = |ui: &mut egui::Ui, which: Tab, label: String| {
            if ui.selectable_label(view.tab == which, label).clicked() {
                view.tab = which;
            }
        };
        tab(ui, Tab::Board, "Board".into());
        tab(ui, Tab::Plans, format!("Plans {plans}"));
        tab(ui, Tab::Approvals, if approvals > 0 { format!("Approvals ◆ {approvals}") } else { "Approvals".into() });
        tab(ui, Tab::Diffs, format!("Diffs {diffs}"));
    });
    if let Some(notice) = remote.notice() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(notice).small());
            if ui.small_button("×").on_hover_text("Dismiss").clicked() {
                remote.dismiss_notice();
            }
        });
    }
}

fn centred(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.with_layout(Layout::top_down(Align::Center), |ui| {
        ui.add_space((ui.available_height() / 4.0).min(160.0));
        add(ui);
    });
}

fn body(ui: &mut egui::Ui, driver: &mut Driver, client: &mut Client, view: &mut View) {
    let p = client.palette.clone();
    let has_board = driver.remote.with_mirror(|m| !m.plans.is_empty() || !m.sessions.is_empty());
    match driver.status().clone() {
        Status::Pairing { code } => {
            return centred(ui, |ui| {
                ui.label(RichText::new("Pairing this browser with your computer").size(18.0));
                ui.add_space(16.0);
                match code {
                    Some(code) => {
                        ui.label(RichText::new(noise::show_code(&code)).monospace().size(40.0).color(p.accent));
                        ui.add_space(16.0);
                        ui.label("Check that ATOMICS on the computer shows the same code, then accept it there.");
                    }
                    None => {
                        ui.label(RichText::new("Connecting…").color(p.muted));
                    }
                }
            });
        }
        Status::Stopped(stop) if !has_board => {
            return centred(ui, |ui| {
                ui.label(stop.message());
            });
        }
        Status::Stopped(stop) => {
            ui.label(RichText::new(stop.message()).color(p.error));
        }
        Status::Reconnecting { .. } if has_board => {
            ui.label(
                RichText::new("Reconnecting. The board may be out of date, and nothing can be changed until the computer is back.")
                    .color(p.warning),
            );
        }
        Status::Waiting | Status::Connecting | Status::Reconnecting { .. } if !has_board => {
            return centred(ui, |ui| {
                ui.label(RichText::new("Connecting to your computer…").color(p.muted));
            });
        }
        _ => {}
    }
    let backend: &dyn Backend = &driver.remote;
    match view.tab {
        Tab::Board => board_tab(ui, backend, client, view),
        Tab::Plans => plans_tab(ui, backend, client),
        Tab::Approvals => approvals_tab(ui, driver, client),
        Tab::Diffs => diffs_tab(ui, driver, client, view),
    }
}

fn empty(ui: &mut egui::Ui, client: &Client, text: &str) {
    let muted = client.palette.muted;
    centred(ui, |ui| {
        ui.label(RichText::new(text).color(muted));
    });
}

/// The rest of the panel, given to a draw function that paints where it is told.
fn fill(ui: &mut egui::Ui, draw: impl FnOnce(&mut egui::Ui, Rect)) {
    let rect = ui.available_rect_before_wrap();
    draw(ui, rect);
    ui.allocate_rect(rect, Sense::hover());
}

fn board_tab(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client, view: &mut View) {
    let plans = backend.plan_list();
    if plans.is_empty() {
        return empty(ui, client, "No plans on this computer yet.");
    }
    view.plan = view.plan.min(plans.len() - 1);
    if plans.len() > 1 {
        let p = client.palette.clone();
        ui.horizontal_wrapped(|ui| {
            for (index, row) in plans.iter().enumerate() {
                let label = RichText::new(row.state.label(&row.tab_name)).color(row.state.color(&p));
                if ui.selectable_label(index == view.plan, label).clicked() {
                    client.send(Command::ShowPlan(index));
                }
            }
        });
    }
    fill(ui, |ui, rect| board::draw(ui, rect, view.plan, backend, client));
}

fn plans_tab(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let plans = backend.plan_list();
    if plans.is_empty() {
        return empty(ui, client, "No plans on this computer yet.");
    }
    let p = client.palette.clone();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (index, row) in plans.iter().enumerate() {
            ui.horizontal_wrapped(|ui| {
                let label = RichText::new(row.state.label(&row.goal)).color(row.state.color(&p)).size(14.0);
                if ui.add(egui::Button::new(label).frame(false)).clicked() {
                    client.send(Command::ShowPlan(index));
                }
                let mut facts = format!("{}/{} done", row.done, row.total);
                if row.attention > 0 {
                    facts.push_str(&format!(" · {} need review", row.attention));
                }
                ui.label(RichText::new(facts).color(p.muted).small());
            });
            ui.separator();
        }
    });
}

fn approvals_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client) {
    let backend: &dyn Backend = &driver.remote;
    let sessions = backend.session_list();
    let waiting: Vec<usize> = (0..sessions.len()).filter(|i| backend.session_approval(*i).is_some()).collect();
    if waiting.is_empty() {
        return empty(ui, client, "Nothing is waiting for you.");
    }
    let p = client.palette.clone();
    if driver.remote.welcome().is_some() && !driver.remote.may(Grant::Approve) {
        ui.label(RichText::new("This browser may only view this computer.").color(p.muted));
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for index in waiting {
            let session = &sessions[index];
            ui.label(RichText::new(&session.title).strong());
            ui.label(RichText::new(backend.display(&session.root)).color(p.muted).small());
            let top = ui.cursor().min;
            let width = ui.available_width();
            // The banner takes what it needs of this; the rest stays free.
            let body = Rect::from_min_size(top, vec2(width, 380.0));
            let rest = approvals::banner(ui, body, index, backend, client);
            ui.allocate_rect(Rect::from_min_max(top, pos2(top.x + width, rest.min.y)), Sense::hover());
            ui.add_space(12.0);
        }
    });
}

fn diffs_tab(ui: &mut egui::Ui, driver: &Driver, client: &mut Client, view: &mut View) {
    let backend: &dyn Backend = &driver.remote;
    let roots = backend.diff_roots();
    if let Some(root) = view.diff.clone().filter(|root| roots.contains(root)) {
        return fill(ui, |ui, rect| diff_panel::draw(ui, rect, &root, backend, client));
    }
    if roots.is_empty() {
        return empty(ui, client, "No changes to review.");
    }
    let p = client.palette.clone();
    let rows: Vec<(String, usize, usize, usize)> = driver.remote.with_mirror(|m| {
        m.diffs.values().map(|d| (d.worktree.clone(), d.files.len(), d.additions(), d.deletions())).collect()
    });
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (worktree, files, added, removed) in rows {
            ui.horizontal_wrapped(|ui| {
                let name =
                    Path::new(&worktree).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                if ui.add(egui::Button::new(RichText::new(name).size(14.0)).frame(false)).clicked() {
                    client.send(Command::ShowRoot(PathBuf::from(&worktree)));
                }
                ui.label(RichText::new(format!("+{added}")).color(p.added).small());
                ui.label(RichText::new(format!("−{removed}")).color(p.removed).small());
                ui.label(RichText::new(format!("{files} files · {worktree}")).color(p.muted).small());
            });
            ui.separator();
        }
    });
}

/// Carry out what the draw recorded, in order: a view command here, any other through the backend.
fn carry_out(driver: &mut Driver, client: &mut Client, view: &mut View) {
    for command in std::mem::take(&mut client.commands) {
        if command.is_view() {
            view_command(driver, view, command);
        } else {
            driver.remote.send(command);
        }
    }
    driver.flush();
}

fn view_command(driver: &mut Driver, view: &mut View, command: Command) {
    let remote = &driver.remote;
    let session_root = |id: &str| remote.session_list().into_iter().find(|s| s.id == id).map(|s| s.root);
    let open = match command {
        Command::Activate { session, open_diff: true } | Command::OpenTaskSession(session) => session_root(&session),
        Command::ShowTaskDiff { plan, task } => remote
            .board(plan)
            .and_then(|b| b.plan.tasks.get(task).and_then(|t| t.session.clone()))
            .and_then(|session| session_root(&session)),
        Command::ShowRoot(root) => Some(root),
        Command::OpenDiff | Command::ToggleDiff | Command::SetPanel(PanelMode::Diff) => {
            view.tab = Tab::Diffs;
            None
        }
        Command::CloseDiff => {
            if let Some(old) = view.diff.take() {
                remote.unwant_diff(&old);
            }
            None
        }
        Command::SetPanel(PanelMode::Plan) | Command::TogglePlan => {
            view.tab = Tab::Board;
            None
        }
        Command::ShowPlan(index) | Command::ViewPlanHistory(index) => {
            view.plan = index;
            view.tab = Tab::Board;
            None
        }
        _ => None,
    };
    if let Some(root) = open {
        if let Some(old) = view.diff.replace(root.clone())
            && old != root
        {
            remote.unwant_diff(&old);
        }
        remote.want_diff(&root);
        view.tab = Tab::Diffs;
    }
}

#[cfg(test)]
mod tests;
