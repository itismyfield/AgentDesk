//! Bot-level settings persisted to disk and the per-user records kept alongside them.

use super::*;

/// Bot-level settings persisted to disk
#[derive(Clone)]
pub(in crate::services) struct DiscordBotSettings {
    /// Optional agent identity (e.g. "codex", "spark") for same-provider isolation.
    pub(in crate::services) agent: Option<String>,
    pub(in crate::services) provider: ProviderKind,
    pub(in crate::services) allowed_tools: Vec<String>,
    /// Explicit Discord channel allowlist for this bot token.
    /// Empty means "no channel restriction".
    pub(in crate::services) allowed_channel_ids: Vec<u64>,
    /// Channels that require an explicit bot mention before intake proceeds.
    pub(in crate::services) require_mention_channel_ids: Vec<u64>,
    /// channel_id (string) → persisted model override
    pub(in crate::services) channel_model_overrides: std::collections::HashMap<String, String>,
    /// channel_id (string) → native fast mode enabled
    pub(in crate::services) channel_fast_modes: std::collections::HashMap<String, bool>,
    /// channel_id (string) → pending native fast mode reset on the next turn
    pub(in crate::services) channel_fast_mode_reset_pending: std::collections::HashSet<String>,
    /// channel_id (string) → Codex goals feature enabled
    pub(in crate::services) channel_codex_goals: std::collections::HashMap<String, bool>,
    /// channel_id (string) → pending Codex goals session reset on the next turn
    pub(in crate::services) channel_codex_goals_reset_pending: std::collections::HashSet<String>,
    /// channel_id (string) → pending same-session relaunch of the channel's local Claude TUI
    pub(in crate::services) channel_relaunch_pending: std::collections::HashSet<String>,
    /// channel_id (string) → effort level a local Claude TUI launches with
    pub(in crate::services) channel_effort_levels: std::collections::HashMap<String, String>,
    /// channel_id (string) → selected cluster node instance for intake routing
    pub(in crate::services) channel_node_overrides: std::collections::HashMap<String, String>,
    /// Discord user ID of the registered owner (must be configured explicitly)
    pub(in crate::services) owner_user_id: Option<u64>,
    /// Additional authorized user IDs (bot settings `allowed_user_ids`)
    pub(in crate::services) allowed_user_ids: Vec<u64>,
    /// When true, any Discord user may talk to this bot in allowed channels.
    pub(in crate::services) allow_all_users: bool,
    /// Bot IDs whose messages are NOT ignored (e.g. announce bot for CEO directives)
    pub(in crate::services) allowed_bot_ids: Vec<u64>,
}

impl Default for DiscordBotSettings {
    fn default() -> Self {
        Self {
            agent: None,
            provider: ProviderKind::Claude,
            allowed_tools: DEFAULT_ALLOWED_TOOLS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allowed_channel_ids: Vec::new(),
            require_mention_channel_ids: Vec::new(),
            channel_model_overrides: std::collections::HashMap::new(),
            channel_fast_modes: std::collections::HashMap::new(),
            channel_fast_mode_reset_pending: std::collections::HashSet::new(),
            channel_codex_goals: std::collections::HashMap::new(),
            channel_codex_goals_reset_pending: std::collections::HashSet::new(),
            channel_relaunch_pending: std::collections::HashSet::new(),
            channel_effort_levels: std::collections::HashMap::new(),
            channel_node_overrides: std::collections::HashMap::new(),
            owner_user_id: None,
            allowed_user_ids: Vec::new(),
            allow_all_users: false,
            allowed_bot_ids: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub(in crate::services) struct ModelPickerPendingState {
    pub(in crate::services) owner_user_id: UserId,
    pub(in crate::services) target_channel_id: ChannelId,
    pub(in crate::services) pending_model: Option<String>,
    pub(in crate::services) updated_at: Instant,
}

pub(super) const CHANNEL_ROSTER_MAX_USERS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services) struct UserRecord {
    pub(in crate::services) id: UserId,
    pub(in crate::services) name: String,
}

impl UserRecord {
    pub(in crate::services) fn new(id: UserId, name: &str) -> Self {
        let collapsed = name.split_whitespace().collect::<Vec<_>>().join(" ");
        let base = if collapsed.is_empty() {
            format!("user {}", id.get())
        } else {
            collapsed
        };
        let sanitized = base
            .chars()
            .map(|ch| match ch {
                '\r' | '\n' => ' ',
                _ => ch,
            })
            .collect::<String>();
        Self {
            id,
            name: sanitized.split_whitespace().collect::<Vec<_>>().join(" "),
        }
    }

    pub(in crate::services) fn label(&self) -> String {
        format!("{} (ID: {})", self.name, self.id.get())
    }
}
