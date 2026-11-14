//! Provider session naming under the immutable process namespace.

use super::ProviderKind;
use crate::utils::format::safe_prefix;
use std::sync::OnceLock;

static TEST_PREFIX: OnceLock<String> = OnceLock::new();
pub const TMUX_SESSION_PREFIX: &str = "AgentDesk";

pub(crate) fn install_test_prefix(prefix: &str) -> Result<(), String> {
    let id = prefix
        .strip_prefix("AdkTest-")
        .ok_or("invalid test tmux prefix")?;
    if !uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
        return Err("invalid test tmux prefix".into());
    }
    match TEST_PREFIX.set(prefix.to_string()) {
        Ok(()) => Ok(()),
        Err(value) if TEST_PREFIX.get() == Some(&value) => Ok(()),
        Err(_) => Err("test tmux prefix already installed".into()),
    }
}

pub fn tmux_session_prefix() -> &'static str {
    TEST_PREFIX
        .get()
        .map_or(TMUX_SESSION_PREFIX, String::as_str)
}

/// Tmux session name suffix (reserved for future isolation use; currently empty).
pub fn tmux_env_suffix() -> &'static str {
    ""
}

impl ProviderKind {
    pub fn build_tmux_session_name(&self, channel_name: &str) -> String {
        let sanitized: String = channel_name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        // #145: Preserve -t{thread_id} suffix when truncating, so unified-thread
        // guards (is_unified_thread_channel_name_active) can extract the thread ID.
        let trimmed: String = if let Some(pos) = sanitized.rfind("-t") {
            let suffix = &sanitized[pos..];
            let is_thread_suffix =
                suffix.len() > 2 && suffix[2..].chars().all(|c| c.is_ascii_digit());
            if is_thread_suffix && sanitized.len() > 44 {
                let prefix_budget = 44_usize.saturating_sub(suffix.len());
                let prefix = safe_prefix(&sanitized[..pos], prefix_budget);
                format!("{}{}", prefix, suffix)
            } else {
                safe_prefix(&sanitized, 44).to_string()
            }
        } else {
            safe_prefix(&sanitized, 44).to_string()
        };
        format!(
            "{}-{}-{}{}",
            tmux_session_prefix(),
            self.as_str(),
            trimmed,
            tmux_env_suffix()
        )
    }
}
