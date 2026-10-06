//! A hosted Herdr actor halted by a store error while a cleared-away source is not retired: the
//! resumed actor publishes that source as unsettled again, never a zero it has not read.

use std::io::ErrorKind::StorageFull;
use std::time::Duration;

use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::store::fault::{self, Keep, Step as At};

/// Startup on `a`, then the pane's clear onto `b`, as the hook path logs them.
fn startup_then_clear(a: &SourceId, b: &SourceId) -> Vec<u8> {
    let mut log = startup(a.clone());
    let mut clear: serde_json::Value = serde_json::from_slice(&p5_event(
        CHANNEL,
        "claude",
        p5::BindingTarget::Source(b.clone()),
    ))
    .unwrap();
    clear["seq"] = 2.into();
    clear["old"] = serde_json::to_value(a).unwrap();
    clear["cause"] = serde_json::to_value(p5::BindingCause::Clear).unwrap();
    clear["evidence"]["hook_event"] = HookEventKind::SessionStart.as_str().into();
    log.extend(serde_json::to_vec(&clear).unwrap());
    log.push(b'\n');
    log
}

/// Sleeps a second at a time until `done`, failing after `limit` seconds.
async fn until(limit: u64, mut done: impl FnMut() -> bool) {
    for _ in 0..limit {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(done(), "not reached within {limit}s");
}

// A halt while the cleared-away source is unretired reads unknown; the resumed actor has no count
// until it publishes one, then publishes that source as unsettled until it retires it once quiet.
#[tokio::test(start_paused = true)]
async fn a_resumed_actor_publishes_its_unretired_predecessor_and_never_an_unread_zero() {
    let (harness, a_path, a) = switched_over(&row("m0", "before the clear"));
    let (b_path, b) = beside(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
    let log = startup_then_clear(&a, &b);
    p5_log(harness._runtime.path(), CHANNEL, &log);
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let hosts = herdr_configured();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    harness.gate.acquired();
    let tasks = hosted(&harness, &io, true, &ready);
    polls(3).await;
    assert!(ready.accepts(CHANNEL), "the boot actor is up");
    assert_eq!(
        ready.rotation_unsettled(CHANNEL),
        Some(1),
        "bound but not retired: {:?}",
        io.alarms.0.lock().unwrap()
    );

    let spool = harness._runtime.path().join("o_store");
    let spool = spool.join(CHANNEL.to_string()).join("spool");
    let full = fault::plant(&spool, At::Append(Keep::Nothing), StorageFull, None);
    append(&b_path, &row("n2", "after the halt"));
    until(10, || !io.alarms.halted().is_empty()).await;
    assert_eq!(ready.rotation_unsettled(CHANNEL), None, "a halted actor");

    // Unconfigured while it resumes, the resumed actor publishes nothing: its count stays unknown.
    drop(hosts);
    drop(full);
    until(120, || ready.accepts(CHANNEL)).await;
    polls(2).await;
    assert_eq!(
        ready.rotation_unsettled(CHANNEL),
        None,
        "no count before the resumed actor publishes one"
    );
    let _hosts = herdr_configured();
    polls(1).await;
    assert_eq!(
        ready.rotation_unsettled(CHANNEL),
        Some(1),
        "the resumed actor still reads the unretired predecessor"
    );
    polls(12).await;
    assert_eq!(
        ready.rotation_unsettled(CHANNEL),
        Some(0),
        "retired once quiet"
    );
    assert_eq!(harness.port.posts(), ["new first", "after the halt"]);
    abort(tasks);
}
