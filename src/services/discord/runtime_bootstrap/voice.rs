use super::*;

pub(super) fn voice_auto_join_provider_map(
    cfg: &crate::config::Config,
) -> std::collections::HashMap<String, (String, Option<String>)> {
    let mut map = std::collections::HashMap::new();
    for agent in &cfg.agents {
        for (slot_provider, channel) in agent.channels.iter() {
            let Some(channel) = channel else { continue };
            let Some(channel_id) = channel.channel_id() else {
                continue;
            };
            let provider = channel
                .provider()
                .unwrap_or_else(|| slot_provider.to_string());
            map.insert(channel_id.to_string(), (provider, None));
        }
        if let Some(voice_channel_id) = agent
            .voice
            .channel_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let provider = agent
                .voice
                .foreground
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(agent.provider.as_str());
            map.insert(
                voice_channel_id.to_string(),
                (provider.to_string(), Some(agent.id.clone())),
            );
        }
    }
    map
}

/// #2274: rehydrate the process-local voice-background handoff store from the
/// durable PG side table. Best-effort — a PG error is logged and the
/// terminal-delivery path falls back to a per-call `load_handoff_durable`
/// lookup. Runs early in run_bot, before upload cleanup and SharedData build.
pub(super) async fn run_bot_rehydrate_voice_handoffs(pg_pool: &Option<sqlx::PgPool>) {
    if let Some(pool) = pg_pool.as_ref() {
        match crate::voice::announce_meta::rehydrate_handoffs_from_pg(pool).await {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(
                        rehydrated = count,
                        "voice_background_handoff_meta rehydrated from durable PG store"
                    );
                } else {
                    tracing::debug!("voice_background_handoff_meta rehydrate found no live rows");
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "voice_background_handoff_meta rehydrate failed; terminal-delivery will fall back to per-call durable load"
                );
            }
        }
    }
}

/// Build the voice receive hook (when barge-in is enabled), construct the
/// `VoiceReceiver`, and spawn the barge-in sensitivity-TTL-reset and progress
/// workers. Returns the `VoiceReceiver` so run_bot can hand it to the poise
/// framework setup. Runs after SharedData is built and before the intake
/// worker spawn — order preserved.
pub(super) fn run_bot_init_voice_workers(
    voice_config: &crate::voice::VoiceConfig,
    voice_barge_in: &Arc<voice_barge_in::VoiceBargeInRuntime>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> crate::voice::VoiceReceiver {
    let voice_hook: Option<Arc<dyn crate::voice::VoiceReceiveHook>> =
        voice_barge_in.enabled().then(|| {
            Arc::new(voice_barge_in::DiscordVoiceBargeInHook::new(
                voice_barge_in.clone(),
                shared.clone(),
                provider.clone(),
            )) as Arc<dyn crate::voice::VoiceReceiveHook>
        });
    let voice_receiver =
        crate::voice::VoiceReceiver::from_voice_config_with_hook(voice_config, voice_hook);
    voice_barge_in.spawn_sensitivity_ttl_reset(shared.restart.shutting_down.clone());
    voice_barge_in.spawn_progress_worker(shared.clone(), shared.restart.shutting_down.clone());
    voice_receiver
}

/// Pure bootstrap gating for the voice runtime (#4234 r1), unit-testable
/// without a serenity context.
///
/// The rejoin supervisor must cover BOTH documented join entry paths — config
/// auto-join AND an operator `/voice join`. A manual join registers the same
/// `DriverDisconnect` lifecycle handler and occupancy entry (commands/voice.rs)
/// as auto-join, so gating the supervisor behind a non-empty auto-join list
/// silently dropped rejoin requests in manual-join-only configs: the lifecycle
/// router found no sender and logged "not scheduled". The supervisor therefore
/// starts whenever voice is enabled (both manual-join gates require
/// `voice.enabled`, so an enabled runtime is exactly the set of runtimes that
/// can ever join); auto-join additionally requires configured targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct VoiceBootstrapPlan {
    pub(super) start_rejoin_supervisor: bool,
    pub(super) schedule_auto_join: bool,
}

pub(super) fn voice_bootstrap_plan(
    voice_enabled: bool,
    auto_join_target_count: usize,
) -> VoiceBootstrapPlan {
    VoiceBootstrapPlan {
        start_rejoin_supervisor: voice_enabled,
        schedule_auto_join: voice_enabled && auto_join_target_count > 0,
    }
}

/// Start the rejoin supervisor (whenever voice is enabled) and auto-join
/// configured voice channels (leader-only). The gating lives inside
/// (`voice_bootstrap_plan`) so the call site is unconditional. Async because it
/// reads `shared_clone.settings` before spawning.
pub(super) async fn run_bot_spawn_voice_auto_join(
    ctx: &serenity::Context,
    voice_config_for_setup: &crate::voice::VoiceConfig,
    voice_receiver_for_setup: &crate::voice::VoiceReceiver,
    shared_clone: &Arc<SharedData>,
    provider_for_setup: &ProviderKind,
) {
    let plan = voice_bootstrap_plan(
        voice_config_for_setup.enabled,
        voice_config_for_setup
            .auto_join_channel_ids_with_lobby()
            .len(),
    );
    if plan.start_rejoin_supervisor {
        // #4235: bring up the per-provider rejoin supervisor before any join so
        // a DriverDisconnect fired during the very first join already has a
        // registered router sender to route to. #4234 r1: started for every
        // voice-enabled runtime — not only when auto-join targets exist — so a
        // manual /voice join in an auto-join-less config gets the same
        // disconnect→rejoin coverage (see voice_bootstrap_plan).
        super::super::voice_lifecycle::spawn_voice_rejoin_supervisor(
            ctx.clone(),
            voice_receiver_for_setup.clone(),
            shared_clone.voice_barge_in.clone(),
            provider_for_setup.clone(),
            shared_clone.restart.shutting_down.clone(),
        );
    }
    if plan.schedule_auto_join {
        let ctx_for_voice = ctx.clone();
        let receiver_for_voice = voice_receiver_for_setup.clone();
        let config_for_voice = voice_config_for_setup.clone();
        let barge_in_for_voice = shared_clone.voice_barge_in.clone();
        let pairings_for_voice = shared_clone.voice_pairings.clone();
        let provider_for_voice = provider_for_setup.clone();
        let agent_for_voice = {
            let settings = shared_clone.settings.read().await;
            settings.agent.clone()
        };
        // #2054 v7: agent voice binding은 channel_id → provider+agent
        // 매핑을 build 해서 같은 provider의 다른 에이전트 봇까지
        // 같은 voice 채널에 진입하는 중복 STT/TTS를 차단한다.
        let cfg = crate::config::load_graceful();
        let channel_provider_map = voice_auto_join_provider_map(&cfg);
        tokio::spawn(async move {
            commands::auto_join_voice_channels(
                ctx_for_voice,
                receiver_for_voice,
                config_for_voice,
                barge_in_for_voice,
                pairings_for_voice,
                provider_for_voice,
                agent_for_voice,
                channel_provider_map,
            )
            .await;
        });
    } else {
        // #4234: the dormant path was previously silent, so a release node with
        // voice unconfigured produced zero auto-join log lines — leaving the
        // "why is there no auto-join?" question unanswerable from logs alone.
        // One INFO line makes the dormant state observable.
        tracing::info!(
            enabled = voice_config_for_setup.enabled,
            target_count = voice_config_for_setup
                .auto_join_channel_ids_with_lobby()
                .len(),
            "voice auto-join not scheduled: voice disabled or no auto-join targets"
        );
    }
}

#[cfg(test)]
mod voice_bootstrap_tests {
    use super::*;
    use crate::services::discord::voice_lifecycle;

    // #4234 r1 regression: a manual-join-only config (voice enabled, zero
    // auto-join targets) previously never started the rejoin supervisor — the
    // spawn sat inside the auto-join branch — so a DriverDisconnect after an
    // operator /voice join found no router sender and the rejoin request was
    // dropped ("not scheduled"). The supervisor gate must be voice enablement
    // alone, and once the supervisor registers its router sender the request
    // is routed instead of dropped.
    #[test]
    fn manual_join_only_config_starts_rejoin_supervisor_and_routes_rejoin() {
        // Exact defect condition: enabled, no auto-join targets.
        let plan = voice_bootstrap_plan(true, 0);
        assert!(
            plan.start_rejoin_supervisor,
            "voice-enabled runtime must start the rejoin supervisor even with zero auto-join targets"
        );
        assert!(
            !plan.schedule_auto_join,
            "zero auto-join targets -> nothing to auto-join"
        );
        // Full gating matrix around the defect case: disabled never starts
        // anything (no join path exists); enabled + targets starts both.
        assert_eq!(
            voice_bootstrap_plan(false, 0),
            VoiceBootstrapPlan {
                start_rejoin_supervisor: false,
                schedule_auto_join: false,
            }
        );
        assert_eq!(
            voice_bootstrap_plan(false, 3),
            VoiceBootstrapPlan {
                start_rejoin_supervisor: false,
                schedule_auto_join: false,
            }
        );
        assert_eq!(
            voice_bootstrap_plan(true, 2),
            VoiceBootstrapPlan {
                start_rejoin_supervisor: true,
                schedule_auto_join: true,
            }
        );

        // The supervisor's first act (spawn_voice_rejoin_supervisor) is
        // registering the provider's router sender; replicate that and assert
        // the previously-dropped rejoin request is now routed — the
        // channel/guard-level contract the manual-join fix restores.
        let provider = "test-manual-join-r1-0xC0FFEE";
        let mut rx = voice_lifecycle::register_lifecycle_router(provider);
        let routed = voice_lifecycle::dispatch_reconnect(voice_lifecycle::ReconnectRequest {
            guild_id: serenity::GuildId::new(0xC0FFEE_0000_0E01),
            channel_id: serenity::ChannelId::new(0xC0FFEE_0000_0E02),
            control_channel_id: serenity::ChannelId::new(0xC0FFEE_0000_0E03),
            provider: provider.to_string(),
        });
        assert!(
            routed,
            "rejoin request must be routed once the supervisor registered its sender"
        );
        let received = rx
            .try_recv()
            .expect("supervisor receiver should get the rejoin request");
        assert_eq!(received.guild_id.get(), 0xC0FFEE_0000_0E01);
        assert_eq!(received.channel_id.get(), 0xC0FFEE_0000_0E02);
        assert_eq!(received.control_channel_id.get(), 0xC0FFEE_0000_0E03);
        voice_lifecycle::remove_lifecycle_router_for_tests(provider);
    }
}
