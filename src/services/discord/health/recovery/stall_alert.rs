//! Non-destructive stall-watchdog paging.
//!
//! Branch 4 of the watchdog may alert an operator, but it never owns turn
//! cleanup. This module keeps the alert's Discord routing identity aligned with
//! the active provider session and makes producer-liveness suppression explicit.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::ProviderKind;

use super::super::stall_liveness;

/// Stable reason identity for cooldown dedupe in `message_outbox`.
const STALL_WATCHDOG_MENTION_REASON_CODE: &str = "stall_watchdog_suspected_stall";

/// A persistently suspect session pages at most once per 30 minutes.
const STALL_WATCHDOG_MENTION_COOLDOWN_SECS: i64 = 1800;

fn session_key_tmux_for_provider(session_key: &str, provider: &ProviderKind) -> Option<String> {
    let Some(identity) = discord::session_identity::SessionIdentity::parse(session_key) else {
        return None;
    };
    crate::services::provider::parse_provider_and_channel_from_tmux_name(&identity.tmux_name)
        .filter(|(parsed, _)| parsed == provider)
        .map(|_| identity.tmux_name)
}

fn namespaced_key_for_tmux(
    shared: &SharedData,
    provider: &ProviderKind,
    tmux_name: &str,
) -> Option<String> {
    crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)
        .filter(|(parsed, _)| parsed == provider)
        .map(|_| {
            discord::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                provider,
                tmux_name,
            )
        })
}

/// Resolve the same provider/session identity that normal Discord delivery uses.
/// A persisted inflight key is accepted only when its tmux identity matches the
/// exact inflight tmux or channel binding; legacy/corrupt rows fall through to
/// those trusted identities. Synthetic watchdog keys are deliberately forbidden
/// because they hide DM provider ownership from the outbox worker.
async fn canonical_alert_session_key(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight: Option<&discord::inflight::InflightTurnState>,
) -> Option<String> {
    let tmux_fallback = inflight
        .and_then(|state| state.tmux_session_name.as_deref())
        .and_then(|tmux_name| {
            namespaced_key_for_tmux(shared, provider, tmux_name)
                .map(|key| (tmux_name.to_string(), key))
        });
    let channel_fallback =
        discord::adk_session::build_adk_session_key(shared, channel_id, provider)
            .await
            .filter(|key| session_key_tmux_for_provider(key, provider).is_some());
    let expected_tmux = tmux_fallback
        .as_ref()
        .map(|(tmux_name, _)| tmux_name.clone())
        .or_else(|| {
            channel_fallback
                .as_deref()
                .and_then(|key| session_key_tmux_for_provider(key, provider))
        });

    if let Some(session_key) = inflight
        .and_then(|state| state.session_key.as_deref())
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .filter(|key| {
            expected_tmux.as_deref() == session_key_tmux_for_provider(key, provider).as_deref()
        })
    {
        return Some(session_key.to_string());
    }

    if let Some((_, session_key)) = tmux_fallback {
        return Some(session_key);
    }

    channel_fallback
}

fn owner_mention(inflight: Option<&discord::inflight::InflightTurnState>) -> String {
    inflight
        .map(|state| state.request_owner_user_id)
        .filter(|owner| {
            *owner != 0 && *owner != discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
        })
        .map(|owner| format!(" <@{owner}>"))
        .unwrap_or_default()
}

/// Positive producer evidence is authoritative for page suppression until the
/// existing finite absolute backstop. A genuinely stalled decision (no evidence)
/// and a producer that crossed the backstop still page; neither path cleans.
pub(super) fn should_page_suspected_stall(
    decision: Option<&stall_liveness::StallWatchdogLivenessDecision>,
) -> bool {
    !decision.is_some_and(|decision| {
        decision.should_defer()
            && decision
                .evidence
                .has_positive_liveness(stall_liveness::STALL_WATCHDOG_POSITIVE_LIVENESS_SECS)
    })
}

/// Queue a rate-limited operator alert without modifying any turn state.
pub(super) async fn notify_suspected_stall_without_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    inflight: Option<&discord::inflight::InflightTurnState>,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        tracing::warn!(
            "  [stall-watchdog] #4460 suspected stall on channel {} (provider={}) but no pg_pool — skipping mention (never terminating)",
            channel_id,
            provider.as_str(),
        );
        return;
    };

    let mention = owner_mention(inflight);
    let content = format!(
        "⚠️ 스톨 의심: 이 세션이 오래 응답이 없어 보입니다{mention}. 워치독은 더 이상 자동 종료하지 않습니다 — 실제로 멈췄다면 취소해 주시고, 정상 작업 중이면 무시하세요. (채널 {channel_id})"
    );
    // `channel:` is the established manual-outbound target contract. The
    // worker combines it with a provider DM session key to select the Claude or
    // Codex bot; public sessions intentionally keep the configured notify bot.
    let target = format!("channel:{}", channel_id.get());
    let session_key = canonical_alert_session_key(shared, provider, channel_id, inflight).await;
    match crate::services::message_outbox::enqueue_outbox_pg_with_ttl(
        pool,
        crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: &content,
            bot: "notify",
            source: "stall_watchdog",
            reason_code: Some(STALL_WATCHDOG_MENTION_REASON_CODE),
            session_key: session_key.as_deref(),
        },
        STALL_WATCHDOG_MENTION_COOLDOWN_SECS,
    )
    .await
    {
        Ok(true) => tracing::warn!(
            "  [stall-watchdog] #4460 suspected stall alert queued on channel {} (provider={}) WITHOUT force-clean",
            channel_id,
            provider.as_str(),
        ),
        Ok(false) => tracing::debug!(
            "  [stall-watchdog] #4460 suspected stall alert suppressed by cooldown ({}s) on channel {}",
            STALL_WATCHDOG_MENTION_COOLDOWN_SECS,
            channel_id,
        ),
        Err(error) => tracing::warn!(
            "  [stall-watchdog] #4460 suspected stall alert enqueue failed on channel {}: {error}",
            channel_id,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::health::stall_liveness::{
        StallWatchdogLivenessAction, StallWatchdogLivenessDecision, StallWatchdogLivenessEvidence,
    };
    use crate::services::discord::inflight::InflightTurnState;

    fn liveness_decision(
        action: StallWatchdogLivenessAction,
        positive: bool,
    ) -> StallWatchdogLivenessDecision {
        let mut evidence = StallWatchdogLivenessEvidence::default();
        if positive {
            evidence.runtime_activity_age_secs = Some(0);
        }
        StallWatchdogLivenessDecision {
            action,
            evidence,
            max_deferrals: stall_liveness::STALL_WATCHDOG_MAX_LIVENESS_DEFERRALS,
        }
    }

    fn inflight_fixture(
        provider: &ProviderKind,
        channel_id: u64,
        owner: u64,
        tmux_name: &str,
        persist_session_key: bool,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel_id,
            None,
            owner,
            channel_id + 100,
            channel_id + 200,
            "stall alert fixture".to_string(),
            Some(format!("provider-session-{channel_id}")),
            Some(tmux_name.to_string()),
            None,
            None,
            0,
        );
        if persist_session_key {
            state.session_key = Some(discord::adk_session::build_namespaced_session_key(
                "test-token-hash",
                provider,
                tmux_name,
            ));
        }
        state
    }

    #[test]
    fn producer_liveness_suppresses_only_pre_backstop_page() {
        let live = liveness_decision(
            StallWatchdogLivenessAction::Defer { deferral_count: 0 },
            true,
        );
        assert!(!should_page_suspected_stall(Some(&live)));

        let stalled = liveness_decision(StallWatchdogLivenessAction::ProceedNoEvidence, false);
        assert!(should_page_suspected_stall(Some(&stalled)));

        let at_backstop = liveness_decision(
            StallWatchdogLivenessAction::ProceedAfterAbsoluteBackstop {
                age_secs: stall_liveness::STALL_WATCHDOG_ABSOLUTE_BACKSTOP_SECS,
                deferral_count: 0,
            },
            true,
        );
        assert!(should_page_suspected_stall(Some(&at_backstop)));
    }

    #[test]
    fn owner_zero_and_tui_sentinel_never_render_mentions() {
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let provider = ProviderKind::Codex;
        let zero = inflight_fixture(&provider, 44_600, 0, "AgentDesk-codex-adk-cc", true);
        let sentinel = inflight_fixture(
            &provider,
            44_601,
            discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
            "AgentDesk-codex-adk-cc",
            true,
        );
        let real = inflight_fixture(
            &provider,
            44_602,
            343_742_347,
            "AgentDesk-codex-adk-cc",
            true,
        );

        assert_eq!(owner_mention(Some(&zero)), "");
        assert_eq!(owner_mention(Some(&sentinel)), "");
        assert_eq!(owner_mention(Some(&real)), " <@343742347>");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pg_outbox_routes_dm_alerts_and_dedupes_without_synthetic_owner_mentions() {
        let Some(pg_db) = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
            "agentdesk_stall_watchdog_alert_authority",
            "stall watchdog alert authority tests",
        )
        .await
        else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;
        let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("runtime root tempdir");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );
        let shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));

        #[derive(Clone, Copy)]
        enum SessionFixture {
            Persisted,
            TmuxFallback,
            CorruptWrongProvider,
            CorruptSameProviderGuild,
            CorruptSameProviderDm,
            ChannelBindingFallback,
        }
        struct Scenario {
            channel_id: u64,
            provider: ProviderKind,
            owner: u64,
            tmux_name: &'static str,
            session_fixture: SessionFixture,
            expected_bot: &'static str,
            expected_mention: Option<u64>,
        }
        let scenarios = [
            Scenario {
                channel_id: 1_479_662_682_909_966_490,
                provider: ProviderKind::Claude,
                owner: 343_742_347_365_974_026,
                tmux_name: "AgentDesk-claude-dm-343742347365974026",
                // Exercise the legacy-row fallback from exact tmux identity.
                session_fixture: SessionFixture::TmuxFallback,
                expected_bot: "claude",
                expected_mention: Some(343_742_347_365_974_026),
            },
            Scenario {
                channel_id: 1_479_662_682_909_966_491,
                provider: ProviderKind::Codex,
                owner: 343_742_347_365_974_026,
                tmux_name: "AgentDesk-codex-dm-343742347365974026",
                session_fixture: SessionFixture::Persisted,
                expected_bot: "codex",
                expected_mention: Some(343_742_347_365_974_026),
            },
            Scenario {
                channel_id: 1_504_455_726_595_051_591,
                provider: ProviderKind::Codex,
                owner: 343_742_347_365_974_026,
                tmux_name: "AgentDesk-codex-adk-cc",
                session_fixture: SessionFixture::Persisted,
                expected_bot: "notify",
                expected_mention: Some(343_742_347_365_974_026),
            },
            Scenario {
                channel_id: 1_504_455_726_595_051_592,
                provider: ProviderKind::Claude,
                owner: 0,
                tmux_name: "AgentDesk-claude-adk-cc",
                session_fixture: SessionFixture::Persisted,
                expected_bot: "notify",
                expected_mention: None,
            },
            Scenario {
                channel_id: 1_504_455_726_595_051_593,
                provider: ProviderKind::Codex,
                owner: discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
                tmux_name: "AgentDesk-codex-adk-cc",
                session_fixture: SessionFixture::Persisted,
                expected_bot: "notify",
                expected_mention: None,
            },
            Scenario {
                channel_id: 1_479_662_682_909_966_494,
                provider: ProviderKind::Codex,
                owner: 343_742_347_365_974_026,
                tmux_name: "AgentDesk-codex-dm-343742347365974026",
                // A corrupt/wrong-provider persisted key must fall through to
                // the exact tmux identity rather than selecting the wrong bot.
                session_fixture: SessionFixture::CorruptWrongProvider,
                expected_bot: "codex",
                expected_mention: Some(343_742_347_365_974_026),
            },
            Scenario {
                channel_id: 1_479_662_682_909_966_496,
                provider: ProviderKind::Codex,
                owner: 343_742_347_365_974_026,
                tmux_name: "AgentDesk-codex-dm-343742347365974026",
                // Provider equality alone is insufficient: a stale public
                // key for the same provider must not downgrade a DM alert.
                session_fixture: SessionFixture::CorruptSameProviderGuild,
                expected_bot: "codex",
                expected_mention: Some(343_742_347_365_974_026),
            },
            Scenario {
                channel_id: 1_504_455_726_595_051_596,
                provider: ProviderKind::Codex,
                owner: 343_742_347_365_974_026,
                tmux_name: "AgentDesk-codex-adk-cc",
                // Conversely, a stale same-provider DM key must not upgrade a
                // public informational alert to the provider bot.
                session_fixture: SessionFixture::CorruptSameProviderDm,
                expected_bot: "notify",
                expected_mention: Some(343_742_347_365_974_026),
            },
            Scenario {
                channel_id: 1_479_662_682_909_966_495,
                provider: ProviderKind::Claude,
                owner: 343_742_347_365_974_026,
                tmux_name: "AgentDesk-claude-dm-343742347365974026",
                // Legacy/corrupt rows may have neither persisted provider
                // identity. The runtime channel binding remains authoritative.
                session_fixture: SessionFixture::ChannelBindingFallback,
                expected_bot: "claude",
                expected_mention: Some(343_742_347_365_974_026),
            },
        ];

        for scenario in &scenarios {
            let persist_session_key = matches!(scenario.session_fixture, SessionFixture::Persisted);
            let mut state = inflight_fixture(
                &scenario.provider,
                scenario.channel_id,
                scenario.owner,
                scenario.tmux_name,
                persist_session_key,
            );
            match scenario.session_fixture {
                SessionFixture::Persisted | SessionFixture::TmuxFallback => {}
                SessionFixture::CorruptWrongProvider => {
                    state.session_key = Some(
                        "claude/wrong/host:AgentDesk-claude-dm-343742347365974026".to_string(),
                    );
                }
                SessionFixture::CorruptSameProviderGuild => {
                    state.session_key = Some("codex/wrong/host:AgentDesk-codex-adk-cc".to_string());
                }
                SessionFixture::CorruptSameProviderDm => {
                    state.session_key =
                        Some("codex/wrong/host:AgentDesk-codex-dm-343742347365974026".to_string());
                }
                SessionFixture::ChannelBindingFallback => {
                    state.session_key = None;
                    state.tmux_session_name = None;
                    shared.core.lock().await.sessions.insert(
                        ChannelId::new(scenario.channel_id),
                        discord::DiscordSession {
                            session_id: Some("channel-binding-fallback".to_string()),
                            memento_context_loaded: true,
                            memento_reflected: false,
                            current_path: None,
                            history: Vec::new(),
                            pending_uploads: Vec::new(),
                            cleared: false,
                            remote_profile_name: None,
                            channel_id: Some(scenario.channel_id),
                            channel_name: Some("dm-343742347365974026".to_string()),
                            category_name: None,
                            last_active: tokio::time::Instant::now(),
                            worktree: None,
                            born_generation: discord::runtime_store::load_generation(),
                        },
                    );
                }
            }
            // Two watchdog passes inside the cooldown must create one row.
            notify_suspected_stall_without_cleanup(
                &shared,
                &scenario.provider,
                ChannelId::new(scenario.channel_id),
                Some(&state),
            )
            .await;
            notify_suspected_stall_without_cleanup(
                &shared,
                &scenario.provider,
                ChannelId::new(scenario.channel_id),
                Some(&state),
            )
            .await;
        }

        let rows: Vec<(String, String, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT target, bot, source, reason_code, session_key, content,
                    EXTRACT(EPOCH FROM (dedupe_expires_at - created_at))::BIGINT
               FROM message_outbox
              ORDER BY target",
        )
        .fetch_all(&pool)
        .await
        .expect("load stall watchdog outbox rows");
        assert_eq!(
            rows.len(),
            scenarios.len(),
            "cooldown must dedupe each pair"
        );

        for scenario in &scenarios {
            let target = format!("channel:{}", scenario.channel_id);
            let row = rows
                .iter()
                .find(|row| row.0 == target)
                .expect("scenario row must use canonical channel target");
            assert_eq!(row.1, "notify", "stored bot remains the safe default");
            assert_eq!(row.2, "stall_watchdog");
            assert_eq!(row.3, STALL_WATCHDOG_MENTION_REASON_CODE);
            assert_eq!(row.6, STALL_WATCHDOG_MENTION_COOLDOWN_SECS);
            let delivery_bot = crate::services::message_outbox::delivery_bot_for_target_session(
                &row.0,
                &row.1,
                Some(&row.4),
            );
            assert_eq!(delivery_bot.as_ref(), scenario.expected_bot);
            match scenario.expected_mention {
                Some(owner) => assert!(row.5.contains(&format!("<@{owner}>"))),
                None => assert!(
                    !row.5.contains("<@"),
                    "owner 0/1 is synthetic and must not render a mention: {}",
                    row.5
                ),
            }
        }
    }
}
