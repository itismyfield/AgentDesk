//! The live record lands before the handler, catch-up re-reads it under the record lock
//! right before the enqueue commit, and a broken record never settles anything.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::discord_mock::{BOT_ID, HistoryQuery};
use super::stop_command_catch_up_e2e::{
    assert_no_unhandled, hold_a_turn, queued_ids, recent_snowflake_base, stop_the_turn_with,
};
use super::{PROVIDER_KEY, RelayE2eHarness, wait_until};
use crate::services::discord::catch_up::consumed_commands::test_gate::{self, Stage};
use crate::services::discord::runtime_store;

#[derive(Clone, Copy, Debug)]
enum Phase {
    One,
    Two,
}

fn record_path() -> std::path::PathBuf {
    let root = runtime_store::last_message_root().expect("an isolated runtime root");
    root.join(PROVIDER_KEY)
        .join(format!("{}.consumed.json", super::CHANNEL_ID))
}

/// Keep the scanner before the incoming scenario while the live cursor tracks the active turn.
fn leave_the_scan_to_phase2(harness: &RelayE2eHarness, active: u64, frontier: u64, newest: u64) {
    runtime_store::save_last_message_id(PROVIDER_KEY, super::CHANNEL_ID, newest);
    harness.seed_scanner_checkpoint(frontier);
    harness
        .shared
        .last_message_ids
        .insert(harness.channel_id, active);
}

fn assert_phase1_read_past(harness: &RelayE2eHarness, frontier: u64) {
    assert_eq!(
        harness.history_queries(),
        vec![
            HistoryQuery {
                limit: Some(50),
                before: None,
                after: Some(frontier)
            },
            HistoryQuery {
                limit: Some(20),
                before: None,
                after: None
            },
        ],
        "phase 1 reads the earlier snapshot; phase 2 alone sees the incoming scenario"
    );
}

async fn reveal_to_phase2(harness: &RelayE2eHarness, history: &[(u64, &str, bool)]) {
    assert!(
        harness.wait_for_held_history(Duration::from_secs(5)).await,
        "the first After GET must be held before reading the old history"
    );
    harness.hold_next_history();
    harness.release_held_history();
    assert!(
        harness.wait_for_held_history(Duration::from_secs(5)).await,
        "the phase 2 GET must be held after the empty After response"
    );
    assert_eq!(harness.history_queries().len(), 2);
    eprintln!("phase2 history GET held after the empty After snapshot");
    harness.seed_channel_history(history);
    harness.release_held_history();
}

/// A command consumed after classification is refused at the commit in either phase, with or
/// without a leading mention, while an unconsumed command of the same spelling recovers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_consumed_after_classification_is_refused_at_the_commit() {
    let mention = format!("<@{BOT_ID}> ");
    let nick_mention = format!("<@!{BOT_ID}> ");
    for prefix in ["", mention.as_str(), nick_mention.as_str()] {
        for phase in [Phase::One, Phase::Two] {
            let case = format!("{prefix:?} {phase:?}");
            let harness = RelayE2eHarness::start().await;
            harness.register_channel_in_role_map();
            let base = recent_snowflake_base();
            let (answered, active, dropped, stop, missed) =
                (base | 1, base | 2, base | 3, base | 4, base | 5);
            let (dropped_text, stop_text) =
                (format!("{prefix}!skill review"), format!("{prefix}!stop"));
            let history = [
                (answered, "bot answer", true),
                (dropped, &dropped_text, false),
                (stop, &stop_text, false),
                (missed, "question the gateway dropped", false),
            ];
            if let Phase::Two = phase {
                harness.seed_channel_history(&[(answered, "bot answer", true)]);
            } else {
                harness.seed_channel_history(&history);
            }
            let (_turn, token) = hold_a_turn(&harness, active).await;
            if let Phase::Two = phase {
                leave_the_scan_to_phase2(&harness, active, answered, missed);
                harness.hold_next_history();
            }
            let (held, release) = test_gate::hold(Stage::BeforeLock, stop);
            let sweep = harness.spawn_catch_up();
            if let Phase::Two = phase {
                reveal_to_phase2(&harness, &history).await;
            }
            let parked = tokio::time::timeout(Duration::from_secs(5), held.notified()).await;
            assert!(
                parked.is_ok(),
                "{case}: the sweep must classify the stop as recoverable"
            );
            eprintln!("BeforeLock(stop) reached: {case}");
            stop_the_turn_with(&harness, &token, stop, &stop_text).await;
            release.notify_one();
            sweep.await.expect("the sweep finishes");
            if let Phase::Two = phase {
                assert_phase1_read_past(&harness, answered);
            }

            assert_eq!(queued_ids(&harness).await, vec![dropped, missed], "{case}");
            // The refusal leaves a retry; the next sweep settles the stop from the record.
            harness.run_catch_up().await;
            assert_eq!(
                queued_ids(&harness).await,
                vec![dropped, missed],
                "{case}: rescan"
            );
            assert_no_unhandled(&harness);
        }
    }
}

/// Phase 2 alone sees a command consumed before the sweep: it settles it like a
/// reply would, rather than leaving it to the commit refusal and a retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase2_settles_a_consumed_command_without_a_retry() {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    let base = recent_snowflake_base();
    let (answered, active, stop, missed) = (base | 1, base | 2, base | 3, base | 4);
    let history = [
        (answered, "bot answer", true),
        (stop, "!stop", false),
        (missed, "question the gateway dropped", false),
    ];
    harness.seed_channel_history(&[(answered, "bot answer", true)]);
    let (_turn, token) = hold_a_turn(&harness, active).await;
    stop_the_turn_with(&harness, &token, stop, "!stop").await;
    let recorded: Vec<u64> =
        serde_json::from_slice(&std::fs::read(record_path()).unwrap()).unwrap();
    assert!(
        recorded.contains(&stop),
        "the live handler must have recorded the stop before the scan"
    );
    eprintln!("consumed stop recorded before phase2 GET");
    leave_the_scan_to_phase2(&harness, active, answered, missed);

    harness.hold_next_history();
    let sweep = harness.spawn_catch_up();
    reveal_to_phase2(&harness, &history).await;
    sweep.await.expect("the phase 2 sweep finishes");

    assert_phase1_read_past(&harness, answered);
    assert_eq!(queued_ids(&harness).await, vec![missed]);
    assert!(
        !harness
            .shared
            .catch_up_retry_pending
            .contains_key(&harness.channel_id),
        "a settled command leaves nothing to re-read"
    );
    assert_no_unhandled(&harness);
}

/// The record lands before the handler returns: a sweep that runs while the
/// handler is still answering already finds the command consumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_is_recorded_before_its_handler_returns() {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    let base = recent_snowflake_base();
    let (answered, stop, missed) = (base | 1, base | 2, base | 3);
    harness.seed_channel_history(&[
        (answered, "bot answer", true),
        (stop, "!stop", false),
        (missed, "question the gateway dropped", false),
    ]);
    // An idle `!stop` answers with a reply, which the mock parks mid-handler.
    harness.hold_next_note();
    let delivery = harness.spawn_user_message(stop, "!stop");
    assert!(
        harness.wait_for_held_note(Duration::from_secs(5)).await,
        "the handler must be parked on its reply"
    );

    harness.run_catch_up().await;

    assert_eq!(queued_ids(&harness).await, vec![missed]);
    harness.release_held_note();
    delivery.await.expect("delivery task").expect("live intake");
    assert_no_unhandled(&harness);
}

/// A live record waits while catch-up holds the record lock between its final
/// re-read and the enqueue commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_final_check_holds_the_record_lock_through_the_commit() {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    let base = recent_snowflake_base();
    let (answered, active, stop, missed) = (base | 1, base | 2, base | 3, base | 4);
    harness.seed_channel_history(&[
        (answered, "bot answer", true),
        (stop, "!stop", false),
        (missed, "question the gateway dropped", false),
    ]);
    let (_turn, token) = hold_a_turn(&harness, active).await;
    let (held, release) = test_gate::hold(Stage::AfterRead, stop);
    let sweep = harness.spawn_catch_up();
    let parked = tokio::time::timeout(Duration::from_secs(5), held.notified()).await;
    assert!(
        parked.is_ok(),
        "the re-read must find no record and reach the commit"
    );

    let delivery = harness.spawn_user_message(stop, "!stop");
    let finished = wait_until(Duration::from_secs(2), {
        let token = token.clone();
        move || {
            let cancelled = token.cancelled.load(Ordering::SeqCst);
            Box::pin(async move { cancelled })
        }
    })
    .await;
    assert!(
        !finished && !delivery.is_finished(),
        "the record must wait for the commit"
    );

    release.notify_one();
    sweep.await.expect("the sweep finishes");
    delivery.await.expect("delivery task").expect("live intake");
    assert!(token.cancelled.load(Ordering::SeqCst));
}

/// A record that cannot be written, read or parsed is no evidence: catch-up
/// keeps recovering unconsumed commands and questions as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_io_failures_never_settle_unconsumed_input() {
    let failures: &[&str] = if cfg!(unix) {
        &["rename", "unreadable", "malformed"]
    } else {
        &["rename", "malformed"]
    };
    for &failure in failures {
        let harness = RelayE2eHarness::start().await;
        harness.register_channel_in_role_map();
        let base = recent_snowflake_base();
        let (answered, dropped, stop, missed) = (base | 1, base | 2, base | 3, base | 4);
        harness.seed_channel_history(&[
            (answered, "bot answer", true),
            (dropped, "!skill review", false),
            (stop, "!stop", false),
            (missed, "question the gateway dropped", false),
        ]);
        let path = record_path();
        if failure == "rename" {
            std::fs::create_dir_all(path.join("occupied")).unwrap();
        }
        harness
            .deliver_user_message(stop, "!stop")
            .await
            .expect("live intake handles the command");
        match failure {
            "rename" => assert!(path.is_dir(), "the record write must have failed"),
            #[cfg(unix)]
            "unreadable" => {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::Permissions::from_mode(0o000);
                std::fs::set_permissions(&path, mode).unwrap();
                assert!(
                    std::fs::read_to_string(&path).is_err(),
                    "the record must be unreadable"
                );
            }
            _ => {
                std::fs::write(&path, "not json").unwrap();
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
            }
        }

        harness.run_catch_up().await;

        let queued = queued_ids(&harness).await;
        for id in [dropped, missed] {
            assert!(
                queued.contains(&id),
                "{failure}: {id} must recover: {queued:?}"
            );
        }
        assert_no_unhandled(&harness);
    }
}
