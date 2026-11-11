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
