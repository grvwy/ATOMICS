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
    args: &mut Vec<String>,
    env: &mut HashMap<String, String>,
) -> Result<(), String> {
    let dir = directory(home, pane);
    std::fs::create_dir_all(dir.join("hooks")).map_err(|error| format!("Creating provider plugin: {error}"))?;
    let mut mcp = json!({"mcpServers": {"atomics": {
        "command": exe, "args": ["mcp", "--agent"],
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
            write_json(&dir.join("hooks/hooks.json"), &atomics_core::hook::copilot_hooks(hook_exe))?;
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
            write_json(&dir.join("hooks/hooks.json"), &atomics_core::hook::cursor_hooks(hook_exe, socket, pane))?;
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
}
