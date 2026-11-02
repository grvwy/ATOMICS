import sys, pathlib
root = pathlib.Path(sys.argv[1])
def sub(path, old, new, count=1):
    p = root / path
    s = p.read_text()
    assert s.count(old) == count, (path, old, s.count(old))
    p.write_text(s.replace(old, new))
sub("crates/atomics/src/main.rs", "mod app;\n", "mod app;\nmod spike;\n")
sub("crates/atomics/src/main.rs",
    "    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup)",
    "    if std::env::var_os(\"ATOMICS_SPIKE_NOVSYNC\").is_some() {\n        native.wgpu_options.surface.present_mode = eframe::wgpu::PresentMode::AutoNoVsync;\n    }\n    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup)")
sub("crates/atomics/src/app.rs",
    "    replies: Vec<Receiver<ControlOutcome>>,\n}\n\nimpl App {",
    "    replies: Vec<Receiver<ControlOutcome>>,\n    spike: Option<crate::spike::Spike>,\n}\n\nimpl App {")
sub("crates/atomics/src/app.rs",
    "        Self { host: state.into_host(), client, replies: Vec::new() }",
    "        let spike = crate::spike::Spike::from_env(cc);\n        Self { host: state.into_host(), client, replies: Vec::new(), spike }")
sub("crates/atomics/src/app.rs",
    "    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {\n        let mut state = self.host.state();\n",
    "    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {\n        if let Some(spike) = self.spike.as_mut() {\n            spike.raw_input(raw_input);\n        }\n    }\n\n"
    "    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {\n        let mut state = self.host.state();\n"
    "        if let Some(spike) = self.spike.as_mut() {\n            let probe = spike.probe_agent.clone();\n            let terminal = probe.and_then(|name| state.sessions.iter().find(|s| s.agent == name)).and_then(|s| s.terminal.as_ref());\n            spike.frame(ctx, frame, terminal);\n        }\n")
print("patched")
