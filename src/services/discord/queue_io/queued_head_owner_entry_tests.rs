//! Entries into a channel the input owner holds, through the real route and kickoff: each reaches
//! the owner and runs none of the legacy steps.
use super::*;
use crate::services::discord::queue_io::input_ingress::{ADOPTS, adopt};
use crate::services::discord::queue_io::input_owner::admission::{
    Admission, AdmissionState, AdmissionTicket, admit, status,
};
use crate::services::discord::queue_io::input_owner::routine::synthetic_original;
use crate::services::turn_orchestrator::PendingQueueItem;

#[tokio::test(flavor = "current_thread")]
async fn an_owner_channel_kickoff_wakes_the_owner_and_runs_no_legacy_step_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(7_272_581, "all").await;
    let adopted = ADOPTS.scope(
        true,
        adopt(&fixture.shared, &ProviderKind::Claude, fixture.channel),
    );
    assert_eq!(adopted.await, Ok(true));
    let mailbox = fixture.shared.mailbox(fixture.channel);
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":8_000_000_000_000_581_001_u64,"channel_id":fixture.channel.get(),
        "source_message_ids":[8_000_000_000_000_581_001_u64],"queued_generation":1,
        "text":"after the completion"}))
    .unwrap();
    let admitted = admit(&mailbox, &original, AdmissionTicket::new()).await;
    assert!(matches!(admitted, Admission::Accepted(_)));
    // The drive grants the front when it looks; a delivered front leaves no item behind.
    let looked = || async {
        let state = mailbox.v2_request(|slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            Ok((owner.front_outstanding(), owner.front().is_none()))
        });
        let (granted, gone) = state.await.unwrap();
        granted || gone
    };
    assert!(!looked().await, "nothing looked before the kickoff");
    let gets = fixture.gets();

    fixture.kick().await;
    for _ in 0..200 {
        if looked().await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(looked().await, "the owner's drive looked at its front");
    assert_eq!(fixture.gets(), gets, "no legacy routing read");
    let channel = fixture.channel.get();
    let rows = ATTEMPT_ROWS.lock().unwrap().clone();
    assert!(
        !rows.iter().any(|row| row.channel_id == channel),
        "no legacy queued-head attempt"
    );
}

/// An HTTP start on the owner's channel behind a TUI-direct turn is admitted by the owner under its
/// turn's ticket instead of refused as busy, and the legacy start types nothing.
#[tokio::test(flavor = "current_thread")]
async fn an_owner_channel_http_start_is_admitted_by_the_owner_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::build(7_272_582, "all").await;
    // A booted process has settled its generation; a synthetic original is of that generation.
    let generation = runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "5").unwrap();
    // A TUI-direct turn holds the channel, which the legacy route refuses as busy.
    let held = fixture.holder_message.get();
    fixture.pane.reseat_row(TurnSource::ExternalInput, held);
    let channel = fixture.channel.get();
    let prompt = "start through the owner";
    let request = Request::builder()
        .method(Method::POST)
        .uri("/agents/s2b-head-agent/turn/start")
        .header("content-type", "application/json")
        .extension(crate::server::routes::auth::loopback_peer())
        .body(Body::from(json!({ "prompt": prompt }).to_string()))
        .unwrap();
    let response = ADOPTS.scope(true, fixture.app.clone().oneshot(request));
    let response = response.await.unwrap();
    let code = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert!(code.is_success(), "{code} {body}");
    assert_eq!(body["status"], "accepted", "{body}");
    let turn_id = body["turn_id"].as_str().unwrap();
    let (_, ticket) = synthetic_original(turn_id, channel, prompt, 1).unwrap();
    let mailbox = fixture.shared.mailbox(fixture.channel);
    assert_eq!(
        status(&mailbox, &ticket).await,
        Ok(AdmissionState::Committed)
    );
    assert_eq!(
        fixture.pane.tmux_calls(),
        0,
        "the legacy start typed nothing"
    );
}

/// A channel whose legacy queue still holds input stays legacy until that backlog drains: the
/// owner never adopts over it, so the legacy kickoff takes A then B and the next ingress adopts.
#[tokio::test(flavor = "current_thread")]
async fn a_v1_backlog_drains_through_the_legacy_kickoff_before_the_owner_adopts_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let fixture = Fixture::new(7_272_583, "all").await;
    let _gate = router::busy_inject_support::open_gate(fixture.channel.get());
    let before = fixture.queue().await;
    let adopt_now = || {
        let adopted = adopt(&fixture.shared, &ProviderKind::Claude, fixture.channel);
        ADOPTS.scope(true, adopted)
    };
    for left in [1, 0] {
        assert_eq!(
            adopt_now().await,
            Ok(false),
            "no owner over a legacy backlog"
        );
        ADOPTS.scope(true, fixture.kick()).await;
        assert_eq!(
            fixture.queue().await.len(),
            left,
            "the legacy kickoff took its head"
        );
    }
    let terminal = fixture.disk()["terminal_identities"].clone();
    let taken: Vec<u64> = (terminal.as_array().unwrap().iter())
        .map(|identity| identity["primary"].as_u64().unwrap())
        .collect();
    let queued: Vec<u64> = before.iter().map(|item| item.message_id.get()).collect();
    assert_eq!(taken, queued, "A then B");
    assert_eq!(
        adopt_now().await,
        Ok(true),
        "the drained channel's next ingress adopts"
    );
}
