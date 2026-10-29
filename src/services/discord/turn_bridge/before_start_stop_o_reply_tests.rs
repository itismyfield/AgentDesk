//! Where O owns the destination body a sealed before-start stop shows as a reply to the turn's
//! message: only a send that returned a real message id acks the receipt, then the close settles.
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
use crate::services::discord::recovery_engine::o_cut_recorder;
use crate::services::discord::shared_state::test_rest;
use crate::services::tui_o::cutover::test_override::force_channels;
use std::sync::atomic::AtomicBool;

fn dead_http() -> Arc<serenity::Http> {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let http = serenity::HttpBuilder::new("test-token").proxy(format!("http://{address}"));
    Arc::new(http.ratelimiter_disabled(true).build())
}

/// A refused reply keeps the receipt pending and the turn held; the next reply that returns its
/// message id acks the receipt and settles the close. O's body is never edited.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_o_owned_body_shows_the_stop_as_one_reply_then_settles() {
    let close = Close::open(927, |row| row.runtime_kind = Some(ClaudeTui)).await;
    let _o = force_channels(&[(927, ClaudeTui)]);
    let submitted = test_hooks::submitted();
    let polled = AtomicBool::new(false);
    {
        let _rest = test_rest::install(dead_http());
        assert!(!close.consume(recorded(&polled)).await, "a refused reply");
    }
    assert_eq!(close.receipt_pending(), Some(true), "no message id, no ack");
    assert!(close.held().await);
    assert_eq!(test_hooks::submitted(), submitted);

    let discord = o_cut_recorder::start(927).await;
    let _rest = test_rest::install(discord.http.clone());
    assert!(
        close.consume(recorded(&polled)).await,
        "{:?}",
        discord.calls()
    );
    assert!(!polled.load(Ordering::Acquire), "O's body is never edited");
    assert_eq!(test_hooks::submitted(), submitted + 1);
    assert!(
        close.durable().is_none(),
        "the close cleared its receipt row"
    );
    let calls = discord.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].route.starts_with("POST "), "{calls:?}");
    assert!(
        calls[0].route.ends_with("/channels/927/messages"),
        "{calls:?}"
    );
    assert_eq!(calls[0].content.as_deref(), Some(ADMITTED_ABORT_NOTICE));
    let anchor = close.row.current_msg_id.to_string();
    assert_eq!(
        calls[0].reply_to,
        Some(anchor),
        "a reply to the turn's message"
    );
}

/// An ownership that cannot be read shows nothing: the receipt stays pending and the turn held.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_unreadable_o_ownership_shows_nothing() {
    let close = Close::open(928, |row| row.runtime_kind = Some(ClaudeTui)).await;
    let _o = force_channels(&[(928, CodexTui)]);
    let discord = o_cut_recorder::start(928).await;
    let _rest = test_rest::install(discord.http.clone());
    let polled = AtomicBool::new(false);
    assert!(!close.consume(recorded(&polled)).await);
    assert!(!polled.load(Ordering::Acquire));
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
    assert_eq!(close.receipt_pending(), Some(true));
    assert!(close.held().await);
}

/// A reply shown whose receipt ack then failed is not shown again: the next attempt gets the same
/// message back from the outbound dedupe and only acks it.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_o_reply_shown_before_a_failed_ack_is_acked_without_a_second_send() {
    let close = Close::open(929, |row| row.runtime_kind = Some(ClaudeTui)).await;
    let _o = force_channels(&[(929, ClaudeTui)]);
    let polled = AtomicBool::new(false);
    {
        let _rest = test_rest::install(dead_http());
        assert!(!close.consume(recorded(&polled)).await, "a refused reply");
    }
    assert_eq!(close.receipt_pending(), Some(true));
    let discord = o_cut_recorder::start(929).await;
    let _rest = test_rest::install(discord.http.clone());
    InflightTurnState::arm_before_start_receipt_fault(929, false);
    assert!(!close.consume(recorded(&polled)).await, "the ack failed");
    assert!(InflightTurnState::before_start_receipt_faults_consumed(929));
    assert_eq!(close.receipt_pending(), Some(true), "no ack yet");
    assert_eq!(discord.calls().len(), 1, "{:?}", discord.calls());
    let shown = discord.calls()[0].clone();
    let row = close.durable().unwrap();
    let again = send_stop_reply(&discord.http, &close.shared, &row).await;
    let first = Some(MessageId::new(900_001));
    assert_eq!(again, Ok(first), "the same message, not a new one");
    assert!(
        close.consume(recorded(&polled)).await,
        "{:?}",
        discord.calls()
    );
    assert!(close.durable().is_none(), "the acked close settled");
    let calls = discord.calls();
    assert_eq!(calls.len(), 1, "one reply in all: {calls:?}");
    assert_eq!(calls[0].reply_to, shown.reply_to);
    assert!(!polled.load(Ordering::Acquire), "O's body is never edited");
}

/// A turn-mode turn shows no anchor, so its stop is a reply to the user's own message; the reply
/// id acks the receipt and the close settles without any edit.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_turn_mode_anchorless_turn_replies_to_the_user_message_then_settles() {
    let close = Close::open(930, |row| {
        row.runtime_kind = Some(ClaudeTui);
        row.current_msg_id = 0;
    })
    .await;
    let _o = force_channels(&[(930, ClaudeTui)]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(930);
    let discord = o_cut_recorder::start(930).await;
    let _rest = test_rest::install(discord.http.clone());
    let submitted = test_hooks::submitted();
    let polled = AtomicBool::new(false);
    assert!(
        close.consume(recorded(&polled)).await,
        "{:?}",
        discord.calls()
    );
    assert!(!polled.load(Ordering::Acquire), "no anchor is edited");
    assert_eq!(test_hooks::submitted(), submitted + 1);
    assert!(close.durable().is_none(), "the close cleared its row");
    let calls = discord.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].route.starts_with("POST "), "{calls:?}");
    assert_eq!(calls[0].content.as_deref(), Some(ADMITTED_ABORT_NOTICE));
    assert_eq!(
        calls[0].reply_to,
        Some(close.row.user_msg_id.to_string()),
        "a reply to the user's message"
    );
}

/// Off turn mode an anchorless row keeps today's hold: nothing is shown and the receipt waits.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_legacy_anchorless_turn_shows_nothing_and_stays_held() {
    let close = Close::open(931, |row| {
        row.runtime_kind = Some(ClaudeTui);
        row.current_msg_id = 0;
    })
    .await;
    let _o = force_channels(&[(931, ClaudeTui)]);
    let discord = o_cut_recorder::start(931).await;
    let _rest = test_rest::install(discord.http.clone());
    let submitted = test_hooks::submitted();
    let polled = AtomicBool::new(false);
    assert!(!close.consume(recorded(&polled)).await);
    assert!(!polled.load(Ordering::Acquire));
    assert!(discord.calls().is_empty(), "{:?}", discord.calls());
    assert_eq!(test_hooks::submitted(), submitted);
    assert!(close.held().await);
}

/// The bridge exit hands over its in-memory absence, the sentinel id; normalized to no message,
/// a turn-mode stop still replies to the user's message, acks and clears the row.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_turn_mode_bridge_exit_normalizes_the_sentinel_and_settles_by_reply() {
    let close = Close::open(932, |row| {
        row.runtime_kind = Some(ClaudeTui);
        row.current_msg_id = 0;
    })
    .await;
    let _o = force_channels(&[(932, ClaudeTui)]);
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(932);
    let key = TurnKey::new(ChannelId::new(932), close.row.user_msg_id, 17)
        .with_episode_nonce(close.row.turn_nonce.as_deref());
    close.shared.turn_finalizer.register_start(
        key,
        ProviderKind::Claude,
        RelayOwnerKind::None,
        &close.shared,
    );
    let discord = o_cut_recorder::start(932).await;
    let _rest = test_rest::install(discord.http.clone());
    let sentinel =
        crate::services::discord::turn_bridge::current_message_anchor::detached_current_msg_id_from_durable(0);
    exit_held(
        &close.shared,
        &crate::services::discord::gateway::HeadlessGateway,
        &close.actor,
        &close.row,
        sentinel,
        (false, false, false),
    )
    .await;
    let calls = discord.calls();
    assert!(close.durable().is_none(), "the stop settled: {calls:?}");
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(
        calls[0].reply_to,
        Some(close.row.user_msg_id.to_string()),
        "a reply to the user's message"
    );
}
