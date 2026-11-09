//! Native interactive CLI adapters. All configuration is private to this launch; neither the
//! repository nor the provider's account/authentication directory is rewritten.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use atomics_core::config::AgentProvider;
use serde_json::{Value, json};

/// Add a provider's interactive prompt and exact session selection after its ordinary task flags.
/// Cursor assigns its own conversation id; its sessionStart hook persists it once available.
pub(crate) fn interactive_args(
    provider: AgentProvider,
    args: &mut Vec<String>,
    resume: Option<&str>,
    prompt: Option<&str>,
) -> Result<Option<String>, String> {
    // Session selectors in task arguments would compete with ATOMICS's persisted identity.
    for arg in args.iter() {
        let name = arg.split('=').next().unwrap_or(arg);
        if matches!(name, "--resume" | "--continue" | "--session-id" | "--connect" | "-r") {
            return Err(format!("{name} is managed by ATOMICS; resume the saved session instead"));
        }
        if matches!(name, "-p" | "--print" | "--prompt") {
            return Err(format!("{name} exits the interactive CLI; provide the task as the ATOMICS prompt instead"));
        }
    }
    let resume = match resume {
        Some(id) if id.trim().is_empty() || id.starts_with('-') => {
            return Err("Resuming this provider requires an exact session id".into());
        }
        other => other,
    };
    let id = match provider {
        AgentProvider::Copilot => {
            // Unlike --resume, --session-id never matches a prefix or a conversation's name.
            let id = resume.map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            args.extend(["--session-id".into(), id.clone()]);
            if let Some(prompt) = prompt {
                args.extend(["-i".into(), prompt.into()]);
            }
            Some(id)
        }
        AgentProvider::Cursor => {
            if let Some(id) = resume {
                args.extend(["--resume".into(), id.into()]);
            }
            if let Some(prompt) = prompt {
                // End option parsing so a prompt beginning with '-' stays a prompt.
                args.extend(["--".into(), prompt.into()]);
            }
            resume.map(str::to_owned)
        }
    };
    Ok(id)
}

pub(crate) fn directory(home: &Path, pane: &str) -> PathBuf {
    home.join("provider-launches").join(pane)
}

/// Return launch-local plugin arguments and environment. Copilot's documented --plugin-dir
/// supports hooks/hooks.json; Cursor's format uses .cursor-plugin/plugin.json and mcp.json.
/// Sandboxes use sbx's existing MCP gateway and cannot execute this machine's hook helper.
#[allow(clippy::too_many_arguments)]
pub(crate) fn configure(
    provider: AgentProvider,
    home: &Path,
    exe: &Path,
    hook_exe: &Path,
    socket: &Path,
    pane: &str,
    chief_prompt: Option<&str>,
    args: &mut Vec<String>,
    env: &mut HashMap<String, String>,
) -> Result<(), String> {
    if chief_prompt.is_some()
        && provider == AgentProvider::Copilot
        && args.iter().any(|arg| arg == "--no-custom-instructions")
    {
        return Err(
            "Copilot chief requires custom instructions; remove --no-custom-instructions from [chief].args".into()
        );
    }
    let dir = directory(home, pane);
    std::fs::create_dir_all(dir.join("hooks")).map_err(|error| format!("Creating provider plugin: {error}"))?;
    let mut mcp = json!({"mcpServers": {"atomics": {
        "command": exe, "args": ["mcp", if chief_prompt.is_some() { "--chief" } else { "--agent" }],
        "env": {"ATOMICS_PANE": pane, "ATOMICS_SOCKET": socket}
    }}});
    match provider {
        AgentProvider::Copilot => {
            write_json(
                &dir.join("plugin.json"),
                &json!({
                    "name": "atomics-session", "version": "1.0.0", "hooks": "hooks/hooks.json"
                }),
            )?;
            let mut hooks = atomics_core::hook::copilot_hooks(hook_exe);
            if let Some(prompt) = chief_prompt {
                mark_chief_hooks(&mut hooks);
                let instructions = dir.join("instructions");
                std::fs::create_dir_all(&instructions)
                    .map_err(|error| format!("Creating chief instructions: {error}"))?;
                write_text(&instructions.join("AGENTS.md"), prompt)?;
                // The native CLI merges these with the user's existing instructions on every
                // turn, including exact resumes. Authentication and history keep their home.
                let inherited = env
                    .get("COPILOT_CUSTOM_INSTRUCTIONS_DIRS")
                    .cloned()
                    .or_else(|| std::env::var("COPILOT_CUSTOM_INSTRUCTIONS_DIRS").ok())
                    .filter(|value| !value.trim().is_empty());
                let path = instructions.to_string_lossy();
                if path.contains(',') {
                    return Err("Copilot chief instructions require an ATOMICS_HOME path without commas".into());
                }
                env.insert(
                    "COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into(),
                    match inherited {
                        Some(inherited) => format!("{inherited},{path}"),
                        None => path.into_owned(),
                    },
                );
            }
            write_json(&dir.join("hooks/hooks.json"), &hooks)?;
            env.insert(atomics_core::hook::HOOK_PANE_ENV.into(), pane.into());
            env.insert(atomics_core::hook::HOOK_SOCKET_ENV.into(), socket.to_string_lossy().into_owned());
            mcp["mcpServers"]["atomics"]["type"] = json!("local");
            mcp["mcpServers"]["atomics"]["tools"] = json!(["*"]);
            args.extend(["--additional-mcp-config".into(), mcp.to_string()]);
        }
        AgentProvider::Cursor => {
            std::fs::create_dir_all(dir.join(".cursor-plugin"))
                .map_err(|error| format!("Creating Cursor plugin: {error}"))?;
            write_json(
                &dir.join(".cursor-plugin/plugin.json"),
                &json!({
                    "name": "atomics-session", "version": "1.0.0"
                }),
            )?;
            let mut hooks = atomics_core::hook::cursor_hooks(hook_exe, socket, pane);
            if let Some(prompt) = chief_prompt {
                mark_chief_hooks(&mut hooks);
                let rules = dir.join("rules");
                std::fs::create_dir_all(&rules).map_err(|error| format!("Creating chief rules: {error}"))?;
                write_text(
                    &rules.join("atomics-chief.mdc"),
                    &format!(
                        "---\ndescription: ATOMICS chief orchestration instructions\nalwaysApply: true\n---\n\n{prompt}\n"
                    ),
                )?;
            }
            write_json(&dir.join("hooks/hooks.json"), &hooks)?;
            write_json(&dir.join("mcp.json"), &mcp)?;
        }
    }
    args.extend(["--plugin-dir".into(), dir.to_string_lossy().into_owned()]);
    Ok(())
}

/// Called only once this pane's process group is gone, or its launch failed.
pub(crate) fn cleanup(home: &Path, pane: &str) {
    // Pane ids are generated UUIDs. Refuse paths that could escape our own launch directory.
    if uuid::Uuid::parse_str(pane).is_err() {
        return;
    }
    let dir = directory(home, pane);
    if let Err(error) = std::fs::remove_dir_all(&dir)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("ATOMICS: removing {}: {error}", dir.display());
    }
}

fn mark_chief_hooks(hooks: &mut Value) {
    if let Some(events) = hooks["hooks"].as_object_mut() {
        for entries in events.values_mut().filter_map(Value::as_array_mut) {
            for entry in entries {
                if let Some(command) = entry["command"].as_str() {
                    entry["command"] = format!("{command} --chief").into();
                }
            }
        }
    }
}

fn write_text(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|error| format!("Writing {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    std::fs::write(path, bytes).map_err(|error| format!("Writing {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_keeps_initial_prompt_interactive_and_exact_identity() {
        let mut args = vec!["--model".into(), "gpt-5".into()];
        let id = interactive_args(AgentProvider::Copilot, &mut args, None, Some("fix tests")).unwrap().unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert_eq!(args, ["--model", "gpt-5", "--session-id", &id, "-i", "fix tests"]);
        let mut resumed = Vec::new();
        assert_eq!(interactive_args(AgentProvider::Copilot, &mut resumed, Some(&id), None).unwrap(), Some(id.clone()));
        assert_eq!(resumed, ["--session-id", &id]);
    }

    #[test]
    fn cursor_uses_prompt_and_resume_options_without_latest_fallback() {
        let mut args = vec!["--mode".into(), "plan".into()];
        assert_eq!(
            interactive_args(AgentProvider::Cursor, &mut args, Some("chat-123"), Some("-fix tests")).unwrap(),
            Some("chat-123".into())
        );
        assert_eq!(args, ["--mode", "plan", "--resume", "chat-123", "--", "-fix tests"]);
        assert!(interactive_args(AgentProvider::Cursor, &mut Vec::new(), Some(""), None).is_err());
        assert!(interactive_args(AgentProvider::Cursor, &mut vec!["--continue".into()], None, None).is_err());
    }

    #[test]
    fn plugins_scope_reporting_without_changing_provider_home() {
        let home = tempfile::tempdir().unwrap();
        for (provider, pane) in [(AgentProvider::Copilot, "copilot-pane"), (AgentProvider::Cursor, "cursor-pane")] {
            let mut args = Vec::new();
            let mut env = HashMap::new();
            configure(
                provider,
                home.path(),
                Path::new("/atomics"),
                Path::new("/atomics-hook"),
                Path::new("/socket"),
                pane,
                None,
                &mut args,
                &mut env,
            )
            .unwrap();
            let plugin = directory(home.path(), pane);
            let hooks: Value =
                serde_json::from_slice(&std::fs::read(plugin.join("hooks/hooks.json")).unwrap()).unwrap();
            assert!(hooks["hooks"].is_object());
            assert!(!env.contains_key("COPILOT_HOME"));
            assert!(!args.iter().any(|arg| arg == "--force" || arg == "--yolo" || arg == "--allow-all-tools"));
            let mcp: Value = match provider {
                AgentProvider::Copilot => serde_json::from_str(&args[1]).unwrap(),
                AgentProvider::Cursor => {
                    serde_json::from_slice(&std::fs::read(plugin.join("mcp.json")).unwrap()).unwrap()
                }
            };
            if provider == AgentProvider::Copilot {
                assert_eq!(mcp["mcpServers"]["atomics"]["type"], "local");
                assert_eq!(mcp["mcpServers"]["atomics"]["tools"], json!(["*"]));
            }
            assert_eq!(mcp["mcpServers"]["atomics"]["args"], json!(["mcp", "--agent"]));
            assert_eq!(mcp["mcpServers"]["atomics"]["env"]["ATOMICS_PANE"], pane);
        }
    }

    #[test]
    fn chief_plugins_load_persistent_instructions_and_chief_only_tools() {
        let home = tempfile::tempdir().unwrap();
        for provider in [AgentProvider::Copilot, AgentProvider::Cursor] {
            let pane = provider.name();
            let mut args = Vec::new();
            let mut env = HashMap::from([("COPILOT_CUSTOM_INSTRUCTIONS_DIRS".into(), "/user/instructions".into())]);
            configure(
                provider,
                home.path(),
                Path::new("/atomics"),
                Path::new("/hook"),
                Path::new("/socket"),
                pane,
                Some("CHIEF_INSTRUCTIONS: plan, coordinate and review."),
                &mut args,
                &mut env,
            )
            .unwrap();
            let plugin = directory(home.path(), pane);
            let mcp: Value = match provider {
                AgentProvider::Copilot => serde_json::from_str(&args[1]).unwrap(),
                AgentProvider::Cursor => {
                    serde_json::from_slice(&std::fs::read(plugin.join("mcp.json")).unwrap()).unwrap()
                }
            };
            assert_eq!(mcp["mcpServers"]["atomics"]["args"], json!(["mcp", "--chief"]));
            let hooks: Value =
                serde_json::from_slice(&std::fs::read(plugin.join("hooks/hooks.json")).unwrap()).unwrap();
            for entries in hooks["hooks"].as_object().unwrap().values() {
                assert!(
                    entries
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|entry| entry["command"].as_str().unwrap().ends_with(" --chief"))
                );
            }
            let instructions = match provider {
                AgentProvider::Copilot => {
                    assert_eq!(
                        env["COPILOT_CUSTOM_INSTRUCTIONS_DIRS"],
                        format!("/user/instructions,{}", plugin.join("instructions").display())
                    );
                    std::fs::read_to_string(plugin.join("instructions/AGENTS.md")).unwrap()
                }
                AgentProvider::Cursor => {
                    let rule = std::fs::read_to_string(plugin.join("rules/atomics-chief.mdc")).unwrap();
                    assert!(rule.contains("alwaysApply: true"));
                    rule
                }
            };
            assert!(instructions.contains("CHIEF_INSTRUCTIONS: plan, coordinate and review."));
            assert!(!env.contains_key("COPILOT_HOME"));
        }
    }
}
