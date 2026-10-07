//! The live record lands before the handler, catch-up re-reads it under the record lock
//! right before the enqueue commit, and a broken record never settles anything.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::discord_mock::BOT_ID;
use super::stop_command_catch_up_e2e::{
    assert_no_unhandled, hold_a_turn, queued_ids, recent_snowflake_base, stop_the_turn_with,
};
use super::{PROVIDER_KEY, RelayE2eHarness, wait_until};
use crate::services::discord::catch_up::consumed_commands::test_gate::{self, Stage};
use crate::services::discord::runtime_store;

const SKILL_PROMPT: &str = "Execute the skill `/review`";

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

/// Phase 1 resumes past `newest` from the durable checkpoint and reads nothing,
/// while phase 2 still scans from the live cursor at `active`.
fn leave_the_scan_to_phase2(harness: &RelayE2eHarness, active: u64, newest: u64) {
    runtime_store::save_last_message_id(PROVIDER_KEY, super::CHANNEL_ID, newest);
    harness
        .shared
        .last_message_ids
        .insert(harness.channel_id, active);
}

fn assert_phase1_read_past(harness: &RelayE2eHarness, newest: u64) {
    let first = harness.history_queries().first().cloned();
    let phase1_after = first.and_then(|query| query.after);
    assert_eq!(
        phase1_after,
        Some(newest),
        "phase 1 must not see the scenario"
    );
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
            harness.seed_channel_history(&[
                (answered, "bot answer", true),
                (dropped, &dropped_text, false),
                (stop, &stop_text, false),
                (missed, "question the gateway dropped", false),
            ]);
            let (_turn, token) = hold_a_turn(&harness, active).await;
            if let Phase::Two = phase {
                leave_the_scan_to_phase2(&harness, active, missed);
            }
            let (held, release) = test_gate::hold(Stage::BeforeLock, stop);
            let sweep = harness.spawn_catch_up();
            let parked = tokio::time::timeout(Duration::from_secs(5), held.notified()).await;
            assert!(
                parked.is_ok(),
                "{case}: the sweep must classify the stop as recoverable"
            );
            stop_the_turn_with(&harness, &token, stop, &stop_text).await;
            release.notify_one();
            sweep.await.expect("the sweep finishes");
            if let Phase::Two = phase {
                assert_phase1_read_past(&harness, missed);
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
    harness.seed_channel_history(&[
        (answered, "bot answer", true),
        (stop, "!stop", false),
        (missed, "question the gateway dropped", false),
    ]);
    let (_turn, token) = hold_a_turn(&harness, active).await;
    stop_the_turn_with(&harness, &token, stop, "!stop").await;
    leave_the_scan_to_phase2(&harness, active, missed);

    harness.run_catch_up().await;

    assert_phase1_read_past(&harness, missed);
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

/// `!skill` consumes only its own message: catch-up settles it, while the skill
/// prompt dispatched under the confirmation message still reaches the provider.
#[test]
fn a_skill_command_is_settled_while_its_skill_prompt_runs() {
    // Skill intake runs deeper than a default 2 MiB worker stack in debug builds.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(16 << 20)
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(skill_command_scenario());
}

async fn skill_command_scenario() {
    let harness = RelayE2eHarness::start().await;
    harness.register_channel_in_role_map();
    harness.answer_placeholders_immediately();
    (harness.shared.skills_cache.write().await).push(("review".into(), "review".into()));
    let base = recent_snowflake_base();
    let (answered, skill, missed) = (base | 1, base | 2, base | 3);
    harness.seed_channel_history(&[
        (answered, "bot answer", true),
        (skill, "!skill review", false),
        (missed, "question the gateway dropped", false),
    ]);

    harness
        .spawn_user_message(skill, "!skill review")
        .await
        .expect("delivery task")
        .expect("live intake handles the command");
    let dispatched = wait_until(Duration::from_secs(15), {
        let root = harness.root.path().to_path_buf();
        move || {
            let inputs = std::fs::read_to_string(root.join(super::PROVIDER_INPUTS_FILE));
            let ran = inputs.is_ok_and(|inputs| inputs.contains(SKILL_PROMPT));
            Box::pin(async move { ran })
        }
    })
    .await;
    assert!(
        dispatched,
        "the skill prompt must reach the provider: {:?}",
        harness.messages()
    );

    harness.run_catch_up().await;

    assert_eq!(queued_ids(&harness).await, vec![missed]);
    assert_no_unhandled(&harness);
}
