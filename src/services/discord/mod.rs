mod adk_session;
mod bot_init;
mod commands;
mod formatting;
mod handoff;
pub(crate) mod health;
mod inflight;
mod meeting;
mod metrics;
mod model_catalog;
mod model_picker_interaction;
mod org_schema;
pub(crate) mod org_writer;
mod prompt_builder;
mod queue_io;
mod recovery;
mod restart_ctrl;
pub(crate) mod restart_report;
mod role_map;
mod router;
pub mod runtime_store;
pub(crate) mod settings;
pub(crate) mod shared_memory;
mod shared_state;
#[cfg(unix)]
mod tmux;
mod turn_bridge;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, CreateAttachment, CreateMessage, EditMessage, MessageId, UserId};

use crate::services::agent_protocol::{DEFAULT_ALLOWED_TOOLS, StreamMessage};
use crate::services::claude;
use crate::services::codex;
use crate::services::gemini;
use crate::services::provider::{CancelToken, ProviderKind, ReadOutputResult};
use crate::services::qwen;
use crate::ui::ai_screen::{self, HistoryItem, HistoryType};

use adk_session::{
    build_adk_session_key, derive_adk_session_info, lookup_pending_dispatch_for_thread,
    parse_dispatch_id, post_adk_session_status,
};
use formatting::{
    BUILTIN_SKILLS, add_reaction_raw, extract_skill_description, format_for_discord,
    format_skills_notice, format_tool_input, normalize_empty_lines, remove_reaction_raw,
    send_long_message_raw, truncate_str,
};
use handoff::{clear_handoff, load_handoffs, update_handoff_state};
use inflight::{
    InflightTurnState, clear_inflight_state, load_inflight_states, save_inflight_state,
};
use prompt_builder::build_system_prompt;
use recovery::restore_inflight_turns;
use restart_report::flush_restart_reports;
use router::{handle_event, handle_text_message};
use runtime_store::worktrees_root;
use settings::{
    RoleBinding, channel_upload_dir, cleanup_old_uploads, load_bot_settings, resolve_role_binding,
    save_bot_settings, validate_bot_channel_routing,
};
#[cfg(unix)]
use tmux::{
    cleanup_orphan_tmux_sessions, reap_dead_tmux_sessions, restore_tmux_watchers,
    tmux_output_watcher,
};
use turn_bridge::{TurnBridgeContext, spawn_turn_bridge, tmux_runtime_paths};

pub(crate) use prompt_builder::DispatchProfile;

pub use settings::{
    load_discord_bot_launch_configs, resolve_discord_bot_provider, resolve_discord_token_by_hash,
};

/// Discord message length limit
pub(super) const DISCORD_MSG_LIMIT: usize = 2000;
const MAX_INTERVENTIONS_PER_CHANNEL: usize = 30;
const INTERVENTION_TTL: Duration = Duration::from_secs(10 * 60);
const INTERVENTION_DEDUP_WINDOW: Duration = Duration::from_secs(10);
const UPLOAD_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const UPLOAD_MAX_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const SESSION_CLEANUP_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour
const SESSION_MAX_IDLE: Duration = Duration::from_secs(24 * 60 * 60); // 1 day
const DEAD_SESSION_REAP_INTERVAL: Duration = Duration::from_secs(60); // 1 minute
const RESTART_REPORT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const DEFERRED_RESTART_POLL_INTERVAL: Duration = Duration::from_secs(10);

pub(in crate::services::discord) use bot_init::*;
pub(crate) use bot_init::{
    RunBotContext, retry_failed_dm_notifications, run_bot, send_file_to_channel,
    send_message_to_channel, send_message_to_user,
};
pub(in crate::services::discord) use queue_io::*;
pub(crate) use restart_ctrl::extend_watchdog_deadline;
pub(in crate::services::discord) use restart_ctrl::*;
pub(in crate::services::discord) use shared_state::*;

#[cfg(test)]
mod tests {
    use super::{ChannelId, MessageId, UserId};
    use super::{
        DiscordBotSettings, Intervention, InterventionMode, allows_nonlocal_session_path,
        channel_has_pending_soft_queue, choose_restore_channel_name,
        is_synthetic_thread_channel_name, session_path_is_usable, synthetic_thread_channel_name,
        user_is_authorized, watcher_should_kickoff_idle_queue,
    };
    use crate::services::discord::settings::{
        BotChannelRoutingGuardFailure, validate_bot_channel_routing,
    };
    use crate::services::provider::CancelToken;
    use crate::services::provider::ProviderKind;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn synthetic_thread_channel_name_round_trips() {
        let channel_id = ChannelId::new(12345);
        let synthetic = synthetic_thread_channel_name("agentdesk-codex", channel_id);

        assert_eq!(synthetic, "agentdesk-codex-t12345");
        assert!(is_synthetic_thread_channel_name(&synthetic, channel_id));
        assert!(!is_synthetic_thread_channel_name(
            "agentdesk-codex",
            channel_id
        ));
    }

    #[test]
    fn choose_restore_channel_name_prefers_existing_synthetic_thread_name() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(
            Some("agentdesk-codex-t12345"),
            Some("새 스레드 제목"),
            Some((ChannelId::new(777), Some("agentdesk-codex".to_string()))),
            channel_id,
        );

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex-t12345"));
    }

    #[test]
    fn choose_restore_channel_name_builds_synthetic_name_for_threads() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(
            None,
            Some("새 스레드 제목"),
            Some((ChannelId::new(777), Some("agentdesk-codex".to_string()))),
            channel_id,
        );

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex-t12345"));
    }

    #[test]
    fn choose_restore_channel_name_keeps_existing_name_when_live_metadata_missing() {
        let channel_id = ChannelId::new(12345);
        let chosen = choose_restore_channel_name(Some("agentdesk-codex"), None, None, channel_id);

        assert_eq!(chosen.as_deref(), Some("agentdesk-codex"));
    }

    #[test]
    fn user_is_authorized_allows_owner_and_explicit_users() {
        let settings = DiscordBotSettings {
            owner_user_id: Some(42),
            allowed_user_ids: vec![7],
            ..Default::default()
        };

        assert!(user_is_authorized(&settings, 42));
        assert!(user_is_authorized(&settings, 7));
        assert!(!user_is_authorized(&settings, 99));
    }

    #[test]
    fn user_is_authorized_allows_everyone_when_flag_enabled() {
        let settings = DiscordBotSettings {
            owner_user_id: Some(42),
            allow_all_users: true,
            ..Default::default()
        };

        assert!(user_is_authorized(&settings, 42));
        assert!(user_is_authorized(&settings, 99));
    }

    #[test]
    fn allows_nonlocal_session_path_requires_remote_profile_name() {
        assert!(allows_nonlocal_session_path(Some("mac-mini")));
        assert!(!allows_nonlocal_session_path(Some("")));
        assert!(!allows_nonlocal_session_path(None));
    }

    #[test]
    fn session_path_is_usable_for_remote_nonlocal_path() {
        assert!(session_path_is_usable("~/repo", Some("mac-mini")));
    }

    #[test]
    fn channel_has_pending_soft_queue_detects_live_backlog() {
        let channel_id = ChannelId::new(12345);
        let mut queues = HashMap::new();
        queues.insert(
            channel_id,
            vec![Intervention {
                author_id: UserId::new(42),
                message_id: MessageId::new(7),
                text: "pending".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
            }],
        );

        assert!(channel_has_pending_soft_queue(&mut queues, channel_id));
        assert!(queues.contains_key(&channel_id));
    }

    #[test]
    fn channel_has_pending_soft_queue_prunes_expired_entries() {
        let channel_id = ChannelId::new(12345);
        let mut queues = HashMap::new();
        queues.insert(
            channel_id,
            vec![Intervention {
                author_id: UserId::new(42),
                message_id: MessageId::new(7),
                text: "stale".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now() - (super::INTERVENTION_TTL + Duration::from_secs(1)),
            }],
        );

        assert!(!channel_has_pending_soft_queue(&mut queues, channel_id));
        assert!(!queues.contains_key(&channel_id));
    }

    #[test]
    fn watcher_should_kickoff_idle_queue_requires_idle_channel() {
        let channel_id = ChannelId::new(12345);
        let mut queues = HashMap::new();
        queues.insert(
            channel_id,
            vec![Intervention {
                author_id: UserId::new(42),
                message_id: MessageId::new(7),
                text: "pending".to_string(),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
            }],
        );

        assert!(watcher_should_kickoff_idle_queue(
            false,
            &mut queues,
            channel_id
        ));

        let mut busy_cancel_tokens = HashMap::new();
        busy_cancel_tokens.insert(channel_id, Arc::new(CancelToken::new()));
        assert!(!watcher_should_kickoff_idle_queue(
            busy_cancel_tokens.contains_key(&channel_id),
            &mut queues,
            channel_id
        ));
    }

    #[test]
    fn handoff_routing_guard_rejects_wrong_agent_settings() {
        let settings = DiscordBotSettings {
            provider: ProviderKind::Codex,
            agent: Some("openclaw-maker".to_string()),
            allowed_channel_ids: vec![1488022491992424448],
            ..Default::default()
        };

        let result = validate_bot_channel_routing(
            &settings,
            &ProviderKind::Codex,
            ChannelId::new(1488022491992424448),
            Some("agentdesk-spark"),
            false,
        );

        assert_eq!(result, Err(BotChannelRoutingGuardFailure::AgentMismatch));
    }
}
