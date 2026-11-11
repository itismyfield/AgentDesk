//! A stop fallback resumes waiting input once, after its cleanup: its token release wakes no queue,
//! an owned queue wakes once its row is gone, and an unowned backlog keeps its kickoff.

use std::sync::{Arc, Mutex};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::super::stop_judgement::judged_finish_tests::{Channel, claude, run};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::queue_io::cancel_backstop_test_support as support;
use crate::services::discord::queue_io::input_owner::admission::{AdmissionTicket, admit};
use crate::services::discord::{self as discord, inflight};
use crate::services::provider::CancelToken;
use crate::services::turn_lifecycle::TurnLifecycleTarget;
use crate::services::turn_orchestrator::{ChannelMailboxRegistry, PendingQueueItem};
use crate::test_support::wait::{WAIT_CAP, await_within};

/// Whether the channel's queue has an owner, and whether the legacy queue also holds a backlog.
#[derive(Clone, Copy, Debug)]
struct Case {
    owned: bool,
    legacy_backlog: bool,
}

/// Unhooks the channel's owner wake when the case ends.
struct WakeHook(ChannelId);

impl Drop for WakeHook {
    fn drop(&mut self) {
        support::set_owner_wake_for_tests(self.0, None);
    }
}

#[test]
fn a_stop_fallback_resumes_the_waiting_input_once_after_its_cleanup() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let cases = [
        (
            7_753_110,
            Case {
                owned: true,
                legacy_backlog: false,
            },
        ),
        (
            7_753_120,
            Case {
                owned: true,
                legacy_backlog: true,
            },
        ),
        (
            7_753_130,
            Case {
                owned: false,
                legacy_backlog: true,
            },
        ),
    ];
    for (id, case) in cases {
        run(async move {
            let shared = discord::make_shared_data_for_tests();
            let registry = Arc::new(HealthRegistry::new());
            registry
                .register("claude".to_string(), shared.clone())
                .await;
            let channel = ChannelId::new(id);
            let judged = Arc::new(CancelToken::new());
            let user_msg = MessageId::new(id + 1);
            let start = discord::mailbox_try_start_turn;
            assert!(start(&shared, channel, judged, UserId::new(7), user_msg).await);
            super::super::stall_watchdog_auto_heal_tests::seed_runtime_session(&shared, channel)
                .await;
            let row = Channel::row(channel, 1);
            let mailbox = shared.mailbox(channel);
            if case.legacy_backlog {
                let queued = ChannelMailboxRegistry::queued_for_test(id + 7);
                let persistence = discord::queue_persistence_context(&shared, &claude(), channel);
                mailbox.replace_queue(vec![queued], persistence).await;
            }
            if case.owned {
                let dir = discord::runtime_store::discord_pending_queue_root().unwrap();
                let file = dir.join(format!("claude/stop-wake/{id}.json"));
                mailbox.v2_adopt(file, "stop-wake".into()).await.unwrap();
                let front: PendingQueueItem = serde_json::from_value(serde_json::json!({
                    "author_id": 7, "message_id": id + 3, "channel_id": id,
                    "source_message_ids": [id + 3], "queued_generation": 1, "text": "held front"}))
                .unwrap();
                let _ = admit(&mailbox, &front, AdmissionTicket::new()).await;
            }
            // Each owner wake records whether the stop's row was still on disk at that moment.
            let wakes = Arc::new(Mutex::new(Vec::new()));
            let seen = wakes.clone();
            let probe = row.clone();
            let hook = Arc::new(move || seen.lock().unwrap().push(probe.exists()));
            support::set_owner_wake_for_tests(channel, Some(hook));
            let _unhook = WakeHook(channel);
            let completion = support::KickoffCompletion::install(channel);
            let mut events =
                discord::turn_completion_events::subscribe_turn_completion_events(&shared);

            let target = TurnLifecycleTarget {
                provider: Some(claude()),
                channel_id: Some(channel),
                tmux_name: String::new(),
            };
            let kill = crate::services::turn_lifecycle::force_kill_turn;
            let result = kill(Some(&registry), &target, "stop_wake", "stop_wake_force").await;
            assert!(
                !result.host_guard_kept(),
                "{case:?}: the judged turn was stopped"
            );
            assert!(
                !inflight::inflight_state_file_exists(&claude(), id),
                "{case:?}: row gone"
            );

            // Only an emptied bus ends the count; a lagged or closed one could hide an event.
            let mut eligible = 0;
            loop {
                match events.try_recv() {
                    Ok(event) => {
                        eligible +=
                            usize::from(event.channel_id == channel && event.queue_is_eligible());
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(other) => panic!("{case:?}: completion bus unreadable: {other:?}"),
                }
            }
            assert_eq!(eligible, 0, "{case:?}: the token release wakes no queue");
            if case.owned {
                await_within(completion.processed(0, WAIT_CAP), WAIT_CAP).await;
                let wakes = wakes.lock().unwrap().clone();
                assert_eq!(
                    wakes,
                    [false],
                    "{case:?}: one owner wake, after the row went"
                );
            } else {
                assert!(
                    wakes.lock().unwrap().is_empty(),
                    "{case:?}: no owner to wake"
                );
                let kicked = shared.restart.deferred_hook_channels.contains_key(&channel);
                assert!(kicked, "{case:?}: the legacy backlog keeps its kickoff");
                let depth = discord::mailbox_snapshot(&shared, channel)
                    .await
                    .queue_depth();
                assert_eq!(depth, 1, "{case:?}: and its queued input");
            }
        });
    }
}
