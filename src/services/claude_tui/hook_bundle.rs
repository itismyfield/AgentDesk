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
}
