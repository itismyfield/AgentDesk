//! A Herdr launch on a selected channel, then a writer stop: the restarted host takes the channel
//! back from its store and posts each recorded result once, while Legacy's body claim stays O's.

use super::*;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send};

/// The startup event a Herdr launch logs, naming its execution.
fn herdr_launch(source: SourceId) -> Vec<u8> {
    let mut event: serde_json::Value = serde_json::from_slice(&startup(source)).unwrap();
    event["execution_nonce"] = "herdr-p8-3".into();
    [serde_json::to_vec(&event).unwrap(), b"\n".to_vec()].concat()
}

#[tokio::test(start_paused = true)]
async fn a_restarted_host_takes_a_herdr_launch_back_from_its_store_and_posts_each_result_once() {
    let (harness, path) = fresh(herdr_launch);
    harness.gate.acquired();
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert!(
        io.calls().contains(&("facts", CHANNEL)),
        "the launch activated"
    );
    append(&path, &row("m1", "posted before the stop"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["posted before the stop"]);
    // Recorded with no poll in between, so the stopped writer never read it.
    append(&path, &row("m2", "recorded at the stop"));
    abort(tasks);
    polls(2).await;
    let written = std::fs::read(init_path(&harness, CHANNEL)).unwrap();

    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_host(&harness, &io, &ready);
    polls(6).await;
    assert!(
        !io.calls().contains(&("facts", CHANNEL)),
        "the restart recovers the store, it does not activate"
    );
    let reread = std::fs::read(init_path(&harness, CHANNEL)).unwrap();
    assert_eq!(reread, written);
    assert!(ready.is_ready(CHANNEL));
    assert_eq!(
        harness.port.posts(),
        ["posted before the stop", "recorded at the stop"]
    );
    let claim = Some(BodyClaim::new(CHANNEL, Some(ClaudeTui)));
    let legacy = claim_then_send(claim, || async { unreachable!("Legacy sent an O body") });
    assert!(matches!(legacy.await, Ok(BodySend::OwnedByO)));
    abort(tasks);
}

// A new boot where Legacy sends a body before the host sees the store holds the channel: the
// recorded result stays unposted in store and transcript; the next adoption posts it once.
#[tokio::test(start_paused = true)]
async fn a_released_boot_holds_the_herdr_store_and_the_next_adoption_posts_its_result_once() {
    let (harness, path) = fresh(herdr_launch);
    harness.gate.acquired();
    let first = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_host(&harness, &io, &ready);
    polls(3).await;
    append(&path, &row("m1", "posted before the stop"));
    polls(3).await;
    append(&path, &row("m2", "recorded at the stop"));
    abort(tasks);
    polls(2).await;
    drop(first);
    let written = std::fs::read(init_path(&harness, CHANNEL)).unwrap();

    let released = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let claim = Some(BodyClaim::new(CHANNEL, Some(ClaudeTui)));
    let legacy = claim_then_send(claim, || async { "a Legacy body" }).await;
    assert!(
        matches!(legacy, Ok(BodySend::Sent(_))),
        "Legacy took the new boot"
    );
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_host(&harness, &io, &ready);
    polls(6).await;
    let halted = io.alarms.halted();
    assert!(
        halted
            .iter()
            .any(|(_, detail)| detail.contains("before its store was seen")),
        "{halted:?}"
    );
    assert!(!ready.is_ready(CHANNEL));
    assert_eq!(
        harness.port.posts(),
        ["posted before the stop"],
        "held, not posted"
    );
    assert_eq!(
        std::fs::read(init_path(&harness, CHANNEL)).unwrap(),
        written
    );
    let transcript = std::fs::read_to_string(&path).unwrap();
    assert!(
        transcript.contains("recorded at the stop"),
        "the result is kept"
    );
    abort(tasks);
    polls(2).await;
    drop(released);

    let _next = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = start_host(&harness, &io, &ready);
    polls(6).await;
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert!(
        !io.calls().contains(&("facts", CHANNEL)),
        "resumed from the store"
    );
    assert!(ready.is_ready(CHANNEL));
    assert_eq!(
        harness.port.posts(),
        ["posted before the stop", "recorded at the stop"]
    );
    abort(tasks);
}
