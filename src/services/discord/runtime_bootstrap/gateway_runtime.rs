use super::*;

#[cfg(all(test, unix))]
mod slash_command_tests;

/// Start the leader gateway runtime after health registration, restart-marker
/// fencing, and optional intake-worker startup. This final `run_bot` tail owns
/// framework/client construction, gateway-lease keepalive and the gateway
/// backend event loop, in that order.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bot_start_gateway_runtime(
    token: &str,
    provider: ProviderKind,
    provider_for_error: ProviderKind,
    provider_for_framework: ProviderKind,
    startup_reconcile_remaining: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: Arc<std::sync::atomic::AtomicBool>,
    health_registry: Arc<health::HealthRegistry>,
    startup_reconcile_remaining_for_client_start: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started_for_client_start: Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_client_start: Arc<health::HealthRegistry>,
    api_port: u16,
    shared: Arc<SharedData>,
    gateway_lease: Option<crate::db::postgres::AdvisoryLockLease>,
    gateway_waiter: Option<GatewayWaiterGuard>,
    handback_breaker: GatewayHandbackBreaker,
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

    let slash_commands = run_bot_build_slash_commands();
    let framework = poise::Framework::builder()
        .options(run_bot_framework_options(slash_commands))
        .setup(move |ctx, _ready, framework| {
            let shared_for_migrate = shared_clone.clone();
            let health_registry_for_setup = health_registry.clone();
            let provider_for_setup = provider_for_framework.clone();
            let token_for_ready = token_owned.clone();
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
                startup_reconcile_remaining,
                startup_doctor_started,
                api_port,
            ))
        })
        .build();

    let intents = discord_gateway_intents();

    let client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    // This path is reached only after gateway-role resolution. Standby and
    // indeterminate runtimes return before framework construction, so the
    // refresh task remains owned by the active gateway lifecycle.
    let model_catalog_refresh_task = model_catalog::spawn_claude_model_catalog_refresh(&provider);
    let gateway_lease_task = gateway_lease.map(|lease| {
        run_bot_spawn_gateway_lease_keepalive(
            lease,
            &shared,
            &provider,
            settings::discord_token_hash(token),
            client.shard_manager.clone(),
            handback_breaker,
        )
    });

    run_bot_run_gateway_backend(
        client,
        &provider_for_error,
        gateway_waiter,
        gateway_lease_task,
        model_catalog_refresh_task,
        startup_reconcile_remaining_for_client_start,
        startup_doctor_started_for_client_start,
        health_registry_for_client_start,
        api_port,
    )
    .await;
}

/// The production framework options: slash commands, their channel guard and the event handler.
pub(in crate::services::discord) fn run_bot_framework_options(
    commands: Vec<poise::Command<Data, Error>>,
) -> poise::FrameworkOptions<Data, Error> {
    poise::FrameworkOptions {
        commands,
        command_check: Some(|ctx| {
            Box::pin(async move {
                let autocomplete = matches!(ctx, poise::Context::Application(app)
                    if app.interaction_type == poise::CommandInteractionType::Autocomplete);
                let routing = async {
                    let settings_snapshot = ctx.data().shared.settings.read().await.clone();
                    provider_handles_channel(
                        ctx.serenity_context(),
                        &ctx.data().provider,
                        &settings_snapshot,
                        ctx.channel_id(),
                    )
                    .await
                };
                let allowed = if autocomplete {
                    tokio::time::timeout(std::time::Duration::from_millis(500), routing)
                        .await
                        .unwrap_or(false)
                } else {
                    routing.await
                };
                if !allowed {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏭ CMD-GUARD: skipping /{} in channel {} for provider {}",
                        ctx.command().name,
                        ctx.channel_id(),
                        ctx.data().provider.as_str()
                    );
                    if let poise::Context::Application(app) = ctx {
                        match app.interaction_type {
                            poise::CommandInteractionType::Autocomplete => {
                                app.interaction
                                    .create_response(
                                        ctx.serenity_context(),
                                        serenity::CreateInteractionResponse::Autocomplete(
                                            serenity::CreateAutocompleteResponse::new(),
                                        ),
                                    )
                                    .await?;
                            }
                            poise::CommandInteractionType::Command => {
                                let guidance = command_guard_guidance(ctx).await;
                                ctx.send(
                                    poise::CreateReply::default()
                                        .content(guidance)
                                        .ephemeral(true),
                                )
                                .await?;
                            }
                        }
                    }
                }
                Ok(allowed)
            })
        }),
        event_handler: |framework, event| {
            Box::pin(handle_event(
                framework.serenity_context,
                event,
                framework.user_data,
            ))
        },
        ..Default::default()
    }
}

async fn command_guard_guidance(ctx: Context<'_>) -> String {
    let target = tokio::time::timeout(std::time::Duration::from_millis(500), async {
        let channel = ctx.channel_id().get().to_string();
        if let Some(fallback) = crate::services::agent_recovery::fallback_provider(&channel)
            && matches!(
                crate::services::agent_recovery::channel_recovery_intake(&fallback, &channel).await,
                Some(crate::services::agent_recovery::RecoveryIntake::Allow)
            )
        {
            return Some(fallback);
        }
        let http = &ctx.serenity_context().http;
        let (binding_channel, name) = match resolve_thread_parent(http, ctx.channel_id()).await {
            Some(parent) => parent,
            None => (
                ctx.channel_id(),
                session_runtime::resolve_channel_name(http, ctx.channel_id())
                    .await
                    .ok()
                    .flatten(),
            ),
        };
        let binding = settings::resolve_role_binding(binding_channel, name.as_deref());
        for candidate in crate::services::provider::provider_registry()
            .iter()
            .filter_map(|entry| entry.kind())
        {
            if candidate != ctx.data().provider
                && settings::channel_supports_provider(
                    &candidate,
                    name.as_deref(),
                    false,
                    binding.as_ref(),
                )
                && provider_handles_channel(
                    ctx.serenity_context(),
                    &candidate,
                    &DiscordBotSettings::default(),
                    ctx.channel_id(),
                )
                .await
            {
                return Some(candidate);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    let command = ctx
        .parent_commands()
        .iter()
        .map(|command| command.name.as_ref())
        .chain(std::iter::once(ctx.command().name.as_ref()))
        .collect::<Vec<_>>()
        .join(" ");
    match target {
        Some(provider) => format!(
            "이 채널에서는 `{}` 봇의 `/{command}` 명령을 선택해 주세요.",
            provider.as_str()
        ),
        None => format!(
            "이 봇은 이 채널의 명령을 처리할 수 없어요. 이 채널 담당 봇의 `/{command}` 명령을 선택해 주세요."
        ),
    }
}
