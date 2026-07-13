use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecoveryDiscordChannelRelation {
    NonThread,
    Thread {
        parent_id: ChannelId,
        parent_name: Option<String>,
    },
    Unresolved,
}

pub(super) async fn resolve_recovery_memory_scope(
    http: &Arc<serenity::Http>,
    state: &InflightTurnState,
) -> Option<(ChannelId, Option<String>)> {
    if let Some(scope) = persisted_recovery_memory_scope(state) {
        return Some(scope);
    }

    let channel_id = ChannelId::new(state.channel_id);
    if resolve_role_binding(channel_id, state.channel_name.as_deref()).is_some() {
        return Some((channel_id, state.channel_name.clone()));
    }

    let relation = resolve_recovery_discord_channel_relation(http, channel_id).await;
    resolve_legacy_recovery_memory_scope(channel_id, state.channel_name.clone(), relation)
}

fn persisted_recovery_memory_scope(
    state: &InflightTurnState,
) -> Option<(ChannelId, Option<String>)> {
    state.memory_scope_channel_id.map(|scope_id| {
        (
            ChannelId::new(scope_id),
            state.memory_scope_channel_name.clone(),
        )
    })
}

async fn resolve_recovery_discord_channel_relation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
) -> RecoveryDiscordChannelRelation {
    let Ok(channel) = channel_id.to_channel(http).await else {
        return RecoveryDiscordChannelRelation::Unresolved;
    };
    let serenity::model::channel::Channel::Guild(channel) = channel else {
        return RecoveryDiscordChannelRelation::NonThread;
    };
    if !crate::utils::discord::is_thread_channel_type(channel.kind) {
        return RecoveryDiscordChannelRelation::NonThread;
    }
    let Some(parent_id) = channel.parent_id else {
        return RecoveryDiscordChannelRelation::Unresolved;
    };
    let Ok(serenity::model::channel::Channel::Guild(parent)) = parent_id.to_channel(http).await
    else {
        return RecoveryDiscordChannelRelation::Unresolved;
    };
    RecoveryDiscordChannelRelation::Thread {
        parent_id,
        parent_name: Some(parent.name),
    }
}

fn resolve_legacy_recovery_memory_scope(
    channel_id: ChannelId,
    channel_name: Option<String>,
    relation: RecoveryDiscordChannelRelation,
) -> Option<(ChannelId, Option<String>)> {
    if resolve_role_binding(channel_id, channel_name.as_deref()).is_some() {
        return Some((channel_id, channel_name));
    }
    match relation {
        RecoveryDiscordChannelRelation::NonThread => Some((channel_id, channel_name)),
        RecoveryDiscordChannelRelation::Thread {
            parent_id,
            parent_name,
        } => {
            if settings::resolve_inherited_role_binding(parent_id, parent_name.as_deref()).is_some()
            {
                Some((parent_id, parent_name))
            } else {
                Some((channel_id, channel_name))
            }
        }
        RecoveryDiscordChannelRelation::Unresolved => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD_ID: u64 = 4_317_299;
    const PARENT_ID: u64 = 4_317_200;

    fn legacy_inflight() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            CHILD_ID,
            Some("thread".to_string()),
            7,
            8,
            9,
            "legacy recovery".to_string(),
            Some("session-4317".to_string()),
            None,
            None,
            None,
            0,
        )
    }

    fn with_recovery_scope_config(yaml: &str, test: impl FnOnce()) {
        let root = tempfile::tempdir().expect("temp AgentDesk root");
        let config_dir = root.path().join("config");
        std::fs::create_dir_all(&config_dir).expect("create config dir");
        std::fs::write(config_dir.join("agentdesk.yaml"), yaml).expect("write AgentDesk config");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        test();
    }

    fn parent_relation() -> RecoveryDiscordChannelRelation {
        RecoveryDiscordChannelRelation::Thread {
            parent_id: ChannelId::new(PARENT_ID),
            parent_name: Some("adk-cdx".to_string()),
        }
    }

    #[test]
    fn persisted_memory_scope_is_authoritative() {
        with_recovery_scope_config("server:\n  port: 8791\nagents: []\n", || {
            let mut state = legacy_inflight();
            state.set_memory_scope(PARENT_ID, Some("persisted-parent".to_string()));
            assert_eq!(
                persisted_recovery_memory_scope(&state),
                Some((
                    ChannelId::new(PARENT_ID),
                    Some("persisted-parent".to_string())
                ))
            );
        });
    }

    #[test]
    fn legacy_scope_resolves_inherited_parent_and_direct_child_precedence() {
        with_recovery_scope_config(
            &format!(
                "server:\n  port: 8791\nagents:\n  - id: project-agentdesk\n    name: AgentDesk\n    provider: codex\n    channels:\n      codex:\n        id: \"{PARENT_ID}\"\n        name: adk-cdx\n"
            ),
            || {
                assert_eq!(
                    resolve_legacy_recovery_memory_scope(
                        ChannelId::new(CHILD_ID),
                        Some("thread".to_string()),
                        parent_relation(),
                    ),
                    Some((ChannelId::new(PARENT_ID), Some("adk-cdx".to_string())))
                );
            },
        );

        with_recovery_scope_config(
            &format!(
                "server:\n  port: 8791\nagents:\n  - id: direct-child\n    name: Direct Child\n    provider: codex\n    channels:\n      codex:\n        id: \"{CHILD_ID}\"\n        name: thread\n"
            ),
            || {
                assert_eq!(
                    resolve_legacy_recovery_memory_scope(
                        ChannelId::new(CHILD_ID),
                        Some("thread".to_string()),
                        parent_relation(),
                    ),
                    Some((ChannelId::new(CHILD_ID), Some("thread".to_string())))
                );
            },
        );
    }

    #[test]
    fn legacy_scope_honors_opt_out_and_fails_closed_when_unresolved() {
        with_recovery_scope_config(
            &format!(
                "server:\n  port: 8791\nagents:\n  - id: project-agentdesk\n    name: AgentDesk\n    provider: codex\n    channels:\n      codex:\n        id: \"{PARENT_ID}\"\n        name: adk-cdx\n        threadInherit: false\n"
            ),
            || {
                assert_eq!(
                    resolve_legacy_recovery_memory_scope(
                        ChannelId::new(CHILD_ID),
                        Some("thread".to_string()),
                        parent_relation(),
                    ),
                    Some((ChannelId::new(CHILD_ID), Some("thread".to_string())))
                );
                assert_eq!(
                    resolve_legacy_recovery_memory_scope(
                        ChannelId::new(CHILD_ID),
                        Some("possible-thread".to_string()),
                        RecoveryDiscordChannelRelation::Unresolved,
                    ),
                    None
                );
                assert_eq!(
                    resolve_legacy_recovery_memory_scope(
                        ChannelId::new(CHILD_ID),
                        Some("regular-channel".to_string()),
                        RecoveryDiscordChannelRelation::NonThread,
                    ),
                    Some((
                        ChannelId::new(CHILD_ID),
                        Some("regular-channel".to_string())
                    ))
                );
            },
        );
    }
}
