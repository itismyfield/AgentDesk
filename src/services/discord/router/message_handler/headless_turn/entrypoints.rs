use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_reserved_headless_turn_with_owner(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    tmux_session_label: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    use crate::services::discord::input_runtime::fence::effect;
    let permit = effect::admit(&shared.provider, channel_id.get()).map_err(|failure| {
        HeadlessTurnStartError::Conflict(format!("headless input admission refused: {failure:?}"))
    })?;
    if permit.is_none() && effect::current().is_none() {
        return Box::pin(start_reserved_headless_turn_admitted(
            ctx,
            channel_id,
            prompt,
            request_owner_name,
            request_owner,
            shared,
            token,
            source,
            metadata,
            channel_name_hint,
            tmux_session_label,
            is_dm_hint,
            reservation,
        ))
        .await;
    }
    let (ctx, prompt, request_owner_name, shared, token, source) = (
        ctx.clone(),
        prompt.to_owned(),
        request_owner_name.to_owned(),
        shared.clone(),
        token.to_owned(),
        source.map(str::to_owned),
    );
    effect::run(permit, async move {
        Box::pin(start_reserved_headless_turn_admitted(
            &ctx,
            channel_id,
            &prompt,
            &request_owner_name,
            request_owner,
            &shared,
            &token,
            source.as_deref(),
            metadata,
            channel_name_hint,
            tmux_session_label,
            is_dm_hint,
            reservation,
        ))
        .await
    })
    .await
}

pub(in crate::services::discord) async fn start_headless_turn(
    (ctx, shared, token): (&serenity::Context, &Arc<SharedData>, &str),
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        None,
        None,
        reserve_headless_turn(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_reserved_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    // #5: synthetic tmux-session label for routine turns (see
    // `start_reserved_headless_turn_with_owner`); `None` for all other callers.
    tmux_session_label: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        UserId::new(1),
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        tmux_session_label,
        is_dm_hint,
        reservation,
    )
    .await
}

#[allow(dead_code)] // #3034: exported voice entry point, wired-but-dormant (no live dispatch yet).
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_voice_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        request_owner,
        shared,
        token,
        Some(crate::dispatch::Source::Voice.as_str()),
        metadata,
        channel_name_hint,
        None,
        Some(false),
        reserve_headless_turn(),
    )
    .await
}

#[cfg(test)]
mod input_effect_tests {
    use super::*;
    use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
    use crate::services::discord::{input_runtime::fence, live_bridge};
    use futures::FutureExt;

    #[tokio::test]
    async fn c1b_headless_owner_refuses_closing_before_settings_and_original_registration() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
        let mock = discord_mock::DiscordMockState::new();
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        let channel = ChannelId::new(6_325_523);
        let gate = fence::Gate::protect(ProviderKind::Codex, channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let closing = gate.close().unwrap();
        let recovery = live_bridge::try_recovery(&ProviderKind::Codex, channel.get()).unwrap();
        let settings = shared.settings.write().await;
        let result = start_reserved_headless_turn_with_owner(
            &ctx,
            channel,
            "headless input",
            "owner",
            UserId::new(7),
            &shared,
            "",
            None,
            None,
            None,
            None,
            None,
            reserve_headless_turn(),
        )
        .now_or_never()
        .expect("Closing must precede settings and original registration");
        assert!(
            matches!(result, Err(HeadlessTurnStartError::Conflict(ref error)) if error.contains("Closing"))
        );
        assert!(!live_bridge::is_live(&ProviderKind::Codex, channel.get()));
        assert!(
            crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Codex,
                channel.get()
            )
            .is_none()
        );
        assert!(!shared.core.lock().await.sessions.contains_key(&channel));
        assert!(mock.unhandled.lock().unwrap().is_empty());
        drop(settings);
        drop(recovery);
        closing.drain().await;
        shared.mailboxes.remove_fixture_for_test(channel);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn c1b_headless_actual_start_holds_provider_and_bridge_effect_until_cleanup() {
        if !crate::services::discord::admin_host_guard::tests::api_child(concat!(
            "services::discord::router::message_handler::headless_turn::entrypoints::input_effect_tests::",
            "c1b_headless_actual_start_holds_provider_and_bridge_effect_until_cleanup"
        )) {
            return;
        }
        let _root = crate::config::TestRuntimeRootGuard::new();
        let boot = serde_json::from_value(serde_json::json!({"server": {}, "agents": []})).unwrap();
        crate::services::tui_o::channel_policy::install(&boot).unwrap();
        let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
        crate::services::discord::internal_api::init(api.port, None);
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().api_port = api.port;
        let mock = discord_mock::DiscordMockState::new();
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        let channel = ChannelId::new(6_325_525);
        let workspace = tempfile::tempdir().unwrap();
        crate::services::discord::host_defer_gate::tests::map_channel(&shared, channel, "").await;
        {
            let mut core = shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = None;
            session.current_path = Some(workspace.path().display().to_string());
            session.session_id = Some("input-effect-existing".into());
        }
        let gate = fence::Gate::protect(shared.provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let (entered, provider_entered) = tokio::sync::oneshot::channel();
        let (release_provider, release) = std::sync::mpsc::channel();
        *super::super::provider_dispatch::INPUT_EFFECT_PROBE
            .lock()
            .unwrap() = Some(super::super::provider_dispatch::InputEffectProbe {
            channel: channel.get(),
            entered,
            release,
            terminal_before_release: false,
        });
        let (completed, provider_completed) = tokio::sync::oneshot::channel();
        *super::super::provider_dispatch::INPUT_EFFECT_COMPLETION_PROBE
            .lock()
            .unwrap() = Some((channel.get(), completed));
        let (captured, bridge_captured) = tokio::sync::oneshot::channel();
        let (release_bridge, resume) = tokio::sync::oneshot::channel();
        *crate::services::discord::turn_bridge::resume_pin_tests::BRIDGE_CAPTURE_PROBE
            .lock()
            .unwrap() = Some((channel, captured, resume));
        let started = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            start_reserved_headless_turn_with_owner(
                &ctx,
                channel,
                "input effect",
                "owner",
                UserId::new(7),
                &shared,
                "",
                None,
                Some(serde_json::json!({"silent": true})),
                None,
                None,
                Some(true),
                reserve_headless_turn(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(started.status, HeadlessTurnStartStatus::Started);
        let provider = tokio::time::timeout(std::time::Duration::from_secs(10), provider_entered)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), bridge_captured)
            .await
            .unwrap()
            .unwrap();
        assert!(
            crate::services::discord::inflight::load_inflight_state(
                &shared.provider,
                channel.get()
            )
            .is_some()
        );
        let closing = gate.close().unwrap();
        let held = closing.drain().now_or_never().is_none();
        release_provider.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), provider_completed)
            .await
            .unwrap()
            .unwrap();
        let bridge_holds_after_provider = closing.drain().now_or_never().is_none();
        release_bridge.send(()).unwrap();
        assert!(
            bridge_holds_after_provider,
            "bridge retains its effect after provider drop"
        );
        assert!(
            held,
            "Started is not the end of the provider and bridge effect"
        );
        assert_eq!(
            provider,
            (true, true),
            "actual provider closure has named worker capability"
        );
        tokio::time::timeout(std::time::Duration::from_secs(20), closing.drain())
            .await
            .unwrap();
        let remaining = crate::services::discord::inflight::load_inflight_state(
            &shared.provider,
            channel.get(),
        );
        assert!(remaining.is_none(), "actual cleanup row: {remaining:?}");
        let mailbox = shared.mailbox_peek(channel).unwrap().snapshot().await;
        assert!(
            mailbox.cancel_token.is_none(),
            "actual finalizer releases the preclose mailbox"
        );
        assert!(mailbox.active_user_message_id.is_none());
        assert!(!live_bridge::is_live(&shared.provider, channel.get()));
        assert!(mock.unhandled.lock().unwrap().is_empty());
        shared.mailboxes.remove_fixture_for_test(channel);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn c1b_headless_off_and_preclose_empty_prompt_keep_original_error_and_drain() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let mock = discord_mock::DiscordMockState::new();
        let (proxy, gateway, server) = discord_mock::start(mock.clone()).await;
        let ctx = discord_mock::serenity_context(proxy, gateway).await;
        let channel = ChannelId::new(6_325_524);
        let off = start_reserved_headless_turn_with_owner(
            &ctx,
            channel,
            " ",
            "owner",
            UserId::new(7),
            &shared,
            "",
            None,
            None,
            None,
            None,
            None,
            reserve_headless_turn(),
        )
        .await;
        assert_eq!(
            off,
            Err(HeadlessTurnStartError::Internal(
                "prompt is required".into()
            ))
        );
        assert!(fence::lookup(&shared.provider, channel.get()).is_none());
        let gate = fence::Gate::protect(shared.provider.clone(), channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let permit = gate.admit().unwrap();
        let closing = gate.close().unwrap();
        let protected = fence::effect::scope(
            Some(permit),
            start_reserved_headless_turn_with_owner(
                &ctx,
                channel,
                " ",
                "owner",
                UserId::new(7),
                &shared,
                "",
                None,
                None,
                None,
                None,
                None,
                reserve_headless_turn(),
            ),
        )
        .await;
        assert_eq!(protected, off);
        closing.drain().await;
        assert!(fence::effect::current().is_none());
        assert!(mock.unhandled.lock().unwrap().is_empty());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
