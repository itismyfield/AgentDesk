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
    use super::recovery_context_take_order_tests::source_guard_tests::{
        HEADLESS_TURN, row_refusal_gaps,
    };
    assert_eq!(row_refusal_gaps(HEADLESS_TURN), Vec::<String>::new());
}

/// A row kept marked because its receipt did not commit no longer blocks the channel: the next
/// start records that receipt, removes the row and starts; an unmarked foreign row still refuses.
#[tokio::test]
async fn a_marked_row_is_settled_by_the_next_start_which_then_starts() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = shared.provider.clone();
    let channel = ChannelId::new(6_245_211);
    let mut marked = row(&provider, channel, 9);
    marked.turn_nonce = Some("ended-turn".into());
    marked.input_origin_ids = vec!["G-marked".into()];
    marked.terminal_receipt = Some(inflight::TerminalReceiptStamp {
        outcome: crate::db::turns::TerminalOutcome::Completed,
        confirmed_at: chrono::Utc::now(),
    });
    inflight::save_inflight_state_create_new(&marked).expect("marked row");

    let token = Arc::new(CancelToken::new());
    assert!(
        mailbox_try_start_turn(
            &shared,
            channel,
            token.clone(),
            UserId::new(7),
            MessageId::new(11)
        )
        .await
    );
    let mut mine = row(&provider, channel, 11);
    mine.turn_nonce = token.turn_nonce().map(str::to_owned);
    let created = inflight::save_inflight_state_create_new(&mine);
    assert!(
        created.is_err(),
        "the marked row holds the channel at create"
    );
    let result =
        super::super::foreign_row::admit_headless(&shared, &provider, &mine, &token, created).await;
    assert!(result.is_ok(), "{result:?}");
    let on_disk = inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
    assert_eq!(
        on_disk.user_msg_id, 11,
        "this turn's row replaced the settled one"
    );
    assert!(!token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
    shared.mailboxes.remove_fixture_for_test(channel);
}
