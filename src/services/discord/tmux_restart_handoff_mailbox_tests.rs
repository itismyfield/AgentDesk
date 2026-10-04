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
