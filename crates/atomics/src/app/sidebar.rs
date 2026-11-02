use atomics_ui::SessionRow;
use atomics_ui::sidebar::section;

use super::*;

#[derive(Clone)]
enum ViewTab {
    All,
    /// Every session outside a plan.
    Ungrouped,
    Repos,
    Project(PathBuf),
    Plan(usize),
}

impl State {
    /// Whether a session belongs to no plan's task.
    pub(super) fn ungrouped_session(&self, session: &Session) -> bool {
        !session.chief && !self.plans.iter().any(|plan| plan.contains_session(&session.id))
    }

    /// Shows the Ungrouped tab, keeping the active session if it is there.
    pub(super) fn show_ungrouped(&mut self) {
        self.plan_view = false;
        self.view = None;
        self.ungrouped = true;
        if !self.active_index().is_some_and(|i| self.visible_sessions().contains(&i)) {
            match self.visible_sessions().first().map(|&i| self.sessions[i].id.clone()) {
                Some(id) => self.activate(&id, false),
                None => {
                    self.active = None;
                    self.diff_root = None;
                }
            }
        }
    }

    pub(super) fn set_view(&mut self, view: Option<PathBuf>) {
        self.plan_view = false;
        self.ungrouped = false;
        self.view = view.clone();
        let Some(project) = view else { return };
        let in_view = self.active_index().is_some_and(|i| {
            !self.sessions[i].chief && self.project_of(&self.sessions[i].root()).as_ref() == Some(&project)
        });
        if !in_view {
            match self.visible_sessions().first().map(|&i| self.sessions[i].id.clone()) {
                Some(id) => self.activate(&id, false),
                None => {
                    self.active = None;
                    self.diff_root = Some(project);
                }
            }
        }
    }
}

/// A project's agents, and the lines they changed.
fn project_summary(sessions: &[SessionRow], root: &Path) -> (usize, usize) {
    let agents = sessions.iter().filter(|s| !s.chief && s.project.as_deref() == Some(root));
    agents.fold((0, 0), |(count, changes), session| {
        (count + 1, changes + session.changes.map_or(0, |(additions, deletions)| additions + deletions))
    })
}

fn measure_divider(ui: &mut egui::Ui) -> f32 {
    super::topbar::measure(ui, |ui| {
        ui.separator();
    })
}

/// What a plan's tab shows: its ticket's key with progress when it has one, else its goal.
pub(super) fn plan_tab_name(plan: &Plan) -> String {
    let Some(ticket) = plan.ticket.as_ref() else { return plan.goal.clone() };
    if plan.status == PlanStatus::Open && plan.tasks.is_empty() {
        return format!("{} · planning…", ticket.label());
    }
    let done = plan.count(TaskStatus::Done);
    format!("{} · {done}/{}", ticket.label(), plan.tasks.len())
}

impl Desktop<'_> {
    /// The TICKETS section: a plan's ticket tree in its view, else Find and the last tickets read.
    fn tickets_section(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(config) = self.runtime.ticket_config() else { return };
        let mut open: Option<atomics_core::tickets::TicketLink> = None;
        let mut find = false;
        section(ui, p, "TICKETS");
        let plan = self.runtime.plan_view.then(|| &self.runtime.plans[self.runtime.selected_plan]);
        let row = |ui: &mut egui::Ui, depth: usize, link: &atomics_core::tickets::TicketLink, status: Option<TaskStatus>| {
            let dot = match (&link.state, status) {
                (Some(state), _) => config.states.status_of(state).map_or(p.muted, |s| super::board::task_color(s, p)),
                (None, Some(status)) => super::board::task_color(status, p),
                (None, None) => p.muted,
            };
            let mut text = egui::text::LayoutJob::default();
            text.append("● ", 0.0, egui::TextFormat { font_id: p.ui(10.0), color: dot, ..Default::default() });
            text.append(
                &format!("{} {}", link.label(), link.title.as_deref().unwrap_or("")),
                0.0,
                egui::TextFormat { font_id: p.ui(12.0), color: p.foreground, ..Default::default() },
            );
            ui.horizontal(|ui| {
                ui.add_space(depth as f32 * 12.0);
                ui.add(egui::Button::selectable(false, text).truncate()).on_hover_text(link.title.as_deref().unwrap_or(""))
            })
            .inner
            .clicked()
        };
        match plan.and_then(|plan| plan.ticket.as_ref().map(|ticket| (plan, ticket))) {
            Some((plan, ticket)) => {
                if row(ui, 0, ticket, None) {
                    open = Some(ticket.clone());
                }
                for task in &plan.tasks {
                    if let Some(link) = &task.ticket
                        && row(ui, 1, link, Some(task.status))
                    {
                        open = Some(link.clone());
                    }
                }
                for item in self.runtime.tickets.boards.get(&plan.id).into_iter().flatten() {
                    let tasked = plan.tasks.iter().any(|t| t.ticket.as_ref().is_some_and(|l| l.same_ticket(item)));
                    if !tasked && row(ui, 1, item, None) {
                        open = Some(item.clone());
                    }
                }
            }
            None => {
                if ui.add(egui::Button::selectable(false, RichText::new("Find tickets…").size(12.0))).clicked() {
                    find = true;
                }
                for link in &self.runtime.tickets.panel.recent {
                    if row(ui, 0, link, None) {
                        open = Some(link.clone());
                    }
                }
            }
        }
        ui.add_space(10.0);
        if find {
            self.runtime.show_find();
        } else if let Some(link) = open {
            self.runtime.open_ticket(link, tickets_panel::Nav::Fresh);
        }
    }
}

impl Desktop<'_> {
    /// Tabs share a bounded navigation slot. First shorten labels, then move
    /// excess tabs into a menu, keeping the selected tab in the visible row.
    pub(super) fn urgent_tabs_label(&self) -> (String, Color32) {
        let rows = self.runtime.backend().plan_list();
        self.tabs_summary(&rows, 0..rows.len())
    }

    fn tabs_summary(&self, rows: &[PlanRow], indices: impl Iterator<Item = usize>) -> (String, Color32) {
        let mut needs = 0;
        let mut running = 0;
        for index in indices {
            match rows[index].state {
                PlanState::NeedsYou => needs += 1,
                PlanState::Running(count) => running += count,
                _ => {}
            }
        }
        if needs > 0 {
            (format!("Tabs · {needs} needs you"), self.palette().warning)
        } else if running > 0 {
            (format!("Tabs · {running} running"), self.palette().accent)
        } else {
            ("Tabs".into(), self.palette().foreground)
        }
    }

    fn plan_label_width(&self, ui: &mut egui::Ui, label: &str) -> f32 {
        super::topbar::measure(ui, |ui| {
            ui.add(egui::Button::new(RichText::new(label).font(self.palette().ui(12.5))).right_text("   "));
        })
    }

    pub(super) fn urgent_navigation_width(&self, ui: &mut egui::Ui) -> f32 {
        let gap = ui.spacing().item_spacing.x;
        let menu = super::topbar::measure(ui, |ui| {
            ui.menu_button("Tabs", |_| {});
        });
        let rows = self.runtime.backend().plan_list();
        let protected = rows
            .iter()
            .filter(|row| row.state.urgency() > 0)
            .map(|row| {
                let marker = if row.changed { " *" } else { "" };
                self.plan_label_width(ui, &format!("{}{marker}", row.state.label("…"))) + gap
            })
            .sum::<f32>();
        let history = if protected > 0.0 && rows.iter().any(|row| !row.id.is_empty() && row.status != PlanStatus::Open)
        {
            super::topbar::measure(ui, |ui| {
                ui.menu_button("History", |_| {});
            }) + gap
        } else {
            0.0
        };
        protected + menu + history
    }

    pub(super) fn view_tabs(&mut self, ui: &mut egui::Ui) {
        let rows = self.runtime.backend().plan_list();
        let mut tabs = vec![ViewTab::All, ViewTab::Ungrouped, ViewTab::Repos];
        tabs.extend(
            rows.iter()
                .enumerate()
                .filter(|(index, row)| {
                    !row.id.is_empty()
                        && (row.status != PlanStatus::Archived
                            || self.runtime.plan_view && self.runtime.selected_plan == *index)
                })
                .map(|(index, _)| ViewTab::Plan(index)),
        );
        let gap = ui.spacing().item_spacing.x;
        let widths: Vec<f32> =
            tabs.iter().map(|tab| super::topbar::measure(ui, |ui| self.view_tab(ui, tab, None, None))).collect();
        let minimum: Vec<f32> = tabs
            .iter()
            .zip(&widths)
            .map(|(tab, width)| {
                if let ViewTab::Plan(index) = tab {
                    let row = &rows[*index];
                    if row.state.urgency() > 0 {
                        let marker = if row.changed { " *" } else { "" };
                        return self.plan_label_width(ui, &format!("{}{marker}", row.state.label("…")));
                    }
                }
                width.min(90.0)
            })
            .collect();
        let mut priority: Vec<usize> = (0..tabs.len()).collect();
        priority.sort_by_key(|&index| match &tabs[index] {
            ViewTab::Plan(plan) if rows[*plan].state == PlanState::NeedsYou => 0,
            ViewTab::Plan(plan) if matches!(rows[*plan].state, PlanState::Running(_)) => 1,
            ViewTab::All | ViewTab::Ungrouped | ViewTab::Repos if self.view_tab_selected(&tabs[index]) => 2,
            ViewTab::All | ViewTab::Ungrouped | ViewTab::Repos => 3,
            ViewTab::Plan(_) if self.view_tab_selected(&tabs[index]) => 4,
            ViewTab::Plan(plan) if rows[*plan].status == PlanStatus::Open => 5,
            _ => 6,
        });
        let has_plans = tabs.iter().any(|tab| matches!(tab, ViewTab::Plan(_)));
        let divider = if has_plans { measure_divider(ui) + gap } else { 0.0 };
        let closed = rows.iter().any(|row| !row.id.is_empty() && row.status != PlanStatus::Open);
        let history_width = if closed {
            super::topbar::measure(ui, |ui| {
                ui.menu_button("History", |_| {});
            }) + gap
        } else {
            0.0
        };
        let available = ui.available_width();
        let total = |widths: &[f32]| widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
        let all_fit = total(&widths) + divider + history_width <= available;
        let protected: f32 = tabs
            .iter()
            .zip(&minimum)
            .filter_map(|(tab, width)| {
                if let ViewTab::Plan(index) = tab
                    && rows[*index].state.urgency() > 0
                {
                    Some(width + gap)
                } else {
                    None
                }
            })
            .sum();
        let mut menu_label = "Tabs".to_string();
        let mut menu_color = self.palette().foreground;
        let mut menu_width = super::topbar::measure(ui, |ui| {
            ui.menu_button("Tabs", |_| {});
        });
        let mut allocated = minimum.clone();
        let mut visible = Vec::new();
        let mut pin_history = closed;
        let mut content_budget = available - divider - history_width;
        if all_fit {
            visible.extend(0..tabs.len());
        } else {
            for _ in 0..=tabs.len() {
                pin_history = closed && available - divider - history_width >= protected.max(90.0) + menu_width + gap;
                content_budget =
                    (available - divider - if pin_history { history_width } else { 0.0 } - menu_width - gap).max(0.0);
                visible.clear();
                allocated.clone_from(&minimum);
                let urgent: Vec<usize> = priority
                    .iter()
                    .copied()
                    .filter(|&index| matches!(tabs[index],ViewTab::Plan(plan) if rows[plan].state.urgency()>0))
                    .collect();
                let full_urgent_width = urgent.iter().map(|&index| widths[index]).sum::<f32>()
                    + gap * urgent.len().saturating_sub(1) as f32;
                let full_urgent = full_urgent_width <= content_budget;
                let mut used = 0.0;
                for &index in &priority {
                    let is_urgent = urgent.contains(&index);
                    // Idle/completed tabs only use space left after the important
                    // names and their state suffixes have been protected.
                    if !is_urgent && !full_urgent && !urgent.is_empty() {
                        continue;
                    }
                    let width = if is_urgent && full_urgent { widths[index] } else { minimum[index] };
                    let needed = width + if visible.is_empty() { 0.0 } else { gap };
                    if used + needed <= content_budget {
                        used += needed;
                        visible.push(index);
                        allocated[index] = width;
                    } else if !is_urgent {
                        break;
                    }
                }
                (menu_label, menu_color) = self.tabs_summary(
                    &rows,
                    tabs.iter().enumerate().filter_map(|(index, tab)| {
                        if !visible.contains(&index)
                            && let ViewTab::Plan(plan) = tab
                        {
                            Some(*plan)
                        } else {
                            None
                        }
                    }),
                );
                let measured = super::topbar::measure(ui, |ui| {
                    ui.menu_button(RichText::new(&menu_label).color(menu_color), |_| {});
                });
                if measured <= menu_width + 0.5 {
                    break;
                }
                menu_width = measured;
            }
        }
        let overflow = visible.len() < tabs.len() || closed && !pin_history;
        if all_fit {
            allocated.clone_from(&widths);
        } else {
            let used = visible.iter().map(|&index| allocated[index]).sum::<f32>()
                + gap * visible.len().saturating_sub(1) as f32;
            let mut extra = (content_budget - used).max(0.0);
            for &index in &priority {
                if visible.contains(&index) {
                    let grow = extra.min((widths[index] - allocated[index]).max(0.0));
                    allocated[index] += grow;
                    extra -= grow;
                }
            }
        }
        visible.sort_unstable();
        let mut project_drawn = false;
        let mut divided = false;
        for &index in &visible {
            if matches!(tabs[index], ViewTab::Plan(_)) && project_drawn && !divided {
                ui.separator();
                divided = true;
            }
            project_drawn |= !matches!(tabs[index], ViewTab::Plan(_));
            self.view_tab(ui, &tabs[index], Some(allocated[index]), Some(widths[index]));
        }
        if overflow {
            if has_plans && project_drawn && !divided {
                ui.separator();
            }
            let response = ui
                .menu_button(RichText::new(&menu_label).color(menu_color), |ui| {
                    ui.set_max_width(500.0_f32.min(ui.ctx().content_rect().width() - 32.0));
                    for (index, tab) in tabs.iter().enumerate().filter(|(_, tab)| !matches!(tab, ViewTab::Plan(_))) {
                        if !visible.contains(&index) {
                            self.view_tab(ui, tab, None, None);
                        }
                    }
                    if tabs
                        .iter()
                        .enumerate()
                        .any(|(index, tab)| !visible.contains(&index) && !matches!(tab, ViewTab::Plan(_)))
                    {
                        ui.separator();
                    }
                    for &index in &priority {
                        if !visible.contains(&index) && matches!(tabs[index], ViewTab::Plan(_)) {
                            self.view_tab(ui, &tabs[index], None, None);
                        }
                    }
                    if closed && !pin_history {
                        ui.separator();
                        self.history_menu(ui);
                    }
                })
                .response;
            super::topbar::record_item(ui, "Tabs", &response);
        }
        if pin_history {
            self.history_menu(ui);
        }
    }

    fn view_tab_selected(&self, tab: &ViewTab) -> bool {
        match tab {
            ViewTab::All => !self.runtime.plan_view && !self.runtime.ungrouped && self.runtime.view.is_none(),
            ViewTab::Ungrouped => !self.runtime.plan_view && self.runtime.ungrouped,
            ViewTab::Repos => !self.runtime.plan_view && !self.runtime.ungrouped && self.runtime.view.is_some(),
            ViewTab::Project(root) => {
                !self.runtime.plan_view && !self.runtime.ungrouped && self.runtime.view.as_ref() == Some(root)
            }
            ViewTab::Plan(index) => self.runtime.plan_view && self.runtime.selected_plan == *index,
        }
    }

    fn repos_tab(&mut self, ui: &mut egui::Ui, width: Option<f32>) {
        let selected = self.view_tab_selected(&ViewTab::Repos);
        let label = if selected {
            self.runtime
                .view
                .as_ref()
                .and_then(|root| root.file_name())
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        } else {
            let sessions = self.runtime.backend().session_list();
            let changes: usize = self.runtime.repos.iter().map(|root| project_summary(&sessions, root).1).sum();
            format!("Repos · {changes}")
        };
        let color = if selected { self.palette().accent } else { self.palette().foreground };
        let mut menu = |ui: &mut egui::Ui| {
            ui.menu_button(RichText::new(&label).font(self.palette().ui(12.5)).color(color), |ui| {
                for root in self.runtime.repos.clone() {
                    self.view_tab(ui, &ViewTab::Project(root), None, None);
                }
            })
            .response
        };
        let response = match width {
            Some(width) => {
                ui.allocate_ui_with_layout(vec2(width, ui.spacing().interact_size.y), *ui.layout(), |ui| {
                    ui.set_min_width(width);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    menu(ui)
                })
                .inner
            }
            None => menu(ui),
        };
        if width.is_some() {
            super::topbar::record_item(ui, &label, &response);
        }
    }

    fn view_tab(&mut self, ui: &mut egui::Ui, tab: &ViewTab, width: Option<f32>, natural_width: Option<f32>) {
        if matches!(tab, ViewTab::Repos) {
            self.repos_tab(ui, width);
            return;
        }
        let p = self.palette();
        let selected = self.view_tab_selected(tab);
        let plan_row = match tab {
            ViewTab::Plan(index) => self.runtime.backend().plan_row(*index),
            _ => None,
        };
        let (label, hover, color) = match tab {
            ViewTab::All => {
                let agents = self.runtime.backend().session_list().iter().filter(|s| !s.chief).count();
                (format!("All · {agents}"), "All projects".into(), p.foreground)
            }
            ViewTab::Ungrouped => {
                let count = self.runtime.backend().session_list().iter().filter(|s| s.ungrouped).count();
                let hover = "Sessions outside any plan. New sessions start here, in scratch folders of their own";
                (format!("Ungrouped · {count}"), hover.into(), if selected { p.accent } else { p.foreground })
            }
            ViewTab::Project(root) => {
                let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (count, changes) = project_summary(&self.runtime.backend().session_list(), root);
                (
                    format!("{name} · {count} sessions · {changes} changes"),
                    paths::display(root),
                    if selected { p.accent } else { p.foreground },
                )
            }
            ViewTab::Repos => unreachable!(),
            ViewTab::Plan(_) => {
                let row = plan_row.as_ref().expect("a plan tab has a plan");
                let state = row.state;
                let marker = if row.changed { " *" } else { "" };
                let mut name: String = row.tab_name.chars().take(28).collect();
                if state.urgency() > 0
                    && let Some(width) = width
                {
                    let natural = natural_width.unwrap_or_else(|| self.plan_label_width(ui, &state.label(&name)));
                    let font = p.ui(12.5);
                    let budget = ui
                        .fonts_mut(|fonts| fonts.layout_no_wrap(name.clone(), font.clone(), state.color(&p)).size().x)
                        - (natural - width).max(0.0);
                    if natural > width {
                        let mut chars: Vec<char> = name.chars().collect();
                        loop {
                            name = format!("{}…", chars.iter().collect::<String>());
                            if chars.is_empty()
                                || ui.fonts_mut(|fonts| {
                                    fonts.layout_no_wrap(name.clone(), font.clone(), state.color(&p)).size().x
                                }) <= budget
                            {
                                break;
                            }
                            chars.pop();
                        }
                    }
                }
                (format!("{}{marker}", state.label(&name)), row.goal.clone(), state.color(&p))
            }
        };
        let state = plan_row.as_ref().map(|row| row.state);
        let mut text = egui::text::LayoutJob::default();
        let mut rest = label.as_str();
        if matches!(state, Some(PlanState::Running(_))) {
            let pulse = 0.55 + 0.45 * (ui.input(|input| input.time) * 3.2).sin() as f32;
            text.append(
                "● ",
                0.0,
                egui::TextFormat { font_id: p.ui(12.5), color: color.gamma_multiply(pulse), ..Default::default() },
            );
            rest = rest.strip_prefix("● ").unwrap_or(rest);
            ui.ctx().request_repaint_after(PULSE_STEP);
        }
        text.append(rest, 0.0, egui::TextFormat { font_id: p.ui(12.5), color, ..Default::default() });
        let fill = match state {
            Some(PlanState::Running(_)) => p.accent.gamma_multiply(0.35),
            Some(PlanState::NeedsYou) => p.warning.gamma_multiply(0.12),
            _ => Color32::TRANSPARENT,
        };
        let mut button = egui::Button::new(text)
            .fill(fill)
            .stroke(if selected { Stroke::new(1.0, p.accent) } else { Stroke::NONE })
            .truncate();
        if state.is_some() {
            button = button.right_text("   ");
        }
        let response = match width {
            Some(width) => ui.add_sized(vec2(width, ui.spacing().interact_size.y), button),
            None => ui.add(button),
        }
        .on_hover_text(hover);
        if width.is_some() {
            super::topbar::record_item(ui, &label, &response);
        }
        let mut close_clicked = false;
        let mut over_close = false;
        if let ViewTab::Plan(index) = tab {
            let close =
                Rect::from_center_size(pos2(response.rect.right() - 12.0, response.rect.center().y), vec2(18.0, 18.0));
            let _id = egui::Id::NULL.with(("plan-close", plan_row.as_ref().map_or("", |row| row.id.as_str())));
            let hit = ui.interact(close, response.id.with("close"), Sense::click());
            over_close = ui.input(|input| input.pointer.hover_pos().is_some_and(|pos| close.contains(pos)));
            if selected || response.hovered() || hit.hovered() {
                if hit.hovered() {
                    ui.painter().rect_filled(close, 2.0, p.raised);
                }
                let center = close.center();
                for sign in [-1.0, 1.0] {
                    ui.painter().line_segment(
                        [center + vec2(-3.0, sign * -3.0), center + vec2(3.0, sign * 3.0)],
                        Stroke::new(1.0, p.muted),
                    );
                }
                #[cfg(test)]
                if !ui.is_sizing_pass() {
                    ui.ctx().data_mut(|data| data.insert_temp(_id, close));
                }
            }
            close_clicked = hit.clicked() || response.clicked() && over_close;
            if close_clicked {
                self.close_plan_tab(*index);
                if width.is_none() {
                    ui.close();
                }
            }
        }
        if response.clicked() && !close_clicked && !over_close {
            match tab {
                ViewTab::All => self.runtime.set_view(None),
                ViewTab::Ungrouped => self.runtime.show_ungrouped(),
                ViewTab::Project(root) => self.runtime.set_view(Some(root.clone())),
                ViewTab::Repos => unreachable!(),
                ViewTab::Plan(index) => {
                    if plan_row.as_ref().is_some_and(|row| row.status == PlanStatus::Open) {
                        self.runtime.selected_plan = *index;
                        self.runtime.show_plan_fleet();
                    } else {
                        self.view_plan_history(*index);
                    }
                }
            }
            if width.is_none() {
                ui.close();
            }
        }
        match tab {
            ViewTab::Project(root) => {
                response.context_menu(|ui| {
                    if ui
                        .button("Close idle sessions")
                        .on_hover_text("Idle or exited sessions no plan task uses")
                        .clicked()
                    {
                        let root = root.clone();
                        self.post(move |state| {
                            let closed = state.close_idle(&root);
                            state.toast(format!("Closed {closed} idle session{}", if closed == 1 { "" } else { "s" }));
                        });
                        ui.close();
                    }
                });
            }
            ViewTab::Plan(index) if plan_row.as_ref().is_some_and(|row| row.status == PlanStatus::Completed) => {
                response.context_menu(|ui| {
                    if ui.button("Archive plan").clicked() {
                        self.close_plan_tab(*index);
                        ui.close();
                    }
                });
            }
            _ => {}
        }
    }

    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let files_root = self.runtime.diff_target();
        section(ui, &p, "USAGE");
        self.runtime.usage.show(ui, &p);
        ui.separator();
        let total = ui.available_height();
        let sessions_height = if files_root.is_some() { (total * 0.46).max(150.0) } else { total };
        ui.add_space(6.0);
        egui::ScrollArea::vertical().id_salt("sidebar").max_height(sessions_height).auto_shrink([false, true]).show(
            ui,
            |ui| {
                self.tickets_section(ui, &p);
                self.draw(|backend, client| atomics_ui::sidebar::sessions(ui, backend, client));
            },
        );

        ui.add_space(4.0);
        ui.separator();
        self.draw(|backend, client| atomics_ui::sidebar::explorer(ui, backend, client));
    }

    pub(super) fn project_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Open project…").clicked() {
            ui.close();
            self.pick_project();
        }
        let recent: Vec<PathBuf> = self
            .store
            .recent_projects
            .iter()
            .filter(|p| !self.runtime.open_projects.contains(p) && p.is_dir())
            .take(8)
            .cloned()
            .collect();
        if !recent.is_empty() {
            ui.separator();
            ui.label(RichText::new("Recent projects").size(11.0).color(self.palette().muted));
            for root in recent {
                if ui.button(paths::display(&root)).clicked() {
                    self.open_project(&root);
                    ui.close();
                }
            }
        }
        ui.separator();
        ui.label(RichText::new("Workspaces").size(11.0).color(self.palette().muted));
        for name in Workspace::list() {
            let current = self.runtime.workspace.as_deref() == Some(name.as_str());
            if ui.selectable_label(current, &name).clicked() {
                self.open_workspace(&name);
                ui.close();
            }
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.client.workspace_input).hint_text("name").desired_width(120.0));
            let name = self.client.workspace_input.trim().to_string();
            if ui
                .add_enabled(
                    !name.is_empty() && !self.runtime.open_projects.is_empty(),
                    egui::Button::new("Save workspace"),
                )
                .clicked()
            {
                self.save_workspace(&name);
                self.client.workspace_input.clear();
                ui.close();
            }
        });
    }

    pub(super) fn settings_menu(&mut self, ui: &mut egui::Ui) {
        if ui.button("Edit config").clicked() {
            let _ = open_path(&self.runtime.config_path);
            ui.close();
        }
        if global_hooks_installed() {
            ui.label(RichText::new("Hooks for all Claude sessions are installed").size(12.0));
        } else if ui
            .button("Install hooks for all Claude sessions")
            .on_hover_text(
                "Adds `atomics hook` to ~/.claude/settings.json so agents in any terminal show up here and can be steered",
            )
            .clicked()
        {
            match install_global_hooks() {
                Ok(path) => self.runtime.toast(format!("Hooks added to {}", paths::display(&path))),
                Err(err) => self.runtime.toast(format!("Installing hooks failed: {err:#}")),
            }
            ui.close();
        }
        ui.separator();
        self.themes_menu(ui);
        self.sandbox_menu(ui);
        self.credentials_menu(ui);
        self.connections_menu(ui);
        ui.separator();
        let keys = &self.runtime.config.keys;
        for (key, what) in [
            (&keys.quick_open, "open file"),
            (&keys.toggle_diff, "show/hide diff"),
            (&keys.next_attention, "next agent that needs you"),
            (&keys.focus_steer, "steer bar"),
            (&keys.new_session, "new session"),
            (&keys.voice, "voice: record, send (Esc cancels)"),
            (&keys.github, "GitHub controls"),
        ] {
            ui.label(RichText::new(format!("{}  {what}", shortcut_label(ui.ctx(), key))).size(12.0).monospace());
        }
    }

}

#[cfg(test)]
mod tests {
    use super::super::tests::{request, session, with_app};
    use super::*;

    #[test]
    fn clicking_plan_tabs_and_approving_boards_targets_only_the_selected_plan() {
        with_app(
            "app::sidebar::tests::clicking_plan_tabs_and_approving_boards_targets_only_the_selected_plan",
            |app, ctx| {
                app.runtime.repos = (0..8).map(|i| PathBuf::from(format!("/project/repo-{i}"))).collect();
                for goal in ["Atlas", "Beacon"] {
                    assert!(
                        request(
                            app,
                            json!({"cmd":"plan_set", "goal":goal, "tasks":[
                    {"id":"a", "title":format!("{goal} task"), "brief":format!("{goal} instructions")}]})
                        )
                        .ok
                    );
                }
                let one = app.runtime.plans.iter().position(|p| p.goal == "Atlas").unwrap();
                let two = app.runtime.selected_plan;
                app.runtime.plans.push(Plan {
                    id: "archived".into(),
                    goal: "Archived".into(),
                    status: plan::PlanStatus::Archived,
                    ..Plan::default()
                });
                app.runtime.plans.push(Plan {
                    id: "completed".into(),
                    goal: "Completed".into(),
                    status: plan::PlanStatus::Completed,
                    ..Plan::default()
                });
                for (index, id) in [(one, "atlas-agent"), (two, "beacon-agent")] {
                    app.runtime.plans[index].tasks[0].session = Some(id.into());
                    app.runtime.sessions.push(session(
                        id,
                        AgentKind::Generic,
                        AgentStatus::Working,
                        Path::new("/project"),
                    ));
                }
                let mut time = 0.0;
                let mut frame = |app: &mut Desktop<'_>, events| {
                    time += 0.05;
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1700.0, 1000.0))),
                            time: Some(time),
                            events,
                            ..Default::default()
                        },
                        |ui| {
                            egui::Panel::top("test-top").exact_size(34.0).show(ui, |ui| app.top_bar(ui));
                            egui::CentralPanel::default().show(ui, |ui| {
                                if app.runtime.panel == PanelMode::Plan {
                                    app.plan_panel(ui, ui.max_rect());
                                }
                            });
                            app.dialogs(ctx);
                        },
                    );
                    app.runtime.drain_actions();
                    let text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::epaint::Shape::Text(t) => {
                                Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2()))
                            }
                            _ => None,
                        })
                        .collect();
                    output.drop_without_applying_deltas();
                    text
                };
                let button = |pos, pressed| egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                };
                for (goal, index, visible) in [("Atlas", one, 0), ("Beacon", two, 1)] {
                    let text = frame(app, vec![]);
                    assert!(text.iter().any(|(t, _)| t == "Repos · 0"));
                    assert!(!text.iter().any(|(t, _)| t.starts_with("repo-")));
                    assert!(!text.iter().any(|(t, _)| t.contains("Archived")));
                    assert!(text.iter().any(|(t, _)| t.starts_with("✓ Completed")));
                    let pos = text.iter().find(|(t, _)| t.starts_with(&format!("● {goal} ·"))).unwrap().1;
                    frame(app, vec![egui::Event::PointerMoved(pos), button(pos, true)]);
                    frame(app, vec![button(pos, false)]);
                    assert_eq!(app.runtime.selected_plan, index);
                    assert!(app.runtime.plan_view);
                    assert_eq!(app.runtime.visible_sessions(), [visible]);
                    app.runtime.toggle_plan();
                    let text = frame(app, vec![]);
                    assert!(text.iter().any(|(t, _)| t == &format!("{goal} task")));
                    let other = if goal == "Atlas" { "Beacon task" } else { "Atlas task" };
                    assert!(!text.iter().any(|(t, _)| t == other));
                    assert!(!text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                    let task_pos = text.iter().find(|(t, _)| t == &format!("{goal} task")).unwrap().1;
                    frame(app, vec![egui::Event::PointerMoved(task_pos), button(task_pos, true)]);
                    frame(app, vec![button(task_pos, false)]);
                    let text = frame(app, vec![]);
                    assert!(text.iter().any(|(t, _)| t == &format!("{goal} instructions")));
                    let approve = text.iter().find(|(t, _)| t == "Approve plan").unwrap().1;
                    frame(app, vec![egui::Event::PointerMoved(approve), button(approve, true)]);
                    frame(app, vec![button(approve, false)]);
                    assert!(app.runtime.plans[index].approved);
                    if index == one {
                        assert!(!app.runtime.plans[two].approved);
                    }
                }
                assert_eq!(app.runtime.sessions.len(), 2, "closing plans does not terminate their sessions");
            },
        );
    }
}
