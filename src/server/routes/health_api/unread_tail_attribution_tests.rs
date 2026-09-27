//! HTTP entry tests for the unread-tail refusal record at the manual reattach and
//! stale-mailbox sites; real tmux, skipped (NO VERDICT) without it. No decision changes.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::services::discord::relay_recovery::unread_tail_seed::{UnreadTailSeed, UnreadTailShape};
use crate::services::discord::relay_recovery::{
    UNREAD_TAIL_SITE_STALE_MAILBOX, stale_mailbox_idle_tail_admits,
};

async fn post(seed: &UnreadTailSeed, uri: &str, body: String) -> (StatusCode, serde_json::Value) {
    let app = super::tests::test_api_router_with_config_and_registry(
        crate::config::Config::default(),
        Some(seed.registry.clone()),
    );
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::from(body))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).expect("json body"))
}

async fn relay_recovery(seed: &UnreadTailSeed, apply: bool) -> serde_json::Value {
    let uri = format!("/channels/{}/relay-recovery", seed.channel);
    let body = serde_json::json!({"provider": "claude", "apply": apply}).to_string();
    let (status, json) = post(seed, &uri, body).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

/// Drives the manual lane twice and returns the recorded refusals.
async fn manual_reattach_refusals(
    channel: u64,
    shape: UnreadTailShape,
) -> Option<Vec<serde_json::Value>> {
    let seed = UnreadTailSeed::start(channel, shape).await?;
    let plan = relay_recovery(&seed, false).await;
    assert_eq!(plan["decision"]["action"], "reattach_watcher", "{plan}");
    assert_eq!(plan["decision"]["auto_heal"]["eligible"], true, "{plan}");
    let unread = &plan["decision"]["evidence"]["unread_bytes"];
    match shape {
        UnreadTailShape::MeasuredBacklog => {
            assert!(unread.as_u64().is_some_and(|bytes| bytes > 0), "{plan}")
        }
        _ => assert!(unread.is_null(), "{plan}"),
    }

    for _ in 0..2 {
        let applied = relay_recovery(&seed, true).await;
        let result = &applied["apply_result"];
        assert_ne!(
            result["status"], "cleared_idle_tmux_stale_turn",
            "{applied}"
        );
        assert_ne!(result["removed_mailbox_token"], true, "{applied}");
        assert!(seed.turn_kept(), "the idle turn must survive: {applied}");
    }
    Some(seed.refusals())
}

/// A missing transcript leaves the tail UNMEASURED but readiness Unknown too, so nothing is recorded.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_reattach_records_nothing_when_readiness_also_refuses() {
    let Some(refusals) =
        manual_reattach_refusals(5_996_110_001, UnreadTailShape::RowOutputMissing).await
    else {
        return;
    };
    assert!(refusals.is_empty(), "{refusals:?}");
}

/// An UNMEASURED tail with candidate and readiness admitting is recorded once, unless an
/// unrelayed final answer is what refuses; the turn survives either way.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_reattach_records_only_when_no_unrelayed_answer_refuses() {
    for (channel, answer) in [(5_996_110_003, false), (5_996_110_004, true)] {
        let shape = UnreadTailShape::ForeignWatcher { answer };
        let Some(refusals) = manual_reattach_refusals(channel, shape).await else {
            return;
        };
        assert_eq!(refusals.len(), usize::from(!answer), "{refusals:?}");
        if !answer {
            assert_eq!(refusals[0]["site"], "manual_reattach_idle_clear");
            assert_eq!(refusals[0]["decided_by"], "unattributed_tail");
        }
    }
}

/// A MEASURED backlog refuses the clear as the invariant intends: nothing is recorded.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_reattach_records_nothing_for_a_measured_backlog() {
    let Some(refusals) =
        manual_reattach_refusals(5_996_110_002, UnreadTailShape::MeasuredBacklog).await
    else {
        return;
    };
    assert!(refusals.is_empty(), "{refusals:?}");
}

/// The stale-mailbox route still answers 409 for an UNMEASURED tail, names it, and records it once.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_mailbox_repair_names_and_records_an_unmeasured_tail() {
    let Some(seed) = UnreadTailSeed::start(5_996_120_001, UnreadTailShape::RowOutputMissing).await
    else {
        return;
    };
    let body = serde_json::json!({"channel_id": seed.channel.get(), "provider": "claude"});
    for _ in 0..2 {
        let (status, json) = post(&seed, "/doctor/stale-mailbox/repair", body.to_string()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["safety_gate"], "tmux_present", "{json}");
        assert_eq!(json["unread_tail"], "tail_not_measured", "{json}");
        assert!(seed.turn_kept(), "the idle turn must survive: {json}");
    }
    let refusals = seed.refusals();
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0]["site"], UNREAD_TAIL_SITE_STALE_MAILBOX);
    assert_eq!(refusals[0]["decided_by"], "tail_not_measured");

    // A fresh episode another conjunct already refused records nothing.
    let mut fresh_episode = seed
        .registry
        .snapshot_watcher_state_for_provider(&seed.provider, seed.channel.get())
        .await
        .expect("fixture snapshot");
    fresh_episode.mailbox_active_user_msg_id = Some(1);
    assert!(!stale_mailbox_idle_tail_admits(
        &seed.provider,
        &fresh_episode,
        false
    ));
    assert_eq!(seed.refusals().len(), 1, "{:?}", seed.refusals());
}
