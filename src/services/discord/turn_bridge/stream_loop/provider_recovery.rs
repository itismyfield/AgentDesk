//! Persist before waking the watchdog; never stop/start runtimes from their own bridge.
use crate::services::agent_recovery::{self, CheckpointPayload, ObserveInput, RecoveryLease};
use crate::services::discord::{
    SharedData,
    inflight::{InflightTurnIdentity, InflightTurnState},
};
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;

pub(super) async fn on_error(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    lease: Option<&RecoveryLease>,
    expected: &InflightTurnIdentity,
    inflight: &InflightTurnState,
    allow_profile_retry: bool,
    any_tool_used: bool,
    partial_response: &str,
    message: &str,
    stderr: &str,
) -> bool {
    if !herdr_before_start_mutant("on_error_callsite_guard_removed")
        && !herdr_before_start_mutant("on_error_guard_after_profile")
        && before_start_recovery_blocked(provider, channel, &shared.token_hash, expected, inflight)
    {
        return false;
    }
    // A profile retry owns dispatch recovery; do not also request an agent takeover.
    if allow_profile_retry
        && try_profile_retry(
            provider,
            channel,
            expected,
            inflight,
            any_tool_used,
            partial_response,
            message,
            stderr,
        )
    {
        return true;
    }
    if herdr_before_start_mutant("on_error_guard_after_profile")
        && before_start_recovery_blocked(provider, channel, &shared.token_hash, expected, inflight)
    {
        return false;
    }
    let Some(lease) = lease else {
        return false;
    };
    let Some(signal) = agent_recovery::trigger_from_error_message(message)
        .or_else(|| agent_recovery::trigger_from_error_message(stderr))
    else {
        return false;
    };
    if !crate::services::discord::inflight::load_inflight_state_read_only(provider, channel.get())
        .is_some_and(|current| expected.matches_state(&current))
    {
        return false;
    }
    let workspace = shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|session| session.current_path.clone());
    let checkpoint = CheckpointPayload::compact(
        &lease.active_writer_agent_id,
        inflight.user_text.chars().take(4000).collect::<String>(),
        partial_response.chars().take(8000).collect::<String>(),
        "",
        Vec::new(),
        "Inspect the inherited workspace and continue the unfinished request.",
        &inflight.user_text,
    );
    match agent_recovery::observe_provider_error(
        lease,
        ObserveInput {
            channel_id: channel.get().to_string(),
            primary_turn_id: inflight.effective_finalizer_turn_id().to_string(),
            signal,
        },
        checkpoint,
        workspace,
    )
    .await
    {
        Ok(true) => agent_recovery::recovery_wakeup(provider).notify_one(),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(channel_id = channel.get(), error = %error, "provider error recovery not committed; retaining normal error handling")
        }
    }
    false
}

/// Retry only an anchored, unprocessed request. A tool may have committed an
/// external effect even when the provider subsequently reports a limit error.
pub(super) fn profile_retry_eligible(
    request: u64,
    any_tool_used: bool,
    partial_response: &str,
    message: &str,
    stderr: &str,
) -> bool {
    request != 0
        && !any_tool_used
        && partial_response.trim().is_empty()
        && agent_recovery::trigger_from_error_message(message)
            .or_else(|| agent_recovery::trigger_from_error_message(stderr))
            .is_some()
}

pub(super) fn try_profile_retry(
    provider: &ProviderKind,
    channel: ChannelId,
    expected: &InflightTurnIdentity,
    inflight: &InflightTurnState,
    any_tool_used: bool,
    partial_response: &str,
    message: &str,
    stderr: &str,
) -> bool {
    if inflight.turn_source != crate::services::discord::inflight::TurnSource::Managed
        || !profile_retry_eligible(
            inflight.user_msg_id,
            any_tool_used || inflight.any_tool_used,
            partial_response,
            message,
            stderr,
        )
        || !crate::services::discord::inflight::load_inflight_state_read_only(
            provider,
            channel.get(),
        )
        .is_some_and(|current| expected.matches_state(&current))
    {
        return false;
    }
    // A durable agent takeover pins its account and must keep that authority.
    if !matches!(
        agent_recovery::pinned_auth_profile(&channel.get().to_string(), provider, None),
        Ok(None)
    ) {
        return false;
    }
    match crate::services::discord::org_schema::advance_auth_profile(
        provider,
        channel.get(),
        inflight.user_msg_id,
    ) {
        Ok(Some((from, to))) => {
            tracing::info!(provider = provider.as_str(), channel_id = channel.get(), from_profile = %from, to_profile = %to, "retrying unprocessed request with another auth profile");
            true
        }
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(channel_id = channel.get(), %error, "profile fallback unavailable");
            false
        }
    }
}

#[cfg(test)]
mod profile_tests {
    use super::profile_retry_eligible;

    #[test]
    fn only_unprocessed_anchored_classified_failures_are_replayed() {
        for error in [
            "HTTP 429",
            "quota exhausted",
            "provider produced no output for 180 seconds",
            "tmux session dead",
        ] {
            assert!(profile_retry_eligible(12, false, "", error, ""));
            assert!(!profile_retry_eligible(12, true, "", error, ""));
            assert!(!profile_retry_eligible(
                12,
                false,
                "partial answer",
                error,
                ""
            ));
            assert!(!profile_retry_eligible(0, false, "", error, ""));
        }
        for error in [
            "usage fetch failed",
            "permission denied",
            "user cancelled",
            "invalid configuration",
        ] {
            assert!(!profile_retry_eligible(12, false, "", error, ""));
        }
        assert!(profile_retry_eligible(
            12,
            false,
            "",
            "provider failed",
            "RESOURCE_EXHAUSTED"
        ));
    }
}

fn before_start_recovery_blocked(
    provider: &ProviderKind,
    channel: ChannelId,
    token_hash: &str,
    expected: &InflightTurnIdentity,
    inflight: &InflightTurnState,
) -> bool {
    use crate::services::provider::cancel_token_claude_interrupt::{
        herdr_stop_settlement_available, herdr_turn,
    };
    if !herdr_stop_settlement_available()
        || crate::services::provider::herdr_before_start::mutant("on_error_ignores_user_stop")
    {
        return false;
    }
    if herdr_before_start_mutant("recovery_requires_locator") {
        return false;
    }
    let Some((logical, nonce)) = inflight
        .tmux_session_name
        .as_deref()
        .zip(inflight.turn_nonce.as_deref())
    else {
        return false;
    };
    let Some(state) = herdr_turn(logical, nonce) else {
        return crate::services::provider::cancel_token_claude_interrupt::herdr_turn_indexed(
            logical, nonce,
        );
    };
    if state.owner.discord_token_hash != token_hash
        || state.owner.provider != provider.as_str()
        || state.owner.channel_id != channel.get().to_string()
        || !expected.matches_state(inflight)
        || !crate::services::discord::inflight::load_inflight_state_read_only(
            provider,
            channel.get(),
        )
        .is_some_and(|row| {
            expected.matches_state(&row)
                && row.turn_nonce == inflight.turn_nonce
                && row.provider == inflight.provider
                && row.born_generation == inflight.born_generation
        })
    {
        return true;
    }
    state.user_stop.load(std::sync::atomic::Ordering::Acquire)
        || state.closed_probe().unwrap_or(true)
}

#[cfg(test)]
#[cfg(unix)]
mod coldstop_recovery_tests {
    use super::*;
    use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
    use crate::services::provider::CancelToken;
    use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
    use std::sync::atomic::Ordering;
    #[tokio::test(flavor = "current_thread")]
    async fn coldstop_on_error_blocks_profile_and_takeover_before_any_recovery() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let provider = shared.provider.clone();
        let actor = CancelToken::new();
        let channel = ChannelId::new(913);
        let logical = "AgentDesk-cold-recovery";
        let owner = HostedOwner {
            provider: provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: "913".into(),
            logical_key: logical.into(),
            owner_node: "node".into(),
            runtime_root: "root".into(),
        };
        let state = actor.prepare_herdr_interrupt(provider.clone(), &owner);
        let mut row = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            1,
            913,
            0,
            "cold".into(),
            None,
            Some(logical.into()),
            None,
            None,
            0,
        );
        row.turn_nonce = actor.turn_nonce().map(str::to_owned);
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        let expected = InflightTurnIdentity::from_state(&row);
        assert!(!before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        state.user_stop.store(true, Ordering::Release);
        assert!(before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        let before = serde_json::to_vec(
            &crate::services::discord::inflight::load_inflight_state_read_only(
                &provider,
                channel.get(),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            !on_error(
                &shared,
                &provider,
                channel,
                None,
                &expected,
                &row,
                true,
                false,
                "",
                "quota exhausted",
                ""
            )
            .await
        );
        assert_eq!(
            before,
            serde_json::to_vec(
                &crate::services::discord::inflight::load_inflight_state_read_only(
                    &provider,
                    channel.get()
                )
                .unwrap()
            )
            .unwrap()
        );
        HERDR_SETTLEMENT_OVERRIDE.set(false);
        assert!(!before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        let mut missing = row.clone();
        missing.turn_nonce = Some("missing".into());
        assert!(!before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &missing
        ));
        state.user_stop.store(false, Ordering::Release);
        let lock = state.submission.lock().unwrap();
        assert!(before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        drop(lock);
        // Two live turns sharing the row's exact index name no state; the guard fails closed.
        let twin = CancelToken::from_persisted_turn_nonce(row.turn_nonce.clone());
        let twin_state = twin.prepare_herdr_interrupt(provider.clone(), &owner);
        assert!(before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
        drop((twin_state, twin));
        assert!(!before_start_recovery_blocked(
            &provider,
            channel,
            &shared.token_hash,
            &expected,
            &row
        ));
    }
    fn cold_row(shared: &SharedData, channel: u64) -> (CancelToken, InflightTurnState) {
        let actor = CancelToken::new();
        let logical = format!("AgentDesk-recovery-{channel}");
        let owner = HostedOwner {
            provider: shared.provider.as_str().into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: channel.to_string(),
            logical_key: logical.clone(),
            owner_node: "node".into(),
            runtime_root: "root".into(),
        };
        actor.prepare_herdr_interrupt(shared.provider.clone(), &owner);
        let mut row = InflightTurnState::new(
            shared.provider.clone(),
            channel,
            None,
            1,
            channel,
            0,
            "request".into(),
            None,
            Some(logical),
            None,
            None,
            0,
        );
        row.turn_nonce = actor.turn_nonce().map(str::to_owned);
        crate::services::discord::inflight::save_inflight_state(&row).unwrap();
        (actor, row)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coldstop_real_profile_retry_positive_stop_and_off() {
        use crate::services::provider_auth_profile::fallback::select_for_launch;
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let provider = crate::services::discord::make_shared_data_for_tests_with_storage(None)
            .provider
            .clone();
        // The global home is the fallback: a named primary fails over to it with no profile home.
        let schema = format!(
            "version: 1\nagents: {{}}\nprovider_auth_fallbacks:\n  {}:\n    priority: [default]\n    include_remaining: false\n",
            provider.as_str()
        );
        let path = crate::services::discord::runtime_store::org_schema_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, schema).unwrap();
        for (stop, enabled, channel) in [(false, true, 917), (true, true, 918), (true, false, 919)]
        {
            HERDR_SETTLEMENT_OVERRIDE.set(enabled);
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let (actor, row) = cold_row(&shared, channel);
            actor
                .herdr_interrupt_state()
                .unwrap()
                .user_stop
                .store(stop, Ordering::Release);
            let primary = format!("coldstop-primary-{channel}");
            let candidates = [primary.clone(), "default".to_owned()];
            assert_eq!(
                select_for_launch(&shared.provider, channel, &candidates, |_| true),
                Some(primary.clone())
            );
            let identity = InflightTurnIdentity::from_state(&row);
            let retried = on_error(
                &shared,
                &shared.provider,
                ChannelId::new(channel),
                None,
                &identity,
                &row,
                true,
                false,
                "",
                "quota exhausted",
                "",
            )
            .await;
            assert_eq!(retried, !stop || !enabled, "actual profile transition");
            assert_eq!(
                select_for_launch(&shared.provider, channel, &candidates, |_| true),
                Some(if retried {
                    "default".to_owned()
                } else {
                    primary
                })
            );
        }
        HERDR_SETTLEMENT_OVERRIDE.set(true);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn coldstop_real_checkpoint_wakeup_positive_stop_off_and_absent_pg() {
        let root = tempfile::tempdir().unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        for (stop, enabled, indexed, channel) in [
            (false, true, true, 920),
            (true, true, true, 921),
            (true, false, true, 922),
            (true, true, false, 923),
        ] {
            HERDR_SETTLEMENT_OVERRIDE.set(enabled);
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let (actor, mut row) = cold_row(&shared, channel);
            actor
                .herdr_interrupt_state()
                .unwrap()
                .user_stop
                .store(stop, Ordering::Release);
            if !indexed {
                row.turn_nonce = Some("unindexed".into());
                crate::services::discord::inflight::save_inflight_state(&row).unwrap();
            }
            let config = agent_recovery::RecoveryConfigWire {
                enabled: Some(true),
                fallback_agent_id: Some("backup".into()),
                stall_secs: Some(180),
                workspace_mode: Some("inherit".into()),
                triggers: None,
            };
            let agents = [
                agent_recovery::OrgAgentInput {
                    id: "owner".into(),
                    provider: Some("claude".into()),
                    model: None,
                    workspace: Some("/fixture".into()),
                    auth_profile: "default".into(),
                    recovery: Some(config.clone()),
                },
                agent_recovery::OrgAgentInput {
                    id: "backup".into(),
                    provider: Some("codex".into()),
                    model: None,
                    workspace: None,
                    auth_profile: "default".into(),
                    recovery: None,
                },
            ];
            let channels = [agent_recovery::OrgChannelInput {
                channel_id: channel.to_string(),
                agent: "owner".into(),
                provider: None,
                workspace: None,
                auth_profile: None,
                recovery: Some(config),
            }];
            let catalog = agent_recovery::build_recovery_catalog(&agents, &channels).unwrap();
            let lease = RecoveryLease {
                channel_id: channel.to_string(),
                generation: 0,
                active_writer_agent_id: "owner".into(),
            };
            let notify = agent_recovery::recovery_wakeup(&shared.provider);
            while tokio::time::timeout(std::time::Duration::from_millis(1), notify.notified())
                .await
                .is_ok()
            {}
            let identity = InflightTurnIdentity::from_state(&row);
            agent_recovery::test_store::with_store(
                pool.clone(),
                catalog,
                on_error(
                    &shared,
                    &shared.provider,
                    ChannelId::new(channel),
                    Some(&lease),
                    &identity,
                    &row,
                    false,
                    false,
                    "",
                    "quota exhausted",
                    "",
                ),
            )
            .await;
            let effects = !stop || !enabled || !indexed;
            let checkpoint =
                agent_recovery::checkpoint::load_checkpoint_events(&pool, &channel.to_string(), 20)
                    .await
                    .unwrap();
            assert_eq!(!checkpoint.is_empty(), effects, "durable checkpoint");
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_millis(10), notify.notified())
                    .await
                    .is_ok(),
                effects,
                "consumable wakeup"
            );
            assert_eq!(
                agent_recovery::checkpoint::load_channel_state(&pool, &channel.to_string())
                    .await
                    .unwrap()
                    .is_some(),
                effects
            );
        }
        HERDR_SETTLEMENT_OVERRIDE.set(true);
        pool.close().await;
        db.drop().await;
    }
}

fn herdr_before_start_mutant(name: &str) -> bool {
    crate::services::provider::herdr_before_start::mutant(name)
}
