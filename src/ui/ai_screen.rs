//! Compatibility shim for types originally defined in RCC's ui::ai_screen module.
//! Only the data types needed by services::discord are provided here.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryItem {
    pub item_type: HistoryType,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryType {
    User,
    Assistant,
    Error,
    System,
    ToolUse,
    ToolResult,
}

/// Session data structure for file persistence
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionData {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub history: Vec<HistoryItem>,
    pub current_path: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_channel_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_channel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_category_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_profile_name: Option<String>,
    #[serde(default)]
    pub born_generation: u64,
}

/// Get the AI sessions directory path ($AGENTDESK_ROOT_DIR/ai_sessions)
#[allow(dead_code)]
pub fn ai_sessions_dir() -> Option<PathBuf> {
    crate::cli::dcserver::agentdesk_runtime_root().map(|root| root.join("ai_sessions"))
}

/// Sanitize user input — remove common prompt injection patterns and truncate.
pub fn sanitize_user_input(input: &str) -> String {
    use crate::utils::format::safe_truncate;

    let mut sanitized = input.to_string();

    let dangerous_patterns = [
        "ignore previous instructions",
        "ignore all previous",
        "disregard previous",
        "forget previous",
        "system prompt",
        "you are now",
        "act as if",
        "pretend you are",
        "new instructions:",
        "[system]",
        "[admin]",
        "---begin",
        "---end",
    ];

    let lower_input = sanitized.to_lowercase();
    for pattern in dangerous_patterns {
        if lower_input.contains(pattern) {
            sanitized = sanitized.replace(pattern, "[filtered]");
            let pattern_lower = pattern.to_lowercase();
            let pattern_upper = pattern.to_uppercase();
            let pattern_title: String = pattern
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i == 0 {
                        c.to_uppercase().next().unwrap_or(c)
                    } else {
                        c
                    }
                })
                .collect();
            sanitized = sanitized.replace(&pattern_lower, "[filtered]");
            sanitized = sanitized.replace(&pattern_upper, "[filtered]");
            sanitized = sanitized.replace(&pattern_title, "[filtered]");
        }
    }

    const MAX_INPUT_LENGTH: usize = 4000;
    if sanitized.len() > MAX_INPUT_LENGTH {
        safe_truncate(&mut sanitized, MAX_INPUT_LENGTH);
        sanitized.push_str("... [truncated]");
    }

    sanitized
}
