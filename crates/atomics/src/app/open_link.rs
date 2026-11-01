//! `atomics://open` links, once parsed (see `crate::open_link`): they focus the window and show
//! a plan, or the plan for a ticket, or draft one for the chief to read. They never do more.

use super::*;
use crate::open_link::{Incoming, OpenLink};
use crate::tickets::{TicketOp, TicketOutput};
use atomics_core::tickets::TicketLink;

impl State {
    /// Handles a link, toasting why when it cannot be opened.
    pub(super) fn open_link(&mut self, incoming: Incoming) -> Result<(), String> {
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        let result =
            incoming.map_err(|reason| format!("Ignored a link: {reason}")).and_then(|link| self.show_link(link));
        if let Err(message) = &result {
            self.toast(message.clone());
        }
        result
    }

    fn show_link(&mut self, link: OpenLink) -> Result<(), String> {
        if let Some(id) = link.plan {
            let Ok(index) = plan::resolve_plan(&self.plans, Some(&id)) else {
                return Err(format!("No open plan {}", id.chars().take(40).collect::<String>()));
            };
            self.show_plan_at(index);
            return Ok(());
        }
        let Some(reference) = link.ticket else { return Ok(()) };
        let Some(config) = self.ticket_config() else { return Err("Tickets are not configured".into()) };
        let system = config.system.trim();
        if let Some(wanted) = link.system.as_deref().filter(|wanted| !wanted.eq_ignore_ascii_case(system)) {
            let wanted: String = wanted.chars().take(40).collect();
            return Err(format!("Sign in to {wanted} to open this link; ATOMICS is set up for {system}"));
        }
        if self.plan_needs_sign_in(&config) {
            return Err(format!("Sign in to {system} first"));
        }
        let ticket = if reference.contains("://") {
            config.link_from_url(&reference).ok_or_else(|| format!("That address is not a {system} ticket"))?
        } else if reference.chars().any(|c| c.is_whitespace() || "/\\?&#".contains(c)) || reference.len() > 128 {
            return Err(format!("That is not a {system} ticket key"));
        } else {
            TicketLink {
                system: config.system.clone(),
                id: reference.clone(),
                key: Some(reference),
                ..Default::default()
            }
        };
        let linked = self.plans.iter().position(|plan| {
            let names = |t: &TicketLink| {
                t.same_ticket(&ticket) || ticket.url.as_deref().is_some_and(|url| t.url.as_deref() == Some(url))
            };
            plan.is_open()
                && (plan.ticket.as_ref().is_some_and(names)
                    || plan.tasks.iter().any(|task| task.ticket.as_ref().is_some_and(names)))
        });
        if let Some(index) = linked {
            self.show_plan_at(index);
            return Ok(());
        }
        // Nothing plans it yet: read it for its title, then offer a plan as an unsent draft.
        let panel = &mut self.tickets.panel;
        panel.seq += 1;
        let seq = panel.seq;
        panel.open_link = Some(seq);
        let tag = self.panel_tag(&config, super::tickets_panel::Job::Link, seq);
        self.send_ticket(&config, TicketOp::ReadCard(ticket), tag);
        Ok(())
    }

    fn show_plan_at(&mut self, index: usize) {
        self.selected_plan = index;
        self.show_plan_fleet();
    }

    /// The answer to the read a link started: the draft "Plan KEY: title" for the chief.
    pub(super) fn link_card_read(&mut self, result: Result<TicketOutput, (bool, String)>) {
        match result {
            Ok(TicketOutput::Card(card)) => {
                let cleaner = self.cleaner();
                let key = cleaner.field(card.link.label());
                let title = cleaner.field(card.link.title.as_deref().unwrap_or_default());
                let line = |text: String| text.split_whitespace().collect::<Vec<_>>().join(" ");
                let draft = match line(title).as_str() {
                    "" => format!("Plan {}", line(key)),
                    title => format!("Plan {}: {title}", line(key)),
                };
                self.draft_for_chief(draft);
            }
            Ok(_) => self.toast("Could not read that ticket"),
            Err((true, _)) => {
                let system = self.ticket_config().map(|config| config.system.clone()).unwrap_or_default();
                self.toast(format!("Sign in to {system} first"));
            }
            Err((false, error)) => self.toast(format!("Could not read that ticket: {error}")),
        }
    }

    /// Opens the chief with `text` typed into its prompt and not sent: the user reads it and presses Enter.
    fn draft_for_chief(&mut self, text: String) {
        if !self.sessions.iter().any(|s| s.chief && s.status != AgentStatus::Exited) {
            self.sessions.retain(|s| !s.chief);
            let ctx = self.ctx.clone();
            if let Err(error) = self.start_chief(&ctx, None) {
                self.toast(error);
                return;
            }
        }
        self.chief_draft = Some(text);
        self.show_chief();
        self.flush_chief_draft();
    }

    /// Types the draft into the chief's prompt once the chief is up to read it.
    pub(super) fn flush_chief_draft(&mut self) {
        let Some(text) = &self.chief_draft else { return };
        let ready = self.sessions.iter().find(|s| s.chief && s.status != AgentStatus::Starting);
        if let Some(terminal) = ready.and_then(|s| s.terminal.as_ref()).filter(|t| !t.exited()) {
            terminal.draft(text);
            self.chief_draft = None;
        }
    }
}
