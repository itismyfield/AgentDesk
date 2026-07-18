use super::*;

/// Start the leader gateway runtime after health registration, restart-marker
/// fencing, and optional intake-worker startup. This final `run_bot` tail owns
/// framework/client construction, gateway-lease keepalive, the SIGTERM handler,
/// and the gateway backend event loop, in that order.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bot_start_gateway_runtime(
    token: &str,
    provider: ProviderKind,
    provider_for_error: ProviderKind,
    provider_for_framework: ProviderKind,
    provider_for_shutdown: ProviderKind,
    startup_reconcile_remaining: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: Arc<std::sync::atomic::AtomicBool>,
    health_registry: Arc<health::HealthRegistry>,
    startup_reconcile_remaining_for_client_start: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started_for_client_start: Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_client_start: Arc<health::HealthRegistry>,
    api_port: u16,
    shared: Arc<SharedData>,
    voice_config: crate::voice::VoiceConfig,
    voice_receiver: crate::voice::VoiceReceiver,
    gateway_lease: Option<crate::db::postgres::AdvisoryLockLease>,
    restored_model_overrides: &[(ChannelId, String)],
    restored_fast_mode_channels: &[ChannelId],
) {
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔑 dcserver generation: {}",
            shared.restart.current_generation
        );
        if !restored_model_overrides.is_empty() {
            tracing::info!(
                "  [{ts}] 🧩 restored model overrides: {} channel(s)",
                restored_model_overrides.len()
            );
        }
        if !restored_fast_mode_channels.is_empty() {
            tracing::info!(
                "  [{ts}] ⚡ restored fast mode channels: {} channel(s)",
                restored_fast_mode_channels.len()
            );
        }
    }

    let token_owned = token.to_string();
    let shared_clone = shared.clone();
    let voice_config_for_setup = voice_config.clone();
    let voice_receiver_for_setup = voice_receiver.clone();

    let slash_commands = run_bot_build_slash_commands();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: slash_commands,
            command_check: Some(|ctx| {
                Box::pin(async move {
                    let settings_snapshot = { ctx.data().shared.settings.read().await.clone() };
                    let allowed = provider_handles_channel(
                        ctx.serenity_context(),
                        &ctx.data().provider,
                        &settings_snapshot,
                        ctx.channel_id(),
                    )
                    .await;
                    if !allowed {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⏭ CMD-GUARD: skipping /{} in channel {} for provider {}",
                            ctx.command().name,
                            ctx.channel_id(),
                            ctx.data().provider.as_str()
                        );
                    }
                    Ok(allowed)
                })
            }),
            event_handler: |ctx, event, _framework, data| Box::pin(handle_event(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let shared_for_migrate = shared_clone.clone();
            let health_registry_for_setup = health_registry.clone();
            let provider_for_setup = provider_for_framework.clone();
            let token_for_ready = token_owned.clone();
            let voice_config_for_setup = voice_config_for_setup.clone();
            let voice_receiver_for_setup = voice_receiver_for_setup.clone();
            Box::pin(run_bot_framework_setup(
                ctx,
                _ready,
                framework,
                shared_for_migrate,
                shared_clone,
                health_registry_for_setup,
                provider_for_setup,
                token_for_ready,
                token_owned,
                voice_config_for_setup,
                voice_receiver_for_setup,
                startup_reconcile_remaining,
                startup_doctor_started,
                api_port,
            ))
        })
        .build();

    let intents = discord_gateway_intents();

    let client = commands::register_songbird(serenity::ClientBuilder::new(token, intents))
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    let gateway_lease_task = gateway_lease.map(|lease| {
        run_bot_spawn_gateway_lease_keepalive(
            lease,
            &shared,
            &provider,
            settings::discord_token_hash(token),
            client.shard_manager.clone(),
        )
    });

    // Graceful shutdown: on SIGTERM, persist queue/inflight/last_message state
    // and quick-exit. tmux/TUI processes survive — the next dcserver instance
    // rehydrates the channel bindings (see rehydrate_existing_claude_tui_bindings;
    // polled every CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL ≈ 5s) and resumes transcript
    // tailing from the persisted last_offset.
    run_bot_spawn_sigterm_handler(&shared, provider_for_shutdown);

    run_bot_run_gateway_backend(
        client,
        &provider_for_error,
        gateway_lease_task,
        startup_reconcile_remaining_for_client_start,
        startup_doctor_started_for_client_start,
        health_registry_for_client_start,
        api_port,
    )
    .await;
}
