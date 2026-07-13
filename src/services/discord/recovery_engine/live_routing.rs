use poise::serenity_prelude::ChannelId;

use crate::services::discord::DiscordBotSettings;
use crate::services::discord::settings::{self, BotChannelRoutingGuardFailure};
use crate::services::provider::ProviderKind;

/// No-event recovery has no live message/interaction proving child authority.
/// Unknown Discord metadata must therefore preserve persisted state without
/// proceeding; only a genuine provider mismatch carries a destructive reason.
pub(super) fn validate_recovery_no_event_routing(
    settings_snapshot: &DiscordBotSettings,
    provider: &ProviderKind,
    channel_id: ChannelId,
    is_dm: bool,
    live_child_name: Option<&str>,
    thread_parent: Option<(ChannelId, Option<&str>)>,
) -> Result<(), Option<BotChannelRoutingGuardFailure>> {
    if !is_dm && live_child_name.is_none() {
        return Err(None);
    }
    match settings::validate_bot_channel_routing_with_thread_parent(
        settings_snapshot,
        provider,
        channel_id,
        live_child_name,
        thread_parent,
        is_dm,
    ) {
        Ok(()) => Ok(()),
        Err(reason) if reason.orphans_inflight_on_restart() => Err(Some(reason)),
        Err(_) => Err(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD_ID: u64 = 15_046_124_559_162_459;
    const PARENT_ID: u64 = 14_796_713_013_870_592;

    fn bot_settings(provider: ProviderKind, allowed_channel_ids: Vec<u64>) -> DiscordBotSettings {
        DiscordBotSettings {
            provider,
            allowed_channel_ids,
            agent: Some("project-agentdesk".to_string()),
            ..Default::default()
        }
    }

    fn with_role_map(include_child: bool, test: impl FnOnce()) {
        let root = tempfile::tempdir().expect("temp AgentDesk root");
        let config = root.path().join("config");
        std::fs::create_dir_all(&config).expect("create config dir");
        let child = if include_child {
            format!(
                r#",
    "{CHILD_ID}": {{
      "roleId": "review-agent",
      "promptFile": "/tmp/review-agent.md",
      "provider": "claude"
    }}"#
            )
        } else {
            String::new()
        };
        std::fs::write(
            config.join("role_map.json"),
            format!(
                r#"{{
  "byChannelId": {{
    "{PARENT_ID}": {{
      "roleId": "project-agentdesk",
      "promptFile": "/tmp/project-agentdesk.md",
      "provider": "codex",
      "threadInherit": true
    }}{child}
  }}
}}"#
            ),
        )
        .expect("write role map");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        test();
    }

    #[test]
    fn no_event_routing_requires_resolved_child_but_accepts_dm_and_inherited_parent() {
        with_role_map(false, || {
            let codex = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            assert_eq!(
                validate_recovery_no_event_routing(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(CHILD_ID),
                    false,
                    None,
                    None,
                ),
                Err(None)
            );
            let mut direct_child = bot_settings(ProviderKind::Codex, vec![CHILD_ID]);
            direct_child.agent = None;
            assert_eq!(
                validate_recovery_no_event_routing(
                    &direct_child,
                    &ProviderKind::Codex,
                    ChannelId::new(CHILD_ID),
                    false,
                    None,
                    None,
                ),
                Err(None),
                "an allowlisted child ID cannot prove no-event authority when live metadata is unresolved"
            );
            assert_eq!(
                validate_recovery_no_event_routing(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(CHILD_ID),
                    false,
                    Some("child-thread"),
                    Some((ChannelId::new(PARENT_ID), None)),
                ),
                Ok(())
            );
            assert_eq!(
                validate_recovery_no_event_routing(
                    &codex,
                    &ProviderKind::Codex,
                    ChannelId::new(CHILD_ID),
                    true,
                    None,
                    None,
                ),
                Ok(())
            );
        });
    }

    #[test]
    fn no_event_routing_direct_child_wins_and_cross_bot_failures_preserve() {
        with_role_map(true, || {
            let parent_bot = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            assert_eq!(
                validate_recovery_no_event_routing(
                    &parent_bot,
                    &ProviderKind::Codex,
                    ChannelId::new(CHILD_ID),
                    false,
                    Some("review-thread"),
                    Some((ChannelId::new(PARENT_ID), Some("adk-cdx"))),
                ),
                Err(None),
                "ChannelNotAllowed for a directly-bound sibling must preserve"
            );

            let mut child_bot = bot_settings(ProviderKind::Claude, vec![CHILD_ID]);
            child_bot.agent = Some("review-agent".to_string());
            assert_eq!(
                validate_recovery_no_event_routing(
                    &child_bot,
                    &ProviderKind::Claude,
                    ChannelId::new(CHILD_ID),
                    false,
                    Some("review-thread"),
                    Some((ChannelId::new(PARENT_ID), Some("adk-cdx"))),
                ),
                Ok(())
            );
        });

        with_role_map(false, || {
            let mut wrong_agent = bot_settings(ProviderKind::Codex, vec![PARENT_ID]);
            wrong_agent.agent = Some("review-agent".to_string());
            assert_eq!(
                validate_recovery_no_event_routing(
                    &wrong_agent,
                    &ProviderKind::Codex,
                    ChannelId::new(CHILD_ID),
                    false,
                    Some("child-thread"),
                    Some((ChannelId::new(PARENT_ID), Some("adk-cdx"))),
                ),
                Err(None),
                "AgentMismatch is an expected sibling skip"
            );
        });
    }

    #[test]
    fn no_event_routing_surfaces_provider_mismatch_for_genuine_cleanup() {
        with_role_map(false, || {
            let mut wrong_provider = bot_settings(ProviderKind::Claude, vec![PARENT_ID]);
            wrong_provider.agent = Some("project-agentdesk".to_string());
            assert_eq!(
                validate_recovery_no_event_routing(
                    &wrong_provider,
                    &ProviderKind::Claude,
                    ChannelId::new(CHILD_ID),
                    false,
                    Some("child-thread"),
                    Some((ChannelId::new(PARENT_ID), Some("adk-cdx"))),
                ),
                Err(Some(BotChannelRoutingGuardFailure::ProviderMismatch))
            );
        });
    }
}
