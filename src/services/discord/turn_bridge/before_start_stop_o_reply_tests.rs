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
