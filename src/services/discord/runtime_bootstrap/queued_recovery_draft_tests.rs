//! After a restart, an input a person's draft held is released only by a look at its pane.

use super::*;
use crate::services::claude_tui::host_input::FakeDraftPane;
use crate::services::discord::busy_followup_retry_store as retries;
use crate::services::discord::{inflight, make_shared_data_for_tests};
use std::time::Duration;

/// An idle Claude pane whose composer shows `row`.
fn pane_with(row: &str) -> String {
    let border = "\u{2500}".repeat(60);
    let footer = "  \u{23f5}\u{23f5} bypass permissions on (shift+tab to cycle)";
    format!("\u{23fa} Done.\n\n\n{border}\n{row}\n{border}\n{footer}\n")
}

/// A restore re-protects the pane of an input a person's draft capped: nothing resets or runs while
/// the draft shows, then that input alone runs once; a busy-capped input stays parked.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_restored_draft_hold_lifts_only_when_its_pane_shows_the_draft_gone() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(6_897_501);
    let (held_id, busy_id) = (MessageId::new(6_897_502), MessageId::new(6_897_503));
    let config = crate::runtime_layout::config_file_path(tmp.path());
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let agent = format!(
        "  - id: draft-6897\n    name: \"draft\"\n    provider: claude\n    channels:\n      claude:\n        id: \"{channel}\"\n        name: \"draft-6897\"\n"
    );
    std::fs::write(&config, format!("server:\n  port: 8791\nagents:\n{agent}")).unwrap();
    let session = format!("restart-draft-{}", uuid::Uuid::new_v4().simple());

    let previous = make_shared_data_for_tests();
    for (id, pane) in [(held_id, Some(session.clone())), (busy_id, None)] {
        let text = format!("input {id}");
        let (user, card) = (id.get(), id.get() + 10);
        let state = inflight::InflightTurnState::new(
            provider.clone(),
            channel.get(),
            None,
            7,
            user,
            card,
            text,
            None,
            pane,
            None,
            None,
            0,
        );
        let requeue = crate::services::discord::mailbox_requeue_inflight_for_followup_retry;
        assert!(
            requeue(&previous, &provider, channel, &state)
                .await
                .enqueued
        );
    }
    let ids = |id: MessageId| (channel.get(), id.get(), id.get() + 10);
    let five_minutes = Duration::from_secs(300);
    let held = retries::record_attempt(&provider, ids(held_id), &[], five_minutes, Some(&session));
    assert!(held.unwrap().capped);
    for _ in 0..retries::MAX_BUSY_RETRY_COUNT {
        retries::record_attempt(&provider, ids(busy_id), &[], Duration::ZERO, None).unwrap();
    }
    previous.mailboxes.remove_fixture_for_test(channel);

    let shared = make_shared_data_for_tests();
    let pane = FakeDraftPane::new(&session);
    pane.show(&pane_with("\x1b[39m\u{276f}\u{a0}human draft A"));
    let taken = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = taken.clone();
    let _hook = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(Arc::new(
        move |shared, provider, kicked, _reason| {
            let record = record.clone();
            Box::pin(async move {
                if kicked != channel {
                    return None;
                }
                let take = crate::services::discord::mailbox_take_next_automatic_intervention;
                let take = take(&shared, &provider, kicked).await;
                let started = take.intervention.is_some();
                if let (Some(input), Some(lease)) = (take.intervention, take.dispatch_lease) {
                    record.lock().unwrap().push(input.message_id);
                    let settle =
                        crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success;
                    settle(&shared, &provider, kicked, input.message_id, lease).await;
                }
                Some(crate::services::discord::IdleQueueKickoffChannelOutcome { started })
            })
        },
    ));
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    restore_queued_and_inflight_work(&http, &shared, &provider).await;
    let queued = || async {
        let snapshot = shared.mailbox(channel).snapshot().await;
        let mut ids: Vec<_> = snapshot
            .intervention_queue
            .iter()
            .map(|i| i.message_id)
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(queued().await, [held_id, busy_id]);
    let budget = |id: MessageId| retries::load(&provider, channel.get(), id.get()).unwrap();
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(budget(held_id).capped, "the person's draft still shows");
        assert!(taken.lock().unwrap().is_empty());
    }
    pane.show(&pane_with(
        "\x1b[39m\u{276f}\u{a0}\x1b[2mTry \"refactor <filepath>\"\x1b[0m",
    ));
    tokio::time::sleep(Duration::from_secs(600)).await;
    let released = budget(held_id);
    let spent = (released.busy_retry_count, released.active_retry_ms);
    assert_eq!((spent, released.capped), ((0, 0), false));
    assert_eq!(*taken.lock().unwrap(), [held_id]);
    assert!(budget(busy_id).capped);
    assert_eq!(queued().await, [busy_id]);
    shared.mailboxes.remove_fixture_for_test(channel);
}
