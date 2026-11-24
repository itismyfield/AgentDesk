use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::discord::inflight::{InflightTurnState, RelayOwnerKind};
use crate::services::discord::recovery_engine::o_cut_recorder::start;
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::provider::{CancelToken, ProviderKind};

/// Watcher-death cleanup of a turn whose bridge no longer owns the relay returns the mailbox
/// to idle; a bridge-owned turn and a successor episode keep their tokens.
#[tokio::test(flavor = "current_thread")]
async fn watcher_death_handoff_releases_only_the_relinquished_turn_token() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async move {
        let cases = [
            (
                9_633_301,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                false,
            ),
            (
                9_633_302,
                ProviderKind::Codex,
                RelayOwnerKind::Watcher,
                false,
            ),
            (9_633_303, ProviderKind::Claude, RelayOwnerKind::None, false),
            (
                9_633_304,
                ProviderKind::Claude,
                RelayOwnerKind::Watcher,
                true,
            ),
        ];
        for (channel, provider, owner, successor) in cases {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel_id = ChannelId::new(channel);
            let dead = Arc::new(CancelToken::new());
            let active = if successor {
                Arc::new(CancelToken::new())
            } else {
                dead.clone()
            };
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel_id,
                    active.clone(),
                    UserId::new(1),
                    MessageId::new(10),
                )
                .await
            );
            crate::services::discord::increment_global_active(&shared, "test");
            let mut state = InflightTurnState::new(
                provider.clone(),
                channel,
                None,
                1,
                10,
                0,
                "fresh turn".to_string(),
                None,
                Some(format!("AgentDesk-{}-dead-{channel}", provider.as_str())),
                None,
                None,
                0,
            );
            state.turn_nonce = dead.turn_nonce().map(str::to_owned);
            state.set_relay_owner_kind(owner);
            crate::services::discord::inflight::save_inflight_state(&state).unwrap();
            let state = crate::services::discord::inflight::load_inflight_state(&provider, channel)
                .unwrap();

            let recorder = start(channel).await;
            let handled = super::start_restart_handoff_from_state(
                channel_id,
                &recorder.http,
                &shared,
                &provider,
                state,
                "",
            )
            .await;

            assert!(handled, "channel {channel}");
            assert!(
                crate::services::discord::inflight::load_inflight_state(&provider, channel)
                    .is_none()
            );
            let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
            let released = owner != RelayOwnerKind::None && !successor;
            assert_eq!(
                snapshot.cancel_token.is_none(),
                released,
                "channel {channel}: owner={owner:?} successor={successor}"
            );
            assert_eq!(snapshot.active_user_message_id.is_none(), released);
            assert_eq!(
                shared.restart.global_active.load(Ordering::Relaxed),
                usize::from(!released)
            );
            if released {
                assert!(dead.is_completion_cleanup());
            }
        }
    })
    .await;
}

#[path = "tmux_restart_handoff_mailbox_tests/fresh_start_failure_tests.rs"]
mod fresh_start_failure_tests;

// A rowless watcher death wakes a V2 owner's waiting input once, through the owner and never also
// through a legacy kickoff; a front an attempt carries already has its drive and wakes nothing.
#[tokio::test(flavor = "current_thread")]
async fn rowless_watcher_death_wakes_the_v2_owner_waiting_input_once() {
    use crate::services::discord::queue_io::cancel_backstop_test_support as support;
    use crate::services::discord::relay_recovery::tests::orphan_token_finish::v2_attempted_front;
    with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let http = Arc::new(poise::serenity_prelude::Http::new("test-token"));
        // The last row is a dead session whose name says nothing: only the watcher's provider
        // can route its recovery.
        for (channel, waiting, session) in [
            (9_633_401_u64, true, "AgentDesk-codex-v2-rowless-9633401"),
            (9_633_402, false, "AgentDesk-codex-v2-rowless-9633402"),
            (9_633_403, true, "스마트홈-cc-i8119-dead"),
        ] {
            let channel_id = ChannelId::new(channel);
            support::set_owner_wake_for_tests(channel_id, Some(Arc::new(|| {})));
            let completion = support::KickoffCompletion::install(channel_id);
            v2_attempted_front(&shared, &ProviderKind::Codex, channel_id, waiting).await;
            let resume = super::resume_aborted_restart_turn(
                channel_id,
                &http,
                &shared,
                &ProviderKind::Codex,
                session,
                "",
            );
            assert!(!resume.await);
            completion
                .processed(0, std::time::Duration::from_secs(5))
                .await;
            assert_eq!(
                support::owner_wakes(channel_id),
                usize::from(waiting),
                "{channel}"
            );
            support::set_owner_wake_for_tests(channel_id, None);
        }
    })
    .await;
}

/// A watcher death that ends a pipe turn commits its failure with the origin the turn consumed.
#[tokio::test(flavor = "current_thread")]
async fn watcher_death_commits_the_dead_turn_origin_as_failed_pg() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async move {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let (channel, provider) = (9_828_901, ProviderKind::Claude);
        let mut state = InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            1,
            10,
            0,
            "pipe turn".into(),
            None,
            Some("AgentDesk-claude-pipe-dead".into()),
            None,
            None,
            0,
        );
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
        state.input_origin_ids = vec!["dlg:8289:pipe".into()];
        crate::services::discord::inflight::save_inflight_state(&state).unwrap();
        let state =
            crate::services::discord::inflight::load_inflight_state(&provider, channel).unwrap();
        let recorder = start(channel).await;
        let channel_id = ChannelId::new(channel);
        assert!(
            super::start_restart_handoff_from_state(
                channel_id,
                &recorder.http,
                &shared,
                &provider,
                state,
                ""
            )
            .await
        );
        let receipt: (Vec<String>, Option<String>) = sqlx::query_as(
            "SELECT consumed_origin_ids, terminal_outcome FROM turns WHERE turn_id = $1",
        )
        .bind(format!("discord:{channel}:10"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            receipt,
            (vec!["dlg:8289:pipe".into()], Some("failed".into()))
        );
        pool.close().await;
        db.drop().await;
    })
    .await;
}

/// Watcher-death cleanup leaves a carried execution's terminal to its same-id retry: a retry that
/// runs records completed, and one refused while the dead turn still holds the slot fails it once.
#[tokio::test(flavor = "current_thread")]
async fn watcher_death_leaves_a_carried_retry_its_own_terminal_pg() {
    use crate::services::discord::{inflight, receipt_settle};
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async move {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let provider = ProviderKind::Claude;
        for (channel, refused) in [(9_828_902_u64, false), (9_828_903, true)] {
            let (channel_id, message) = (ChannelId::new(channel), MessageId::new(10));
            let dead = Arc::new(CancelToken::new());
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel_id,
                    dead.clone(),
                    UserId::new(1),
                    message,
                )
                .await
            );
            let mut state = InflightTurnState::new(
                provider.clone(),
                channel,
                None,
                1,
                10,
                0,
                "pipe turn".into(),
                None,
                Some(format!("AgentDesk-claude-retry-{channel}")),
                None,
                None,
                0,
            );
            state.turn_nonce = dead.turn_nonce().map(str::to_owned);
            state.set_relay_owner_kind(RelayOwnerKind::Watcher);
            state.input_origin_ids = vec![format!("dlg:8289:{channel}")];
            inflight::save_inflight_state(&state).unwrap();
            let state = inflight::load_inflight_state(&provider, channel).unwrap();
            receipt_settle::carry_retry_origins(Some(&pool), &state).await;
            let recorder = start(channel).await;
            if refused {
                crate::services::discord::turn_bridge::auto_retry_with_history(
                    &recorder.http,
                    &shared,
                    &provider,
                    channel_id,
                    message,
                    "pipe turn",
                )
                .await;
            }
            let (http, row) = (&recorder.http, state.clone());
            assert!(
                super::start_restart_handoff_from_state(
                    channel_id, http, &shared, &provider, row, ""
                )
                .await
            );
            if !refused {
                let mut retry = state.clone();
                retry.input_origin_ids.clear();
                let completed = crate::db::turns::TerminalOutcome::Completed;
                assert!(
                    receipt_settle::record(Some(&pool), &retry, completed, chrono::Utc::now())
                        .await
                );
            }
            let receipt: (Vec<String>, Option<String>) = sqlx::query_as(
                "SELECT consumed_origin_ids, terminal_outcome FROM turns WHERE turn_id = $1",
            )
            .bind(format!("discord:{channel}:10"))
            .fetch_one(&pool)
            .await
            .unwrap();
            let ended = if refused { "failed" } else { "completed" };
            assert_eq!(
                receipt,
                (state.input_origin_ids.clone(), Some(ended.into()))
            );
        }
        pool.close().await;
        db.drop().await;
    })
    .await;
}
