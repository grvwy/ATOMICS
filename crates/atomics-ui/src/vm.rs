//! Local VM management. This capability belongs to the computer drawing the window, never the
//! remote host or the wire protocol. Browser backends leave it absent.
use crate::{Backend, Client};
use egui::RichText;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    None,
    Stopped,
    Starting,
    Running,
    Unknown(String),
}
impl State {
    pub fn label(&self) -> &str {
        match self {
            Self::None => "not set up",
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Unknown(note) => note,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resources {
    pub cpus: u32,
    pub memory_gib: u64,
    pub disk_gib: u64,
}
impl Default for Resources {
    fn default() -> Self {
        Self { cpus: 4, memory_gib: 8, disk_gib: 100 }
    }
}
#[derive(Clone, Debug, Default)]
pub struct Artifact {
    pub built: bool,
    pub version: String,
    pub date: String,
}
#[derive(Clone, Debug, Default)]
pub struct Instance {
    pub name: String,
    pub state: State,
    pub resources: Resources,
    pub in_use: bool,
}
#[derive(Clone, Debug, Default)]
pub struct SignIn {
    pub name: String,
    pub state: String,
    pub label: String,
}
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub supported: bool,
    pub source: String,
    pub instance: String,
    pub image: Artifact,
    pub bundle: Artifact,
    pub installed: Option<Artifact>,
    pub outdated: bool,
    pub instances: Vec<Instance>,
    pub defaults: Resources,
    pub host: String,
    pub paired: String,
    pub sign_ins: Vec<SignIn>,
    pub sign_in_note: Option<String>,
    pub busy: Option<String>,
    pub error: Option<String>,
    pub log: String,
    pub can_use_mac: bool,
    /// Only short-lived browser addresses and device codes, never raw authentication output.
    pub addresses: Vec<String>,
    pub codes: Vec<String>,
    pub signing_in: bool,
}
impl Snapshot {
    pub fn selected(&self) -> Option<&Instance> {
        self.instances.iter().find(|i| i.name == self.instance)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Select { source: String, instance: String },
    Refresh,
    Build(Resources),
    Setup,
    Start,
    Stop,
    Resize { resources: Resources, stop_first: bool },
    Delete { confirmation: String },
    OpenWindow,
    UseMac,
    SignIn(String),
    SubmitCode(String),
    Cancel,
}
/// Implementations only enqueue work here; snapshot reads perform no I/O.
pub trait Capability: Send + Sync {
    fn snapshot(&self) -> Snapshot;
    fn send(&self, action: Action);
}

#[derive(Default)]
pub struct Panel {
    pub open: bool,
    source: String,
    instance: String,
    resources: Resources,
    selection: String,
    delete: bool,
    confirmation: String,
    resize: bool,
    stop_first: bool,
    code_input: String,
}

pub fn top_bar(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    let Some(vm) = backend.vm() else { return };
    let view = vm.snapshot();
    if !view.supported {
        return;
    }
    let state = view.selected().map_or("not set up", |i| i.state.label());
    let text = if view.busy.is_some() { "VM …" } else { "VM" };
    let response = ui.selectable_label(client.vm.open, text).on_hover_text(format!("VM: {state}"));
    crate::topbar::record_item(ui, "VM", &response);
    if response.clicked() {
        client.vm.open = true;
        ui.close();
    }
}

pub fn settings(ui: &mut egui::Ui, backend: &dyn Backend, client: &mut Client) {
    if backend.vm().is_some() && ui.button("VM").clicked() {
        client.vm.open = true;
        ui.close();
    }
}

pub fn window(ctx: &egui::Context, backend: &dyn Backend, client: &mut Client) {
    if !client.vm.open {
        return;
    }
    let Some(vm) = backend.vm() else { return };
    let mut open = true;
    egui::Window::new("VM settings")
        .id(egui::Id::unique("vm-settings"))
        .open(&mut open)
        .default_width(640.0)
        .default_height(650.0)
        .vscroll(true)
        .show(ctx, |ui| panel(ui, vm, &mut client.vm));
    client.vm.open = open;
}

pub fn panel(ui: &mut egui::Ui, vm: &dyn Capability, panel: &mut Panel) {
    let view = vm.snapshot();
    if !view.supported {
        ui.heading("The ATOMICS VM is macOS-only");
        ui.label("On Linux, ATOMICS and its agents run on this machine.");
        return;
    }
    let selection = format!("{}\n{}", view.source, view.instance);
    if panel.selection != selection {
        panel.source = view.source.clone();
        panel.instance = view.instance.clone();
        panel.resources = view.selected().map_or_else(|| view.defaults.clone(), |i| i.resources.clone());
        panel.selection = selection;
        panel.delete = false;
        panel.confirmation.clear();
    }
    ui.heading("ATOMICS VM");
    ui.label("Manage the VM on this Mac. Agents and their sign-ins stay in the guest.");
    let idle = view.busy.is_none();
    ui.add_enabled_ui(idle, |ui| {
        ui.horizontal(|ui| {
            ui.label("Build folder");
            ui.text_edit_singleline(&mut panel.source);
        });
        ui.horizontal(|ui| {
            ui.label("Instance");
            ui.text_edit_singleline(&mut panel.instance);
            egui::ComboBox::from_id_salt("vm-instances").selected_text("Choose…").show_ui(ui, |ui| {
                for instance in &view.instances {
                    ui.selectable_value(&mut panel.instance, instance.name.clone(), &instance.name);
                }
            });
            if ui.button("Load").clicked() {
                vm.send(Action::Select { source: panel.source.clone(), instance: panel.instance.clone() });
            }
        });
    });
    if panel.source != view.source || panel.instance != view.instance {
        ui.label("Choose Load to inspect and manage this build folder and instance.");
        return;
    }
    ui.separator();
    artifact(ui, "Image", &view.image);
    artifact(ui, "Built bundle", &view.bundle);
    if let Some(installed) = &view.installed {
        artifact(ui, "Installed bundle", installed);
    } else {
        ui.label("Installed bundle: none");
    }
    if view.outdated {
        ui.colored_label(ui.visuals().warn_fg_color, "Installed bundle is outdated. Build or update, then Set up.");
    }
    ui.add_enabled_ui(idle, |ui| {
        ui.horizontal(|ui| {
            if ui.button("Build or update image").clicked() {
                vm.send(Action::Build(panel.resources.clone()));
            }
            if ui.add_enabled(view.image.built && view.bundle.built, egui::Button::new("Set up")).clicked() {
                vm.send(Action::Setup);
            }
            if ui.button("Refresh").clicked() {
                vm.send(Action::Refresh);
            }
        });
    });
    ui.separator();
    let instance = view.selected();
    let state = instance.map_or(&State::None, |i| &i.state);
    ui.strong(format!("{}: {}", view.instance, state.label()));
    if let Some(instance) = instance {
        let r = &instance.resources;
        ui.label(format!(
            "{} CPUs · {} GiB memory · {} GiB disk{}",
            r.cpus,
            r.memory_gib,
            r.disk_gib,
            if instance.in_use { " · in use by an ATOMICS window" } else { "" }
        ));
    }
    ui.label(format!("Host in VM: {}", view.host));
    ui.label(format!("This Mac: {}", view.paired));
    ui.add_enabled_ui(idle, |ui| {
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(*state == State::Stopped, egui::Button::new("Start")).clicked() {
                vm.send(Action::Start);
            }
            if ui.add_enabled(*state == State::Running, egui::Button::new("Stop")).clicked() {
                vm.send(Action::Stop);
            }
            if ui.add_enabled(instance.is_some(), egui::Button::new("Resize…")).clicked() {
                panel.resize = !panel.resize;
            }
            if ui
                .add_enabled(
                    instance.is_some_and(|i| !i.in_use) || (instance.is_none() && view.installed.is_some()),
                    egui::Button::new("Delete…"),
                )
                .clicked()
            {
                panel.delete = !panel.delete;
            }
            if ui.add_enabled(instance.is_some(), egui::Button::new("Open VM window")).clicked() {
                vm.send(Action::OpenWindow);
            }
            if view.can_use_mac && ui.button("Use this Mac").clicked() {
                vm.send(Action::UseMac);
            }
        });
        if panel.resize || instance.is_none() {
            ui.horizontal_wrapped(|ui| {
                ui.label("CPUs");
                ui.add(egui::DragValue::new(&mut panel.resources.cpus).range(1..=1024));
                ui.label("Memory GiB");
                ui.add(egui::DragValue::new(&mut panel.resources.memory_gib).range(1..=65536));
                ui.label("Disk GiB");
                ui.add(egui::DragValue::new(&mut panel.resources.disk_gib).range(1..=1048576));
            });
            if ui.button("Defaults from this Mac").clicked() {
                panel.resources = view.defaults.clone();
                if let Some(instance) = instance {
                    panel.resources.disk_gib = panel.resources.disk_gib.max(instance.resources.disk_gib);
                }
            }
            ui.label("Disk can only grow. Resizing requires the VM to be stopped.");
            if *state == State::Running {
                ui.checkbox(&mut panel.stop_first, "Stop the VM first (ends running guest work)");
            }
            if panel.resize
                && ui
                    .add_enabled(
                        *state == State::Stopped || (*state == State::Running && panel.stop_first),
                        egui::Button::new("Apply size"),
                    )
                    .clicked()
            {
                vm.send(Action::Resize { resources: panel.resources.clone(), stop_first: panel.stop_first });
                panel.resize = false;
                panel.stop_first = false;
            }
        }
        if panel.delete {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!(
                    "Permanently delete {} and its disk, sign-ins and Mac pairing? Type the instance name:",
                    view.instance
                ),
            );
            ui.text_edit_singleline(&mut panel.confirmation);
            if ui
                .add_enabled(
                    panel.confirmation == view.instance
                        && (instance.is_some_and(|i| !i.in_use) || (instance.is_none() && view.installed.is_some())),
                    egui::Button::new("Delete permanently"),
                )
                .clicked()
            {
                vm.send(Action::Delete { confirmation: std::mem::take(&mut panel.confirmation) });
                panel.delete = false;
            }
        }
    });
    ui.separator();
    ui.strong("Sign-ins in the VM");
    if let Some(note) = &view.sign_in_note {
        ui.label(note);
    }
    for item in &view.sign_ins {
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{}  {}", item.state, item.name));
            ui.label(&item.label);
            if item.state == "missing"
                && ui.add_enabled(idle && *state == State::Running, egui::Button::new("Sign in")).clicked()
            {
                vm.send(Action::SignIn(item.name.clone()));
            }
        });
    }
    for code in &view.codes {
        ui.label(RichText::new(code).monospace().strong());
    }
    for address in &view.addresses {
        ui.horizontal_wrapped(|ui| {
            ui.label(address);
            if ui.button("Open in browser").clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(address));
            }
        });
    }
    if view.signing_in {
        ui.label(
            "If the browser gives you a one-time code to paste back, enter it here. Never enter a token or password.",
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut panel.code_input).password(true).hint_text("One-time code"));
            if ui.button("Send code").clicked() {
                vm.send(Action::SubmitCode(std::mem::take(&mut panel.code_input)));
            }
        });
    } else {
        panel.code_input.clear();
    }
    if let Some(busy) = &view.busy {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(busy);
            if ui.button("Cancel").clicked() {
                vm.send(Action::Cancel);
            }
        });
    }
    if let Some(error) = &view.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    if !view.log.is_empty() {
        ui.collapsing("Live log", |ui| {
            egui::ScrollArea::vertical().max_height(200.0).stick_to_bottom(true).show(ui, |ui| {
                ui.monospace(&view.log);
            });
        });
    }
}
fn artifact(ui: &mut egui::Ui, label: &str, artifact: &Artifact) {
    ui.label(if artifact.built {
        format!("{label}: {} · {}", artifact.version, artifact.date)
    } else {
        format!("{label}: not built")
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Fake {
        view: Snapshot,
        actions: Mutex<Vec<Action>>,
    }
    impl Capability for Fake {
        fn snapshot(&self) -> Snapshot {
            self.view.clone()
        }
        fn send(&self, action: Action) {
            self.actions.lock().unwrap().push(action);
        }
    }
    fn draw(view: Snapshot, panel_state: &mut Panel) -> String {
        let ctx = egui::Context::default();
        let fake = Fake { view, actions: Mutex::default() };
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 1800.0))),
                ..Default::default()
            },
            |ui| {
                panel(ui, &fake, panel_state);
            },
        );
        output.textures_delta.clear();
        fn text(shape: &egui::Shape, output: &mut String) {
            match shape {
                egui::Shape::Text(t) => {
                    output.push_str(t.galley.text());
                    output.push('\n');
                }
                egui::Shape::Vec(v) => {
                    for s in v {
                        text(s, output);
                    }
                }
                _ => {}
            }
        }
        let mut text_output = String::new();
        for shape in output.shapes {
            text(&shape.shape, &mut text_output);
        }
        text_output
    }
    #[test]
    fn every_instance_state_and_platform_draws() {
        let linux = draw(Snapshot::default(), &mut Panel::default());
        assert!(linux.contains("macOS-only") && !linux.contains("Build or update image"));
        for state in [State::None, State::Stopped, State::Starting, State::Running, State::Unknown("broken".into())] {
            let view = Snapshot {
                supported: true,
                instance: "test-vm".into(),
                instances: if state == State::None {
                    vec![]
                } else {
                    vec![Instance { name: "test-vm".into(), state: state.clone(), ..Default::default() }]
                },
                ..Default::default()
            };
            let text = draw(view, &mut Panel::default());
            assert!(text.contains(state.label()), "{text}");
            assert!(
                text.contains("Build or update image")
                    && text.contains("Set up")
                    && text.contains("Sign-ins in the VM"),
                "{text}"
            );
        }
    }
    #[test]
    fn build_pairing_errors_and_sign_ins_are_visible() {
        let view = Snapshot {
            supported: true,
            instance: "test-vm".into(),
            outdated: true,
            image: Artifact { built: true, version: "0.1.0 rev123".into(), date: "2026-01-01".into() },
            host: "ok".into(),
            paired: "paired".into(),
            busy: Some("Building image".into()),
            error: Some("Build failed".into()),
            sign_ins: ["ok", "missing", "skipped"]
                .into_iter()
                .map(|s| SignIn { name: format!("step-{s}"), state: s.into(), label: "Guest sign-in".into() })
                .collect(),
            codes: vec!["ABCD-EFGH".into()],
            addresses: vec!["https://example.com/device".into()],
            ..Default::default()
        };
        let text = draw(view, &mut Panel::default());
        for expected in [
            "0.1.0 rev123",
            "2026-01-01",
            "outdated",
            "paired",
            "Building image",
            "Cancel",
            "Build failed",
            "step-ok",
            "step-missing",
            "step-skipped",
            "ABCD-EFGH",
            "Open in browser",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
    }
    #[test]
    fn changing_selection_clears_the_destructive_confirmation() {
        let mut panel =
            Panel { selection: "old".into(), delete: true, confirmation: "old".into(), ..Default::default() };
        draw(Snapshot { supported: true, instance: "new".into(), ..Default::default() }, &mut panel);
        assert!(!panel.delete && panel.confirmation.is_empty());
    }
}
