use super::*;
use crate::services::discord::{inflight, mailbox_snapshot, mailbox_try_start_turn};
use poise::serenity_prelude::{MessageId, UserId};

fn row(provider: &ProviderKind, channel: ChannelId, user_msg_id: u64) -> InflightTurnState {
    let prompt = "status?".to_string();
    InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        7,
        user_msg_id,
        0,
        prompt,
        None,
        None,
        None,
        None,
        0,
    )
}

#[tokio::test]
async fn a_foreign_durable_row_refuses_the_start_and_unwinds_only_this_turn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = shared.provider.clone();
    let (free, held) = (ChannelId::new(6_245_201), ChannelId::new(6_245_202));
    let mut external = row(&provider, held, 0);
    external.turn_source = inflight::TurnSource::ExternalInput;
    inflight::save_inflight_state_create_new(&external).expect("external row");
    for (channel, refused) in [(free, false), (held, true)] {
        let token = Arc::new(CancelToken::new());
        let owner = (UserId::new(7), MessageId::new(11));
        assert!(mailbox_try_start_turn(&shared, channel, token.clone(), owner.0, owner.1).await);
        let mut mine = row(&provider, channel, 11);
        mine.turn_nonce = token.turn_nonce().map(str::to_owned);
        let created = inflight::save_inflight_state_create_new(&mine);
        let result =
            super::super::foreign_row::admit_headless(&shared, &provider, &mine, &token, created)
                .await;
        let observed = (
            matches!(result, Err(HeadlessTurnStartError::Conflict(_))),
            mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_none(),
            token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
            inflight::load_inflight_state_read_only(&provider, channel.get())
                .map(|r| r.user_msg_id),
        );
        // Refused: Conflict, mailbox released, token cancelled, the other turn's row kept.
        let expected = if refused {
            (true, true, true, Some(0))
        } else {
            (false, false, false, Some(11))
        };
        assert_eq!(observed, expected, "channel {channel}");
    }
}

#[test]
fn the_row_refusal_returns_before_any_provider_or_bridge_spawn() {
    let src = include_str!("../headless_turn.rs");
    let call = "foreign_row::admit_headless(shared, &provider, &inflight_state, &cancel_token, created)\n        .await?;";
    let (refusal, spawn) = (src.find(call), src.find("tokio::task::spawn_blocking"));
    assert!(
        refusal
            .zip(spawn)
            .is_some_and(|(refusal, spawn)| refusal < spawn)
    );
}
