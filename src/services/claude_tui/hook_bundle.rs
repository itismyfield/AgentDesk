use sha2::{Digest, Sha256};
use std::path::Path;

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookBundleConfig {
    pub endpoint: String,
    pub provider: String,
    pub session_id: String,
    pub agentdesk_exe: String,
}

const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "SubagentStop",
];

const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
];

pub fn render_claude_hook_settings(config: &HookBundleConfig) -> Value {
    let mut hooks = serde_json::Map::new();
    for event in CLAUDE_HOOK_EVENTS {
        let hook = json!({
            "type": "command",
            "command": hook_relay_command(config, event),
            "timeout": 5
        });
        let matcher = if matches!(*event, "PreToolUse" | "PostToolUse") {
            json!({
                "matcher": "*",
                "hooks": [hook]
            })
        } else {
            json!({
                "hooks": [hook]
            })
        };
        hooks.insert((*event).to_string(), json!([matcher]));
    }

    json!({
        "hooks": hooks
    })
}

pub fn render_codex_hook_config_override(config: &HookBundleConfig) -> String {
    let mut rendered = String::from("hooks={");
    let mut first_event = true;
    for event in CODEX_HOOK_EVENTS {
        if !first_event {
            rendered.push(',');
        }
        first_event = false;
        rendered.push_str(event);
        rendered.push_str("=[{");
        if let Some(matcher) = codex_event_matcher(event) {
            rendered.push_str("matcher = ");
            rendered.push_str(&toml_string(matcher));
            rendered.push(',');
        }
        rendered.push_str("hooks=[{type=\"command\",command=");
        rendered.push_str(&toml_string(&codex_hook_relay_command(config, event)));
        rendered.push_str(",timeout=5,statusMessage=");
        rendered.push_str(&toml_string(&format!("AgentDesk {event} hook relay")));
        rendered.push_str(",async=false}]}]");
    }
    rendered.push_str(",state={");
    // Codex CLI 0.130 does not expose a usable hook-trust bypass flag. Keep the
    // relay non-persistent by installing it as a session-flag hook override and
    // pairing it with the matching session-flag trust hashes.
    for (index, event) in CODEX_HOOK_EVENTS.iter().enumerate() {
        if index > 0 {
            rendered.push(',');
        }
        let key = codex_session_flag_hook_state_key(event);
        rendered.push_str(&toml_string(&key));
        rendered.push_str("={trusted_hash=");
        rendered.push_str(&toml_string(&codex_hook_trust_hash(config, event)));
        rendered.push('}');
    }
    rendered.push_str("}}");
    rendered
}

pub fn codex_hook_config_overrides(config: &HookBundleConfig) -> Vec<String> {
    vec![
        "features.hooks=true".to_string(),
        render_codex_hook_config_override(config),
    ]
}

pub fn write_claude_hook_settings(path: &Path, config: &HookBundleConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create hook settings dir {}: {error}", parent.display()))?;
    }
    let rendered = serde_json::to_string_pretty(&render_claude_hook_settings(config))
        .map_err(|error| format!("render hook settings: {error}"))?;
    std::fs::write(path, rendered)
        .map_err(|error| format!("write hook settings {}: {error}", path.display()))
}

fn hook_relay_command(config: &HookBundleConfig, event: &str) -> String {
    [
        shell_quote(&config.agentdesk_exe),
        "claude-hook-relay".to_string(),
        "--endpoint".to_string(),
        shell_quote(&config.endpoint),
        "--provider".to_string(),
        shell_quote(&config.provider),
        "--event".to_string(),
        shell_quote(event),
        "--session-id".to_string(),
        shell_quote(&config.session_id),
    ]
    .join(" ")
}

fn codex_hook_relay_command(config: &HookBundleConfig, event: &str) -> String {
    [
        shell_quote(&config.agentdesk_exe),
        "codex-hook-relay".to_string(),
        "--endpoint".to_string(),
        shell_quote(&config.endpoint),
        "--provider".to_string(),
        shell_quote(&config.provider),
        "--event".to_string(),
        shell_quote(event),
        "--session-id".to_string(),
        shell_quote(&config.session_id),
    ]
    .join(" ")
}

fn codex_event_matcher(event: &str) -> Option<&'static str> {
    match event {
        "SessionStart" => Some("startup|resume|clear"),
        "PreToolUse" | "PermissionRequest" | "PostToolUse" => Some("*"),
        _ => None,
    }
}

fn codex_event_key_label(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "Stop" => "stop",
        _ => "unknown",
    }
}

fn codex_session_flag_hook_state_key(event: &str) -> String {
    format!("/config.toml:{}:0:0", codex_event_key_label(event))
}

fn codex_hook_trust_hash(config: &HookBundleConfig, event: &str) -> String {
    let mut handler = serde_json::Map::new();
    handler.insert("async".to_string(), Value::Bool(false));
    handler.insert(
        "command".to_string(),
        Value::String(codex_hook_relay_command(config, event)),
    );
    handler.insert(
        "statusMessage".to_string(),
        Value::String(format!("AgentDesk {event} hook relay")),
    );
    handler.insert("timeout".to_string(), Value::Number(5.into()));
    handler.insert("type".to_string(), Value::String("command".to_string()));

    let mut identity = serde_json::Map::new();
    identity.insert(
        "event_name".to_string(),
        Value::String(codex_event_key_label(event).to_string()),
    );
    if let Some(matcher) = codex_event_matcher(event) {
        identity.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    identity.insert(
        "hooks".to_string(),
        Value::Array(vec![Value::Object(handler)]),
    );

    let canonical = canonical_json(&Value::Object(identity));
    let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(serialized);
    let hash = hasher.finalize();
    let hex = hash
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{hex}")
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys = map.keys().cloned().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                if let Some(value) = map.get(&key) {
                    sorted.insert(key, canonical_json(value));
                }
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

fn toml_string(value: &str) -> String {
    let escaped = value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect::<String>();
    format!("\"{escaped}\"")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r#"'\''"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> HookBundleConfig {
        HookBundleConfig {
            endpoint: "http://127.0.0.1:49152".to_string(),
            provider: "claude".to_string(),
            session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
            agentdesk_exe: "/tmp/Agent Desk/agentdesk".to_string(),
        }
    }

    #[test]
    fn hook_settings_render_all_required_claude_events() {
        let settings = render_claude_hook_settings(&sample_config());
        let hooks = settings["hooks"].as_object().unwrap();

        for event in CLAUDE_HOOK_EVENTS {
            assert!(hooks.contains_key(*event), "missing {event}");
        }
        assert_eq!(hooks["PreToolUse"][0]["matcher"], "*");
        assert_eq!(hooks["PostToolUse"][0]["matcher"], "*");
        assert!(hooks["Stop"][0]["matcher"].is_null());
    }

    #[test]
    fn codex_hook_config_override_renders_all_current_events() {
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let settings = render_codex_hook_config_override(&config);

        for event in CODEX_HOOK_EVENTS {
            assert!(settings.contains(&format!("{event}=[")), "missing {event}");
        }
        assert!(settings.starts_with("hooks={"));
        assert!(settings.contains("matcher = \"startup|resume|clear\""));
        assert!(settings.contains("matcher = \"*\""));
        assert!(settings.contains("codex-hook-relay"));
        assert!(settings.contains("--provider codex"));
        assert!(settings.contains("\"/config.toml:stop:0:0\"={trusted_hash=\"sha256:"));
    }

    #[test]
    fn codex_hook_config_overrides_enable_and_trust_hooks_for_session() {
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let overrides = codex_hook_config_overrides(&config);

        assert_eq!(overrides.len(), 2);
        assert_eq!(overrides[0], "features.hooks=true");
        assert!(overrides[1].starts_with("hooks={"));
        assert!(overrides[1].contains("\"/config.toml:session_start:0:0\"={trusted_hash="));
    }

    #[test]
    fn hook_command_shell_quotes_executable_with_spaces() {
        let settings = render_claude_hook_settings(&sample_config());
        let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();

        assert!(command.starts_with("'/tmp/Agent Desk/agentdesk' claude-hook-relay"));
        assert!(command.contains("--event Stop"));
        assert!(command.contains("--session-id 01234567-89ab-cdef-0123-456789abcdef"));
    }

    #[test]
    fn write_hook_settings_creates_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("settings.json");

        write_claude_hook_settings(&path, &sample_config()).unwrap();

        let raw = std::fs::read_to_string(path).unwrap();
        assert!(raw.contains("claude-hook-relay"));
        assert!(raw.contains("SessionStart"));
    }

    #[test]
    fn codex_hook_trust_hash_changes_when_command_identity_changes() {
        let mut config = sample_config();
        config.provider = "codex".to_string();
        let first = codex_hook_trust_hash(&config, "Stop");

        config.session_id.push_str("-new");
        let second = codex_hook_trust_hash(&config, "Stop");

        assert_ne!(first, second);
        assert!(first.starts_with("sha256:"));
        assert!(second.starts_with("sha256:"));
    }
}
