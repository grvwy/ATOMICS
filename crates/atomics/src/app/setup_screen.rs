//! The host's side of the setup screen (`atomics_ui::management::setup`): what it shows, and what its clicks do. Also
//! the question a person's start asks when no sandbox can be had for it: ATOMICS never runs a sandboxed agent bare on
//! its own, so it asks instead of failing (docs/isolation.md).
use super::*;
use atomics_core::config::AgentKind;
use atomics_core::isolation::IsolationMode;
use atomics_core::projection::{SetupConnection, SetupInfo, SetupProgram, StartChoice};

/// A person's start waiting for their answer.
pub(super) struct PendingStart {
    pub(super) agent: String,
    pub(super) cwd: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) prompt: Option<String>,
    pub(super) why: String,
    pub(super) sandbox_required: bool,
}

/// The programs a fresh machine needs: git, then the agents the config runs. A program whose name a person would not
/// recognize keeps it, so a custom agent shows as what it runs.
fn wanted_programs(config: &Config) -> Vec<(String, Option<String>, String)> {
    let mut wanted = vec![("git".to_string(), None, "git".to_string())];
    for agent in &config.agents {
        let Some(program) = agent.command.first() else { continue };
        let name = match agent.kind {
            AgentKind::Shell => continue,
            AgentKind::Claude => "Claude Code".to_string(),
            AgentKind::Generic if agent.is_codex() => "Codex".to_string(),
            AgentKind::Generic => agent.name.clone(),
        };
        wanted.push((name, Some(agent.name.clone()), program.clone()));
    }
    wanted
}

impl State {
    pub(super) fn setup_info(&self) -> SetupInfo {
        SetupInfo {
            first_run: self.store.setup_pending,
            checking: self.programs_check.is_some(),
            programs: self.setup_programs.clone(),
            project: self.open_projects.first().map(|project| paths::display(project)),
            connections: self.setup_connections(),
            start_choice: self.start_choice.as_ref().map(|pending| StartChoice {
                agent: pending.agent.clone(),
                why: pending.why.clone(),
                sandbox_required: pending.sandbox_required,
            }),
        }
    }

    fn setup_connections(&self) -> Vec<SetupConnection> {
        use atomics_core::github::AZURE_HOST;
        let row =
            |id: &str, name: &str, connected: Option<String>, note: &str, command: Option<&str>| SetupConnection {
                id: id.into(),
                name: name.into(),
                connected,
                note: note.into(),
                command: command.map(str::to_string),
            };
        let github = self.github.signed_in_hosts().into_iter().find(|(host, _)| host != AZURE_HOST);
        let azure = self.github.viewer(AZURE_HOST).map(str::to_string);
        let tickets = self.ticket_config();
        let tickets_connected =
            tickets.as_ref().filter(|_| !self.tickets.panel.signin).map(|config| config.system.clone());
        let tickets_note = match &tickets {
            Some(_) => "Sign in to read tickets and plan from them.",
            None => "Follows a ticket system once the config has a [tickets] profile for it.",
        };
        let devices = self.remote_devices().map_or(0, |devices| devices.list().len());
        vec![
            row("github", "GitHub", github.map(|(_, login)| login), "See, review and merge pull requests here.", None),
            row(
                "azure",
                "Azure DevOps",
                azure,
                "Uses the Azure CLI's sign-in. Nothing is stored, and no agent gets it.",
                Some("az login"),
            ),
            row("tickets", "Tickets", tickets_connected, tickets_note, None),
            row(
                "remote",
                "Remote devices",
                (devices > 0).then(|| format!("{devices} paired")),
                "Follow and steer agents from a phone or another computer.",
                None,
            ),
        ]
    }

    /// Looks for the programs off the window's thread: reading the login shell can take seconds.
    pub(super) fn check_programs(&mut self) {
        if self.programs_check.is_some() {
            return;
        }
        let wanted = wanted_programs(&self.config);
        let (tx, rx) = std::sync::mpsc::channel();
        self.programs_check = Some(rx);
        let wake = self.wake.clone();
        std::thread::spawn(move || {
            // What was installed since ATOMICS started counts, and agents started from now on find it too.
            let path = crate::setup::refresh_login_path();
            let mac = cfg!(target_os = "macos");
            let programs = wanted
                .into_iter()
                .map(|(name, agent, program)| {
                    let found = if program.contains('/') {
                        Path::new(&program).is_file().then(|| PathBuf::from(&program))
                    } else {
                        crate::setup::find_on_path(&program, &path)
                    };
                    let install = crate::sandbox::checks::install_command(&program, mac);
                    SetupProgram { name, agent, found: found.as_deref().map(paths::display), install }
                })
                .collect();
            let _ = tx.send(programs);
            wake.notify();
        });
        // Azure DevOps counts as signed in once a poll has seen its login.
        self.github_poll_now();
    }

    pub(super) fn poll_programs_check(&mut self) -> bool {
        let Some(rx) = &self.programs_check else { return false };
        match rx.try_recv() {
            Ok(programs) => self.setup_programs = programs,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
        self.programs_check = None;
        true
    }

    pub(super) fn finish_setup(&mut self) -> Result<(), String> {
        if std::mem::take(&mut self.store.setup_pending) {
            self.store.save(&paths::store_file()).map_err(|err| format!("Could not save that: {err:#}"))?;
        }
        Ok(())
    }

    /// In this machine's own file: whether sandboxes can run here says nothing of another machine sharing the main one.
    pub(super) fn set_isolation(&mut self, mode: IsolationMode) -> Result<(), String> {
        let [_, file] = Config::source_paths(&self.config_path);
        let unsaved = |why: String| format!("Could not save where agents run: {why}");
        let before = match std::fs::read_to_string(&file) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(unsaved(err.to_string())),
        };
        let after = atomics_core::selfconfig::edit(
            before.as_deref().unwrap_or(""),
            &format!("[isolation]\nmode = \"{}\"", mode.name()),
            &[],
        )
        .map_err(unsaved)?;
        if let Some(folder) = file.parent() {
            std::fs::create_dir_all(folder).map_err(|err| unsaved(err.to_string()))?;
        }
        atomics_core::selfconfig::write(&file, before.as_deref(), &after, atomics_core::store::now())
            .map_err(|err| unsaved(format!("{err:#}")))?;
        self.reload_config();
        Ok(())
    }

    /// A folder nobody has made a repository yet, so that a first project needs no terminal.
    pub(super) fn init_project(&mut self, path: &str) -> Result<(), String> {
        let folder = paths::expand(path);
        if !folder.is_dir() {
            return Err(format!("{} is not a folder", folder.display()));
        }
        if workspace::project_root(&folder).is_err() {
            git::git(&folder, &["init", "-q"]).map_err(|err| format!("Could not start a repository: {err:#}"))?;
        }
        let root = workspace::project_root(&folder).map_err(|err| format!("{err:#}"))?;
        self.add_project(&root);
        Ok(())
    }

    /// Holds a person's start whose sandbox could not be had until they answer, and takes its tile away.
    pub(super) fn ask_about_start(&mut self, launch: AgentLaunch, why: String) {
        self.sessions.retain(|session| session.id != launch.id);
        if self.active.as_deref() == Some(launch.id.as_str()) {
            self.active = None;
        }
        // A start nobody answered stands in for this one; its scratch folder goes the way it would have.
        if let Some(earlier) = self.start_choice.take() {
            remove_scratch(&earlier.cwd);
        }
        let sandbox_required = self.current_project_config(&launch.cwd).isolation.required;
        self.start_choice = Some(PendingStart {
            agent: launch.agent.name,
            cwd: launch.cwd,
            args: launch.extra_args,
            prompt: launch.prompt,
            why: crate::sandbox::without_opt_out(&why),
            sandbox_required,
        });
    }

    /// `run_here` runs it on this machine, which this machine then keeps doing; anything else leaves it.
    pub(super) fn answer_start(&mut self, run_here: bool) -> Result<(), String> {
        let Some(pending) = self.start_choice.take() else { return Ok(()) };
        if !run_here || pending.sandbox_required {
            remove_scratch(&pending.cwd);
            return Ok(());
        }
        self.set_isolation(IsolationMode::Bare)?;
        let selection = self.active.clone();
        let id = self.start_as(
            &pending.agent,
            pending.cwd,
            None,
            pending.args,
            pending.prompt,
            Role::Agent(Some(IsolationMode::Bare)),
        )?;
        // Whoever else was on screen stays there.
        if selection.is_some() {
            self.active = selection;
        } else {
            self.active = Some(id);
        }
        Ok(())
    }
}

fn remove_scratch(folder: &Path) {
    if paths::scratch_of(folder).is_some() {
        let _ = std::fs::remove_dir_all(folder);
    }
}
