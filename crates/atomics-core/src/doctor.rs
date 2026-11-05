//! Diagnostics shared by the doctor command and its tests.

use std::path::Path;

use crate::approvals::ApprovalPolicy;
use crate::hook::CodexHookTrust;
#[cfg(feature = "native")]
use crate::{git, plan::Plan};

pub fn codex_version_check(supports_hooks: Option<bool>) -> String {
    match supports_hooks {
        Some(supported) => check_line(
            supported,
            "Codex supports ATOMICS hooks (0.165+ required)",
            "install Codex 0.165 or newer; check with `codex --version`",
        ),
        None => check_line(
            false,
            "could not determine the Codex version (warning)",
            "check that `codex --version` completes successfully and try again",
        ),
    }
}

/// How Codex treats one of ATOMICS's hooks, from its `[hooks.state]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trust {
    Trusted,
    Untrusted,
    /// Trusted once, for a definition that has changed since: a new install path, socket or handler.
    Changed,
}

/// One ATOMICS install's hooks as Codex identifies them for trust ([`crate::hook::codex_hook_trust`]),
/// with the executable they name, as the doctor describes it.
pub struct InstallHooks {
    /// The executable, and why the doctor looks at it: "/Applications/ATOMICS.app/… (the desktop's)".
    pub binary: String,
    pub hooks: Vec<CodexHookTrust>,
}

/// Whether Codex runs the hooks ATOMICS passes it, from `[hooks.state]` in `text`, Codex's user
/// config read from `config`. Codex runs a hook whose `trusted_hash` matches its current hash,
/// unless `enabled = false`; a missing file trusts nothing.
///
/// Every hook names the executable that runs it, so each install has hooks of its own, but Codex
/// keys its trust by event: it trusts one install's hooks at a time. The check is for the first of
/// `installs`, the one whose hooks bare Codex agents get, and its fix trusts those; each other
/// install found follows as a fact, saying whether Codex trusts its hooks instead.
pub fn codex_trust_lines(installs: &[InstallHooks], config: &Path, text: std::io::Result<String>) -> Vec<String> {
    let Some((checked, others)) = installs.split_first() else { return Vec::new() };
    let states = match hook_states(config, text) {
        Ok(states) => states,
        Err(line) => return vec![line],
    };
    let mut lines = vec![codex_trust_check(checked, &states, config)];
    for other in others {
        let trusted: Vec<&str> = other
            .hooks
            .iter()
            .filter(|hook| states.get(&hook.key).and_then(|state| state.trusted_hash.as_deref()) == Some(&hook.hash))
            .map(|hook| hook.event.as_str())
            .collect();
        let binary = &other.binary;
        lines.push(if trusted.is_empty() {
            format!(
                "Other ATOMICS install {binary}: its hooks are not trusted. A desktop or host started from it asks \
                 Codex to review them again, and trusting them there stops Codex running those of the install \
                 checked above: Codex trusts one install's hooks at a time"
            )
        } else if trusted.len() == other.hooks.len() {
            format!(
                "Other ATOMICS install {binary}: Codex trusts its hooks, rather than those of the install checked above"
            )
        } else {
            format!(
                "Other ATOMICS install {binary}: Codex trusts its hooks for {}, rather than those of the install \
                 checked above",
                trusted.join(", ")
            )
        });
    }
    lines
}

/// What Codex keeps of a hook's trust.
#[derive(serde::Deserialize)]
struct State {
    trusted_hash: Option<String>,
    enabled: Option<bool>,
}

/// `[hooks.state]` of Codex's user config, by trimmed key as Codex reads it, or the check line
/// that says why trust is unknown.
fn hook_states(
    config: &Path,
    text: std::io::Result<String>,
) -> Result<std::collections::HashMap<String, State>, String> {
    let text = match text {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(check_line(
                false,
                &format!("cannot read Codex's config {} for ATOMICS hook trust (warning): {err}", config.display()),
                "fix the file's read permissions",
            ));
        }
    };
    let table = match text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(err) => {
            return Err(check_line(
                false,
                &format!(
                    "Codex's config {} does not parse; ATOMICS hook trust unknown (warning): {err}",
                    config.display()
                ),
                "fix the TOML; Codex refuses to start with it too",
            ));
        }
    };
    // Codex trims the keys and skips a state it cannot read.
    Ok(table
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter_map(|(key, state)| Some((key.trim().to_string(), state.clone().try_into().ok()?)))
        .collect())
}

/// Whether Codex runs every hook of `install`, and if not, how to make it.
fn codex_trust_check(
    install: &InstallHooks,
    states: &std::collections::HashMap<String, State>,
    config: &Path,
) -> String {
    let binary = &install.binary;
    let mut found: Vec<(&CodexHookTrust, Trust, bool)> = Vec::new();
    for hook in &install.hooks {
        let state = states.get(&hook.key);
        let trust = match state.and_then(|state| state.trusted_hash.as_deref()) {
            Some(hash) if hash == hook.hash => Trust::Trusted,
            Some(_) => Trust::Changed,
            None => Trust::Untrusted,
        };
        found.push((hook, trust, state.and_then(|state| state.enabled) != Some(false)));
    }
    if found.iter().all(|(_, trust, enabled)| *trust == Trust::Trusted && *enabled) {
        return check_line(
            true,
            &format!(
                "Codex trusts ATOMICS's hooks for all {} events, as {binary} passes them ({})",
                found.len(),
                config.display()
            ),
            "",
        );
    }
    let events = |wanted: &dyn Fn(Trust, bool) -> bool| -> Vec<&CodexHookTrust> {
        found.iter().filter(|(_, trust, enabled)| wanted(*trust, *enabled)).map(|(hook, _, _)| *hook).collect()
    };
    let names = |hooks: &[&CodexHookTrust]| hooks.iter().map(|hook| hook.event.as_str()).collect::<Vec<_>>().join(", ");
    let review = events(&|trust, _| trust != Trust::Trusted);
    let disabled = events(&|_, enabled| !enabled);
    let mut parts = Vec::new();
    for (name, trust) in
        [("untrusted", Trust::Untrusted), ("changed since trusted", Trust::Changed), ("trusted", Trust::Trusted)]
    {
        let hooks = events(&|other, _| other == trust);
        if !hooks.is_empty() {
            parts.push(format!("{name}: {}", names(&hooks)));
        }
    }
    if !disabled.is_empty() {
        parts.push(format!("disabled: {}", names(&disabled)));
    }
    let label = format!(
        "Codex does not run every ATOMICS hook as {binary} passes them (warning): {} ({})",
        parts.join("; "),
        config.display()
    );
    let mut fixes = Vec::new();
    if !review.is_empty() {
        let entries: Vec<_> = review
            .iter()
            .map(|hook| format!("[hooks.state.\"{}\"] trusted_hash = \"{}\"", hook.key, hook.hash))
            .collect();
        fixes.push(format!(
            "start a Codex agent in ATOMICS and trust {} at its \"Hooks need review\" prompt or in /hooks \
             (repository hooks keep their own review), or set in {}: {}",
            names(&review),
            config.display(),
            entries.join("; ")
        ));
    }
    if !disabled.is_empty() {
        let tables: Vec<_> = disabled.iter().map(|hook| format!("[hooks.state.\"{}\"]", hook.key)).collect();
        fixes.push(format!(
            "enable {} in /hooks, or remove enabled = false from {} in {}",
            names(&disabled),
            tables.join(", "),
            config.display()
        ));
    }
    check_line(false, &label, &fixes.join("; also "))
}

pub fn approvals_check(policy: &ApprovalPolicy) -> String {
    let cargo: Vec<_> = policy
        .allow
        .iter()
        .filter(|entry| {
            shlex::split(entry)
                .and_then(|words| words.into_iter().next())
                .is_some_and(|program| Path::new(&program).file_name().is_some_and(|name| name == "cargo"))
        })
        .map(String::as_str)
        .collect();
    let label = if cargo.is_empty() {
        format!("[approvals] allow list: {} entries, no cargo entries", policy.allow.len())
    } else {
        format!("[approvals] allow list contains cargo entries (warning): {}", cargo.join(", "))
    };
    check_line(
        cargo.is_empty(),
        &label,
        "remove cargo entries from [approvals].allow unless intended; approved commands run outside the sandbox",
    )
}

#[cfg(feature = "native")]
pub fn plans_check(home: &Path) -> Vec<String> {
    let dir = home.join("plans");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return vec![check_line(true, &format!("no plans yet ({})", dir.display()), "")];
        }
        Err(err) => {
            return vec![check_line(
                false,
                &format!("cannot read plans directory {}: {err}", dir.display()),
                "fix the plans directory's path and read permissions",
            )];
        }
    };
    let mut errors = Vec::new();
    for entry in entries {
        let result = entry.map_err(anyhow::Error::from).and_then(|entry| {
            let path = entry.path();
            if !crate::plan::is_plan_file(&path) {
                return Ok(());
            }
            Plan::load(&path).map(|_| ()).map_err(|err| anyhow::anyhow!("{}: {err:#}", path.display()))
        });
        if let Err(err) = result {
            errors.push(check_line(
                false,
                &format!("unreadable plan (warning): {err:#}"),
                "fix this file's read permissions or JSON; restore it from a backup if damaged",
            ));
        }
    }
    errors.sort();
    if errors.is_empty() {
        errors.push(check_line(true, &format!("plan files readable ({})", dir.display()), ""));
    }
    errors
}

/// Probe the actual source/destination filesystems without keeping cache copies.
#[cfg(feature = "native")]
pub fn cache_clone_check(repo: &Path, template: &str) -> String {
    let result = (|| -> anyhow::Result<_> {
        let main = git::main_checkout(repo)?;
        let destination = git::worktree_path(template, &main, "atomics-cow-probe");
        // Custom templates may have parents that do not exist until first launch.
        let parent = destination.parent().and_then(|path| path.ancestors().find(|p| p.is_dir()));
        let parent = parent.ok_or_else(|| anyhow::anyhow!("no destination folder"))?;
        git::probe_cache_clone(&main, parent)
    })();
    check_line(
        result.unwrap_or(false),
        &format!("CoW cache cloning for {}", repo.display()),
        "cache warming will be skipped; use worktrees on the same CoW filesystem as the main checkout",
    )
}

#[cfg(feature = "native")]
pub fn worktree_checks(repo: &Path, base_ref: &str) -> Vec<String> {
    match git::worktree_health(repo, base_ref) {
        Ok(health) => {
            let candidates: Vec<_> = health.iter().filter(|tree| tree.merged && tree.is_safe_to_remove()).collect();
            if candidates.is_empty() {
                return vec![check_line(true, &format!("no merged worktrees to clean up ({})", repo.display()), "")];
            }
            candidates
                .iter()
                .map(|tree| {
                    check_line(
                        false,
                        &format!(
                            "merged worktree can be cleaned up (warning): {} ({})",
                            tree.path.display(),
                            tree.branch.as_deref().unwrap_or("detached")
                        ),
                        "review it in ATOMICS's Clean up… dialog before removing the worktree",
                    )
                })
                .collect()
        }
        Err(err) => vec![check_line(
            false,
            &format!("cannot check worktrees for {}: {err:#}", repo.display()),
            "check repository access and the configured base_ref",
        )],
    }
}

/// The most stash entries the doctor lists for a repository.
#[cfg(feature = "native")]
const STASH_SHOWN: usize = 20;

/// The stash `repo`'s worktrees share, one fact line per entry with its message, newest first.
/// Agents stash with their task id as the message, so an entry names whose it is.
#[cfg(feature = "native")]
pub fn stash_lines(repo: &Path) -> Vec<String> {
    let entries = match crate::stash::list(repo) {
        Ok(entries) => entries,
        Err(err) => {
            return vec![check_line(
                false,
                &format!("cannot list the stash of {} (warning): {err:#}", repo.display()),
                "check repository access",
            )];
        }
    };
    if entries.is_empty() {
        return vec![format!("Stash of {}: empty", repo.display())];
    }
    let count = crate::stash::entries(entries.len());
    let mut lines = vec![format!("Stash of {}, shared by its worktrees: {count}", repo.display())];
    lines.extend(
        entries.iter().take(STASH_SHOWN).map(|entry| format!("  stash@{{{}}}: {}", entry.index, entry.subject)),
    );
    if entries.len() > STASH_SHOWN {
        lines.push(format!("  … and {} more: git stash list", entries.len() - STASH_SHOWN));
    }
    lines
}

pub fn check_line(passed: bool, label: &str, hint: &str) -> String {
    let line = if passed { format!("✓ {label}") } else { format!("✗ {label} — {hint}") };
    line.lines().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(all(feature = "native", any(target_os = "macos", target_os = "linux")))]
    fn cache_clone_diagnostic_probes_repo_folder_and_cleans_up() {
        let (dir, repo) = crate::git::tests::project();
        let template = "{repo_parent}/{repo}-{slug}";
        let before = std::fs::read_dir(dir.path()).unwrap().count();
        let supported = git::probe_cache_clone(&repo, dir.path()).unwrap();
        assert!(cache_clone_check(&repo, template).starts_with(if supported { '✓' } else { '✗' }));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), before);
        assert!(
            !std::fs::read_dir(&repo).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".atomics-cow-"))
        );
        assert!(cache_clone_check(&dir.path().join("missing"), template).starts_with('✗'));
    }

    #[test]
    fn codex_version_passes_or_warns() {
        for supported in [true, false] {
            let version = codex_version_check(Some(supported));
            assert_eq!(version.starts_with('✓'), supported);
            assert_eq!(version.contains("install Codex 0.165"), !supported);
            assert_eq!(version.lines().count(), 1);
        }
        let version = codex_version_check(None);
        assert!(version.starts_with('✗'));
        assert!(version.contains("could not determine the Codex version"));
        assert!(!version.contains("install Codex"));
    }

    #[test]
    #[cfg(feature = "native")]
    fn codex_trust_reads_a_codex_home_and_names_each_event_and_its_fix() {
        let hooks = crate::hook::codex_hook_trust(Path::new("/usr/local/bin/atomics"), Path::new("/run/atomics.sock"));
        let hook = |event: &str| hooks.iter().find(|hook| hook.event == event).unwrap();
        let codex_home = crate::test_support::tempdir().unwrap();
        let config = codex_home.path().join("config.toml");
        let install = || vec![InstallHooks { binary: "/usr/local/bin/atomics".into(), hooks: hooks.clone() }];
        let check = || codex_trust_lines(&install(), &config, std::fs::read_to_string(&config)).remove(0);
        let entry =
            |hook: &CodexHookTrust, hash: &str| format!("[hooks.state.\"{}\"]\ntrusted_hash = \"{hash}\"\n", hook.key);

        // A Codex home without a config trusts nothing yet.
        let line = check();
        assert!(
            line.starts_with(
                "✗ Codex does not run every ATOMICS hook as /usr/local/bin/atomics passes them (warning): untrusted: "
            ),
            "{line}"
        );
        assert!(!line.contains("changed") && !line.contains("disabled") && !line.contains("; trusted:"), "{line}");
        assert!(line.contains(&config.display().to_string()) && line.contains("/hooks"), "{line}");
        assert!(line.contains("repository hooks keep their own review"), "{line}");
        for hook in &hooks {
            let fix = format!("[hooks.state.\"{}\"] trusted_hash = \"{}\"", hook.key, hook.hash);
            assert!(line.contains(&fix), "{line}");
        }
        assert_eq!(line.lines().count(), 1);

        // Trusted as Codex's review writes it, with unrelated state and padded keys left alone.
        let mut all: String = hooks.iter().map(|hook| entry(hook, &hook.hash)).collect();
        all.push_str("[hooks.state.\"/home/user/.codex/hooks.json:stop:0:0\"]\ntrusted_hash = \"sha256:other\"\n");
        std::fs::write(&config, &all).unwrap();
        let line = check();
        assert_eq!(
            line,
            format!(
                "✓ Codex trusts ATOMICS's hooks for all {} events, as /usr/local/bin/atomics passes them ({})",
                hooks.len(),
                config.display()
            )
        );
        std::fs::write(
            &config,
            all.replace(&format!("\"{}\"", hook("Stop").key), &format!("\"  {}  \"", hook("Stop").key)),
        )
        .unwrap();
        assert!(check().starts_with('✓'), "{}", check());

        // A definition trusted before it changed, one disabled and one never trusted.
        let mut mixed: String = hooks
            .iter()
            .filter(|hook| !["SessionStart", "Stop"].contains(&hook.event.as_str()))
            .map(|hook| entry(hook, &hook.hash))
            .collect();
        mixed.push_str(&entry(hook("SessionStart"), "sha256:from-an-older-install"));
        mixed = mixed.replace(
            &entry(hook("Interrupt"), &hook("Interrupt").hash),
            &format!("{}enabled = false\n", entry(hook("Interrupt"), &hook("Interrupt").hash)),
        );
        std::fs::write(&config, mixed).unwrap();
        let line = check();
        let (label, fix) = line.split_once(" — ").unwrap();
        assert!(label.starts_with('✗') && label.contains("(warning)"), "{label}");
        assert!(
            label.contains("untrusted: Stop;") && label.contains("changed since trusted: SessionStart;"),
            "{label}"
        );
        assert!(label.contains("trusted: ") && label.contains("disabled: Interrupt"), "{label}");
        for (event, named) in [("Stop", true), ("SessionStart", true), ("Interrupt", false), ("PreToolUse", false)] {
            let fix_entry = format!("[hooks.state.\"{}\"] trusted_hash = \"{}\"", hook(event).key, hook(event).hash);
            assert_eq!(fix.contains(&fix_entry), named, "{event}: {fix}");
        }
        assert!(fix.contains("trust SessionStart, Stop") || fix.contains("trust Stop, SessionStart"), "{fix}");
        assert!(
            fix.contains(&format!(
                "enable Interrupt in /hooks, or remove enabled = false from [hooks.state.\"{}\"]",
                hook("Interrupt").key
            )),
            "{fix}"
        );

        // A config Codex cannot read leaves trust unknown rather than claiming it.
        std::fs::write(&config, "[hooks.state\n").unwrap();
        let line = check();
        assert!(line.starts_with('✗') && line.contains("does not parse") && line.contains("trust unknown"), "{line}");
        assert_eq!(line.lines().count(), 1);
        let denied = codex_trust_lines(&install(), &config, Err(std::io::ErrorKind::PermissionDenied.into())).remove(0);
        assert!(denied.starts_with('✗') && denied.contains("cannot read Codex's config"), "{denied}");
    }

    #[test]
    #[cfg(feature = "native")]
    fn codex_trust_is_checked_for_the_install_that_passes_the_hooks_and_others_are_named() {
        // The app and a cargo-installed CLI: the same hook keys, hashes of their own executables.
        let socket = Path::new("/run/atomics.sock");
        let (app, cli) = ("/Applications/ATOMICS.app/Contents/MacOS/atomics", "/home/user/.cargo/bin/atomics");
        let hooks_of = |exe: &str| crate::hook::codex_hook_trust(Path::new(exe), socket);
        assert_eq!(
            hooks_of(app).iter().map(|hook| &hook.key).collect::<Vec<_>>(),
            hooks_of(cli).iter().map(|hook| &hook.key).collect::<Vec<_>>()
        );
        assert!(hooks_of(app).iter().zip(hooks_of(cli)).all(|(app, cli)| app.hash != cli.hash));
        let installs = || {
            vec![
                InstallHooks { binary: format!("{app} (the desktop's)"), hooks: hooks_of(app) },
                InstallHooks { binary: format!("{cli} (this doctor)"), hooks: hooks_of(cli) },
            ]
        };
        let codex_home = crate::test_support::tempdir().unwrap();
        let config = codex_home.path().join("config.toml");
        let trust = |exe: &str| -> String {
            hooks_of(exe)
                .iter()
                .map(|hook| format!("[hooks.state.\"{}\"]\ntrusted_hash = \"{}\"\n", hook.key, hook.hash))
                .collect()
        };
        let fix = |exe: &str| -> Vec<String> {
            hooks_of(exe).iter().map(|hook| format!("trusted_hash = \"{}\"", hook.hash)).collect()
        };

        // The app's hooks trusted, as its review wrote them: a doctor run from the CLI says so, and
        // suggests nothing that would trust the CLI's instead.
        std::fs::write(&config, trust(app)).unwrap();
        let lines = codex_trust_lines(&installs(), &config, std::fs::read_to_string(&config));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[0],
            format!(
                "✓ Codex trusts ATOMICS's hooks for all 8 events, as {app} (the desktop's) passes them ({})",
                config.display()
            )
        );
        assert!(!lines[1].starts_with(['✓', '✗']), "a fact, not a failed check: {}", lines[1]);
        assert!(lines[1].starts_with(&format!("Other ATOMICS install {cli} (this doctor): its hooks are not trusted")));
        assert!(lines[1].contains("one install's hooks at a time"), "{}", lines[1]);
        for entry in fix(cli) {
            assert!(!lines.concat().contains(&entry), "{lines:?}");
        }

        // The CLI's trusted instead: the app's are changed since trusted, its fix trusts the app's
        // again, and the CLI's line says why.
        std::fs::write(&config, trust(cli)).unwrap();
        let lines = codex_trust_lines(&installs(), &config, std::fs::read_to_string(&config));
        assert!(
            lines[0].starts_with(&format!(
                "✗ Codex does not run every ATOMICS hook as {app} (the desktop's) passes them (warning): changed since \
                 trusted: "
            )),
            "{}",
            lines[0]
        );
        assert!(fix(app).iter().all(|entry| lines[0].contains(entry)), "{}", lines[0]);
        assert!(fix(cli).iter().all(|entry| !lines[0].contains(entry)), "{}", lines[0]);
        assert_eq!(
            lines[1],
            format!(
                "Other ATOMICS install {cli} (this doctor): Codex trusts its hooks, rather than those of the install checked above"
            )
        );

        // A config Codex cannot read says so once, for every install.
        std::fs::write(&config, "[hooks.state\n").unwrap();
        let lines = codex_trust_lines(&installs(), &config, std::fs::read_to_string(&config));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("does not parse"), "{lines:?}");
    }

    #[test]
    fn approval_list_warns_only_for_cargo_executables() {
        assert!(approvals_check(&ApprovalPolicy::default()).starts_with('✓'));
        for entry in ["cargo", "cargo test", " cargo clippy ", "'/tools with spaces/cargo' build"] {
            let line = approvals_check(&ApprovalPolicy { allow: vec![entry.into()], ..ApprovalPolicy::default() });
            assert!(line.starts_with('✗'), "{entry}");
            assert!(line.contains("outside the sandbox"));
            assert!(line.contains("remove cargo entries"));
            assert_eq!(line.lines().count(), 1);
        }
        for entry in ["cargo-nextest", "echo cargo", "rg cargo"] {
            assert!(
                approvals_check(&ApprovalPolicy { allow: vec![entry.into()], ..ApprovalPolicy::default() })
                    .starts_with('✓')
            );
        }
    }

    #[test]
    #[cfg(feature = "native")]
    fn plan_checks_are_read_only_and_report_bad_json_and_unreadable_files() {
        use std::os::unix::fs::PermissionsExt;

        let home = crate::test_support::tempdir().unwrap();
        assert!(plans_check(home.path())[0].starts_with('✓'));
        assert!(!home.path().join("plans").exists());
        let dir = home.path().join("plans");
        std::fs::create_dir(&dir).unwrap();
        Plan::default().save(&dir.join("good.json")).unwrap();
        std::fs::write(home.path().join("plan.json"), "legacy plan must not be migrated").unwrap();
        assert!(plans_check(home.path())[0].starts_with('✓'));

        std::fs::write(dir.join("bad.json"), "not JSON\n").unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join("unreadable.json")).unwrap();
        Plan::default().save(&dir.join("denied.json")).unwrap();
        std::fs::set_permissions(dir.join("denied.json"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let lines = plans_check(home.path());
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|line| line.contains("bad.json")));
        assert!(lines.iter().any(|line| line.contains("unreadable.json")));
        assert!(lines.iter().any(|line| line.contains("denied.json")));
        assert!(
            lines
                .iter()
                .all(|line| line.starts_with('✗') && line.contains("read permissions") && line.lines().count() == 1)
        );
        assert!(home.path().join("plan.json").exists());
        assert!(!home.path().join("plan.json.migrated").exists());
        assert_eq!(std::fs::read_to_string(dir.join("bad.json")).unwrap(), "not JSON\n");
    }

    #[test]
    #[cfg(feature = "native")]
    fn inaccessible_plans_directory_warns() {
        let home = crate::test_support::tempdir().unwrap();
        std::fs::write(home.path().join("plans"), "not a directory").unwrap();
        let lines = plans_check(home.path());
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with('✗'));
        assert!(lines[0].contains("cannot read plans directory"));
        assert_eq!(lines[0].lines().count(), 1);
    }

    #[test]
    #[cfg(feature = "native")]
    fn only_merged_clean_worktrees_are_reported_for_cleanup() {
        let (dir, repo) = crate::git::tests::project();
        assert!(worktree_checks(&repo, "main")[0].starts_with('✓'));
        let done = dir.path().join("done");
        let dirty = dir.path().join("dirty");
        let unmerged = dir.path().join("unmerged");
        for (path, branch) in [(&done, "done"), (&dirty, "dirty"), (&unmerged, "unmerged")] {
            git::add_worktree(&repo, path, branch, "main").unwrap();
        }
        std::fs::write(dirty.join("wip"), "keep me").unwrap();
        git::git(
            &unmerged,
            &["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "--allow-empty", "-m", "unmerged"],
        )
        .unwrap();
        let lines = worktree_checks(&repo, "main");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with('✗'));
        assert!(lines[0].contains(&done.display().to_string()));
        assert!(lines[0].contains("Clean up…"));
        assert_eq!(lines[0].lines().count(), 1);
        assert!(done.exists() && dirty.exists() && unmerged.exists());
    }

    #[test]
    #[cfg(feature = "native")]
    fn stash_lines_list_every_worktrees_entries_with_their_messages() {
        let (dir, repo) = crate::git::tests::project();
        assert_eq!(stash_lines(&repo), [format!("Stash of {}: empty", repo.display())]);
        let linked = dir.path().join("linked");
        git::add_worktree(&repo, &linked, "atomics/t2", "main").unwrap();
        std::fs::write(repo.join("a.txt"), "main change\n").unwrap();
        git::git(&repo, &["stash", "push", "-q", "-m", "t1"]).unwrap();
        std::fs::write(linked.join("a.txt"), "linked change\n").unwrap();
        git::git(&linked, &["stash", "push", "-q", "-m", "ENG-123 before rebase"]).unwrap();
        let expected = [
            format!("Stash of {}, shared by its worktrees: 2 entries", repo.display()),
            "  stash@{0}: On atomics/t2: ENG-123 before rebase".to_string(),
            "  stash@{1}: On main: t1".to_string(),
        ];
        assert_eq!(stash_lines(&repo), expected);
        for again in 0..STASH_SHOWN {
            std::fs::write(repo.join("a.txt"), format!("again {again}\n")).unwrap();
            git::git(&repo, &["stash", "push", "-q", "-m", "again"]).unwrap();
        }
        let lines = stash_lines(&repo);
        assert_eq!(lines.len(), STASH_SHOWN + 2, "{lines:?}");
        assert_eq!(lines.last().unwrap(), "  … and 2 more: git stash list");
        let missing = stash_lines(&dir.path().join("missing"));
        assert!(missing[0].starts_with("✗ cannot list the stash of") && missing.len() == 1, "{missing:?}");
    }

    #[test]
    #[cfg(feature = "native")]
    fn worktree_scan_failure_warns_instead_of_claiming_no_cleanup_needed() {
        let dir = crate::test_support::tempdir().unwrap();
        let lines = worktree_checks(dir.path(), "main");
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with('✗'));
        assert!(lines[0].contains("check repository access"));
        assert_eq!(lines[0].lines().count(), 1);
    }
}
