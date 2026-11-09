//! Native CLI adapters. Schemas follow the official GitHub Copilot hooks reference and
//! cursor.com/docs/hooks. Unit payloads below are synthetic documentation fixtures.

use std::path::Path;

use serde_json::{Value, json};

use super::shell_quote;
use crate::config::AgentProvider;

const COPILOT_EVENTS: &[&str] = &[
    "sessionStart",
    "userPromptSubmitted",
    "preToolUse",
    "postToolUse",
    "postToolUseFailure",
    "agentStop",
    "sessionEnd",
    "notification",
];
const CURSOR_EVENTS: &[&str] =
    &["sessionStart", "beforeSubmitPrompt", "postToolUse", "postToolUseFailure", "stop", "sessionEnd"];

/// Launch-local Copilot hooks document. The handler is inert unless both ATOMICS routing
/// environment variables are present; destinations never get baked into the handler text.
pub fn copilot_hooks(atomics_exe: &Path) -> Value {
    let exe = shell_quote(&atomics_exe.to_string_lossy());
    let hooks: serde_json::Map<String, Value> = COPILOT_EVENTS
        .iter()
        .map(|event| {
            (
                event.to_string(),
                json!([{
                    "type":"command", "command":format!("{exe} hook --provider copilot --event {event}"),
                    "timeoutSec":15,
                }]),
            )
        })
        .collect();
    json!({"version":1,"hooks":hooks})
}

/// Session-local Cursor plugin hooks/hooks.json. Permission hooks stay with Cursor: even an
/// empty successful reply to preToolUse can block execution, so we do not install it.
pub fn cursor_hooks(atomics_exe: &Path, socket: &Path, pane: &str) -> Value {
    let command = format!(
        "{} hook --socket {} --pane {} --provider cursor",
        shell_quote(&atomics_exe.to_string_lossy()),
        shell_quote(&socket.to_string_lossy()),
        shell_quote(pane)
    );
    let hooks: serde_json::Map<String, Value> = CURSOR_EVENTS
        .iter()
        .map(|event| (event.to_string(), json!([{"command":format!("{command} --event {event}"), "timeout":15}])))
        .collect();
    json!({"version":1,"hooks":hooks})
}

/// Convert native fields to the existing bridge vocabulary. An explicit provider/event is
/// required: a camelCase field or a null transcript must never reinterpret a Claude/Codex hook.
pub fn normalize_native_payload(payload: &Value, provider: AgentProvider, event: &str) -> Option<Value> {
    let get = |keys: &[&str]| keys.iter().find_map(|key| payload.get(*key));
    let text = |keys: &[&str]| get(keys).and_then(Value::as_str).filter(|s| !s.is_empty());
    let name = match (provider, event) {
        (_, "sessionStart" | "SessionStart") => "SessionStart",
        (_, "sessionEnd" | "SessionEnd") => "SessionEnd",
        (AgentProvider::Copilot, "userPromptSubmitted" | "UserPromptSubmit")
        | (AgentProvider::Cursor, "beforeSubmitPrompt") => "UserPromptSubmit",
        (_, "preToolUse" | "PreToolUse") => "PreToolUse",
        (_, "postToolUse" | "PostToolUse") => "PostToolUse",
        (_, "postToolUseFailure" | "PostToolUseFailure") => "PostToolUseFailure",
        (AgentProvider::Copilot, "agentStop" | "Stop") | (AgentProvider::Cursor, "stop") => "Stop",
        (AgentProvider::Copilot, "notification" | "Notification") => "Notification",
        // In particular permissionRequest is not Codex's interactive approval bridge.
        _ => return None,
    };
    let session_id = match provider {
        AgentProvider::Copilot => text(&["sessionId", "session_id"]),
        AgentProvider::Cursor => text(&["conversation_id"])
            .or_else(|| matches!(name, "SessionStart" | "SessionEnd").then(|| text(&["session_id"])).flatten()),
    }?;
    let mut out = json!({"hook_event_name":name,"session_id":session_id,"atomics_provider":provider});
    for (field, aliases) in [
        ("cwd", &["cwd"][..]),
        ("prompt", &["prompt", "initialPrompt", "initial_prompt"]),
        ("notification_type", &["notification_type", "notificationType"]),
        ("message", &["message", "error_message", "error"]),
        ("tool_name", &["tool_name", "toolName"]),
        ("tool_input", &["tool_input", "toolArgs"]),
        ("tool_response", &["tool_response", "tool_output", "toolResult", "tool_result"]),
        ("tool_use_id", &["tool_use_id", "toolUseId"]),
        ("last_assistant_message", &["last_assistant_message", "response"]),
        ("agent_id", &["agent_id", "agentId", "subagent_id"]),
        ("transcript_path", &["transcript_path", "transcriptPath"]),
        ("stop_hook_active", &["stop_hook_active"]),
        ("is_interrupt", &["is_interrupt"]),
        ("completion_status", &["status"]),
        ("loop_count", &["loop_count"]),
    ] {
        if let Some(value) = get(aliases) {
            out[field] = value.clone();
        }
    }
    // Copilot can supply a JSON string for toolArgs; the canonical tool fields are objects.
    if let Some(input) = out["tool_input"].as_str()
        && let Ok(parsed) = serde_json::from_str::<Value>(input)
    {
        out["tool_input"] = parsed;
    }
    if out.get("cwd").is_none()
        && let Some(root) = payload.get("workspace_roots").and_then(Value::as_array).and_then(|roots| roots.first())
        && root.is_string()
    {
        out["cwd"] = root.clone();
    }
    // Null Cursor transcripts mean transcript recording is disabled, not a hidden Codex agent.
    out["atomics_background"] = json!(
        payload.get("is_background_agent").and_then(Value::as_bool) == Some(true)
            || out["notification_type"].as_str() == Some("agent_idle")
    );
    // Some native subagent payloads identify their parent instead of repeating an agent id.
    out["atomics_spawned"] = json!(text(&["parent_conversation_id", "parentSessionId", "parent_session_id"]).is_some());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{HookEvent, HookThread};
    use crate::session::AgentStatus;

    fn event(provider: AgentProvider, name: &str, payload: Value) -> HookEvent {
        HookEvent::from_payload(&normalize_native_payload(&payload, provider, name).unwrap()).unwrap()
    }

    #[test]
    fn documented_copilot_fields_and_pascal_aliases_normalize() {
        for (name, payload) in [
            (
                "postToolUse",
                json!({"sessionId":"one","toolName":"bash","toolArgs":"{\"command\":\"pwd\"}","toolResult":{"resultType":"success"}}),
            ),
            (
                "PostToolUse",
                json!({"hook_event_name":"PostToolUse","session_id":"one","tool_name":"bash","tool_input":{"command":"pwd"},"tool_result":{"result_type":"success"}}),
            ),
        ] {
            let event = event(AgentProvider::Copilot, name, payload);
            assert_eq!(event.session_id, "one");
            assert_eq!(event.tool_input["command"], "pwd");
            assert_eq!(event.tool_name.as_deref(), Some("bash"));
            assert_eq!(event.status(), Some(AgentStatus::Working));
            assert_eq!(
                serde_json::from_str::<Value>(&event.context_output("queued").unwrap()).unwrap(),
                json!({"additionalContext":"queued"})
            );
        }
        assert!(
            normalize_native_payload(&json!({"sessionId":"one"}), AgentProvider::Copilot, "permissionRequest")
                .is_none()
        );
    }

    #[test]
    fn cursor_tracks_conversations_and_disabled_transcripts_stay_main() {
        for generation in ["turn1", "turn2"] {
            let event = event(
                AgentProvider::Cursor,
                "postToolUse",
                json!({"conversation_id":"stable", "generation_id":generation,"session_id":"wrong","transcript_path":null,"workspace_roots":["/work"]}),
            );
            assert_eq!(event.session_id, "stable");
            assert_eq!(event.thread, HookThread::Main);
            assert_eq!(event.cwd.as_deref(), Some(Path::new("/work")));
        }
        assert!(normalize_native_payload(&json!({"generation_id":"turn"}), AgentProvider::Cursor, "stop").is_none());
    }

    #[test]
    fn stop_decisions_use_native_outputs_and_preserve_undeliverable_work() {
        let copilot = event(AgentProvider::Copilot, "agentStop", json!({"sessionId":"one"}));
        assert_eq!(
            serde_json::from_str::<Value>(&copilot.context_output("next").unwrap()).unwrap(),
            json!({"decision":"block","reason":"next"})
        );
        assert!(
            !event(AgentProvider::Copilot, "agentStop", json!({"sessionId":"one","stop_hook_active":true}))
                .can_deliver()
        );
        for status in ["completed", "aborted", "error"] {
            let cursor =
                event(AgentProvider::Cursor, "stop", json!({"conversation_id":"one","status":status,"loop_count":0}));
            assert_eq!(
                cursor.status(),
                Some(if status == "completed" { AgentStatus::Done } else { AgentStatus::Idle })
            );
            assert_eq!(cursor.can_deliver(), status == "completed");
        }
        let cursor =
            event(AgentProvider::Cursor, "stop", json!({"conversation_id":"one","status":"completed","loop_count":0}));
        assert_eq!(
            serde_json::from_str::<Value>(&cursor.context_output("next").unwrap()).unwrap(),
            json!({"followup_message":"next"})
        );
        assert!(
            !event(AgentProvider::Cursor, "stop", json!({"conversation_id":"one","status":"completed","loop_count":5}))
                .can_deliver()
        );
        assert!(
            !event(
                AgentProvider::Cursor,
                "postToolUseFailure",
                json!({
                    "conversation_id":"one","is_interrupt":true
                })
            )
            .can_deliver()
        );
    }

    #[test]
    fn notifications_and_subagents_do_not_consume_main_thread_work() {
        for kind in ["permission_prompt", "elicitation_dialog", "agent_idle", "agent_completed"] {
            let event =
                event(AgentProvider::Copilot, "notification", json!({"sessionId":"one","notification_type":kind}));
            assert!(!event.can_deliver());
            assert_eq!(event.thread == HookThread::Background, kind == "agent_idle");
            assert_eq!(
                event.status(),
                matches!(kind, "permission_prompt" | "elicitation_dialog").then_some(AgentStatus::NeedsInput)
            );
        }
        let spawned = event(AgentProvider::Copilot, "postToolUse", json!({"sessionId":"one","agentId":"child"}));
        assert_eq!(spawned.thread, HookThread::Spawned);
        assert!(spawned.context_output("main only").is_none());
        let cursor = event(
            AgentProvider::Cursor,
            "postToolUse",
            json!({
                "conversation_id":"child","parent_conversation_id":"main"
            }),
        );
        assert_eq!(cursor.thread, HookThread::Spawned);
        assert!(!cursor.can_deliver());
    }

    #[test]
    fn generated_configs_use_native_schemas_and_leave_permissions_native() {
        let copilot = copilot_hooks(Path::new("/path with spaces/atomics"));
        assert_eq!(copilot["version"], 1);
        assert!(copilot["hooks"].get("permissionRequest").is_none());
        assert!(
            copilot["hooks"]["agentStop"][0]["command"]
                .as_str()
                .unwrap()
                .contains("--provider copilot --event agentStop")
        );
        let cursor = cursor_hooks(Path::new("atomics"), Path::new("/socket"), "pane");
        assert!(cursor["hooks"].get("preToolUse").is_none());
        assert!(cursor["hooks"].get("afterAgentResponse").is_none());
        assert!(cursor["hooks"]["stop"][0].get("hooks").is_none());
        assert_eq!(cursor["hooks"]["stop"][0]["timeout"], 15);
    }
}
