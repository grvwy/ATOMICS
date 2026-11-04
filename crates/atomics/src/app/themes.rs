//! The theme picker in the Settings menu: every built-in theme and theme file, each with a strip
//! of its own colors. A click shows the theme at once and saves `[theme] name` the way an applied
//! config change is saved: comments kept, a backup, an atomic write, then a reload.
use super::*;
use atomics_core::selfconfig;

/// Sets `[theme] name` in `file` as an applied config change is written: comments kept, a
/// backup, an atomic replace. Returns whether the file changed.
fn write_theme_name(file: &Path, name: &str, stamp: u64) -> Result<bool, String> {
    let shown = paths::display(file);
    let before = match std::fs::read_to_string(file) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(format!("reading {shown}: {err}")),
    };
    let after =
        selfconfig::with_theme_name(before.as_deref().unwrap_or(""), name).map_err(|err| format!("{shown}: {err}"))?;
    let Some(after) = after else { return Ok(false) };
    selfconfig::write(file, before.as_deref(), &after, stamp).map_err(|err| format!("{err:#}"))?;
    Ok(true)
}

impl State {
    /// The picker's themes, and whether this machine's file picks one; read when the config
    /// or a theme file changes, never per frame.
    pub(super) fn refresh_themes(&mut self) {
        let host = std::fs::read_to_string(&Config::source_paths(&self.config_path)[1]).unwrap_or_default();
        self.host_theme = selfconfig::picks_theme(&host);
    }

    /// The user picked a theme: save its name to `config.toml`, or to this machine's file, and
    /// show it at once. The user's own click, so no proposal.
    pub(super) fn select_theme(&mut self, name: &str, host: bool) {
        let [main, host_file] = Config::source_paths(&self.config_path);
        let file = if host { host_file } else { main };
        match write_theme_name(&file, name, atomics_core::store::now()) {
            Ok(changed) => {
                if changed {
                    self.reload_config();
                }
                let shown = paths::display(&file);
                self.toast(if !host && self.host_theme {
                    format!("Theme {name} saved to {shown}, but this machine's config.d file picks its own")
                } else {
                    format!("Theme {name} saved to {shown}")
                });
            }
            Err(err) => self.toast(format!("Theme not saved: {err}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::with_state;

    #[test]
    fn selecting_a_theme_keeps_comments_and_backs_up_the_file() {
        with_state("app::themes::tests::selecting_a_theme_keeps_comments_and_backs_up_the_file", |mut state, _| {
            let file = state.config_path.clone();
            let before = "# theme\n[theme]\npreset = \"midnight\" # old key\naccent = \"#00ff88\"\n";
            std::fs::write(&file, before).unwrap();
            state.reload_config();
            state.select_theme("paper", false);
            assert_eq!(state.config.theme.name, "paper");
            assert_eq!(state.theme.palette().accent, atomics_ui::theme::parse_hex("#00ff88").unwrap());
            let written = std::fs::read_to_string(&file).unwrap();
            assert!(written.starts_with("# theme") && written.contains("name = \"paper\""), "{written}");
            assert!(!written.contains("preset"));
            let backups: Vec<_> = std::fs::read_dir(file.parent().unwrap())
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|ext| ext == "bak"))
                .collect();
            assert_eq!(backups.len(), 1);
            assert_eq!(std::fs::read_to_string(backups[0].path()).unwrap(), before);
            std::fs::write(&file, "[theme\nname = 'claude'").unwrap();
            state.select_theme("midnight", false);
            assert_eq!(std::fs::read_to_string(file).unwrap(), "[theme\nname = 'claude'");
        });
    }
}
