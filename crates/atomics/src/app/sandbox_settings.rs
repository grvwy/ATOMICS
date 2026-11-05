//! Sandbox readiness is checked on demand, off the UI thread, using doctor's checks.
use super::*;

impl State {
    pub(super) fn check_sandbox(&mut self) {
        if self.sandbox_check.is_some() {
            return;
        }
        let program = resolve_program(&self.sbx);
        let config = match crate::sandbox::checks::configured(&self.config, self.workspace.as_deref()) {
            Ok(config) => config,
            Err(_) => {
                self.sandbox_lines = vec!["✗ Cannot read sandbox/ticket configuration; fix it and refresh".into()];
                return;
            }
        };
        let (main, workspace) = (self.config.clone(), self.workspace.clone());
        let keychain = self.relay.keychain().clone();
        let machine = self.sandbox_machine(config.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.sandbox_check = Some(rx);
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            let claude = crate::sandbox::checks::claude_route(&main, workspace.as_deref(), &keychain);
            let lines = crate::sandbox::doctor_lines(&program, &config, claude);
            let _ = tx.send((lines, crate::sandbox::steps::status(&machine)));
            wake.notify();
        });
    }

    pub(super) fn poll_sandbox_check(&mut self) -> bool {
        let Some(rx) = &self.sandbox_check else {
            return false;
        };
        match rx.try_recv() {
            Ok((lines, steps)) => {
                self.sandbox_lines = lines;
                self.sandbox_steps = steps;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.sandbox_lines = vec!["✗ Sandbox check stopped; refresh to retry".into()]
            }
        }
        self.sandbox_check = None;
        true
    }
}
