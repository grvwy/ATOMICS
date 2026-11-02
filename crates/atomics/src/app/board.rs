use super::*;

impl State {
    pub(super) fn toggle_plan(&mut self) {
        self.window.request_repaint();
        if self.panel == PanelMode::Plan && self.diff_open {
            self.panel = PanelMode::Diff;
        } else {
            self.panel = PanelMode::Plan;
            self.diff_open = true;
        }
    }
}

pub(super) use atomics_ui::status::task_color;

impl Desktop<'_> {
    /// The selected plan's board, drawn by atomics-ui from the plan's [`BoardView`](atomics_ui::BoardView).
    pub(super) fn plan_panel(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let index = self.runtime.selected_plan;
        self.draw(|backend, client| atomics_ui::board::draw(ui, rect, index, backend, client));
    }
}
