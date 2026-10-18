use super::*;

use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use poise::serenity_prelude::{MessageId, UserId};
use serde_json::{Value, json};

use crate::services::discord::make_shared_data_for_tests_with_storage;
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, QueuePersistenceContext, TokenFinish,
};

static NEXT_CHANNEL: AtomicU64 = AtomicU64::new(61_018_401);

fn process_metadata() -> Value {
    json!({"routine_id": "process-mailbox-test", "execution_mode": "process", "execution_strategy": "fresh"})
}

struct Fixture {
    shared: Arc<SharedData>,
    channel: ChannelId,
    token: Arc<CancelToken>,
    mailbox: ChannelMailboxHandle,
}

impl Fixture {
    async fn new(provider: ProviderKind) -> (Self, ProcessMailbox) {
        Self::with_user_message(provider, 123).await
    }

    async fn with_user_message(
        provider: ProviderKind,
        user_message_id: u64,
    ) -> (Self, ProcessMailbox) {
        let mut shared = make_shared_data_for_tests_with_storage(None);
        Arc::get_mut(&mut shared)
            .expect("unshared fixture")
            .provider = provider.clone();
        shared.settings.write().await.provider = provider;
        let channel = ChannelId::new(NEXT_CHANNEL.fetch_add(1, Ordering::Relaxed));
        let token = Arc::new(CancelToken::new());
        let mailbox = shared.mailbox(channel);
        assert!(
            mailbox
                .try_start_turn(
                    token.clone(),
                    UserId::new(7),
                    MessageId::new(user_message_id)
                )
                .await,
            "the fixture must claim a real mailbox episode"
        );
        let lease = capture((&shared, channel), &token, Some(&process_metadata()))
            .await
            .expect("fresh process routine captures its active actor");
        (
            Self {
                shared,
                channel,
                token,
                mailbox,
            },
            lease,
        )
    }

    fn persistence(&self) -> QueuePersistenceContext {
        QueuePersistenceContext::new(&self.shared.provider, &self.shared.token_hash, None)
    }

    async fn settle(&self, cancelled_explicitly: bool) {
        if cancelled_explicitly {
            self.token.publish_cancel("routine_explicit_stop");
        } else {
            self.token.mark_completion_cleanup();
            self.token.cancelled.store(true, Ordering::Release);
        }
        self.finish(&self.token).await;
    }

    async fn finish(&self, token: &Arc<CancelToken>) {
        let result = self
            .mailbox
            .finish_turn_if_token(Some(token.clone()), self.persistence())
            .await;
        let TokenFinish::Finished(finished) = result else {
            panic!("the exact fixture episode must finish");
        };
        assert!(
            finished
                .removed_token
                .is_some_and(|removed| Arc::ptr_eq(&removed, token))
        );
    }

    fn assert_same_actor_retained(&self) {
        let retained = self
            .shared
            .mailbox_peek(self.channel)
            .expect("actor must be retained");
        assert!(
            retained.same_actor(&self.mailbox),
            "retention must preserve the captured actor"
        );
    }
}

async fn finalize(lease: ProcessMailbox) {
    let (tx, rx) = oneshot::channel();
    tx.send(BridgeCompletionSignal::Finalized).unwrap();
    lease.after_completion(rx, tokio::spawn(async {})).await;
}

#[tokio::test]
async fn finalized_settled_tokens_remove_the_actual_captured_actor() {
    with_isolated_runtime_root(|| async {
        for provider in [ProviderKind::Claude, ProviderKind::Codex] {
            for cancelled_explicitly in [false, true] {
                let (fixture, lease) = Fixture::new(provider.clone()).await;
                fixture.settle(cancelled_explicitly).await;
                finalize(lease).await;
                assert!(
                    fixture.shared.mailbox_peek(fixture.channel).is_none(),
                    "Finalized {provider:?} episode must unlink its actor; explicit_cancel={cancelled_explicitly}"
                );
                assert!(fixture.mailbox.snapshot().await.cancel_token.is_none());
            }
        }
    })
    .await;
}

#[tokio::test]
async fn predecessor_cannot_remove_a_live_or_already_finished_successor() {
    with_isolated_runtime_root(|| async {
        for successor_finished in [false, true] {
            let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
            fixture.settle(false).await;
            let successor = Arc::new(CancelToken::new());
            assert!(
                fixture
                    .mailbox
                    .try_start_turn(successor.clone(), UserId::new(7), MessageId::new(124),)
                    .await
            );
            if successor_finished {
                successor.mark_completion_cleanup();
                successor.cancelled.store(true, Ordering::Release);
                fixture.finish(&successor).await;
            }
            finalize(lease).await;
            fixture.assert_same_actor_retained();
            let current = fixture.mailbox.snapshot().await.cancel_token;
            if successor_finished {
                assert!(
                    current.is_none(),
                    "the claim fence protects an idle successor actor"
                );
            } else {
                assert!(current.is_some_and(|token| Arc::ptr_eq(&token, &successor)));
                assert!(!successor.cancelled.load(Ordering::Acquire));
            }
        }
    })
    .await;
}

#[tokio::test]
async fn finalization_preserves_queued_successor_messages_and_their_actor() {
    with_isolated_runtime_root(|| async {
        let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
        for id in [124, 125] {
            let queued = Intervention {
                author_id: UserId::new(7),
                author_is_bot: false,
                message_id: MessageId::new(id),
                queued_generation: fixture.shared.restart.current_generation,
                source_message_ids: vec![MessageId::new(id)],
                source_message_queued_generations: vec![],
                source_text_segments: vec![],
                text: format!("queued {id}"),
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
                pending_uploads: vec![],
                voice_announcement: None,
            };
            assert!(
                fixture
                    .mailbox
                    .enqueue(queued, fixture.persistence())
                    .await
                    .enqueued
            );
        }
        fixture.settle(false).await;
        finalize(lease).await;
        fixture.assert_same_actor_retained();
        assert_eq!(
            fixture
                .mailbox
                .snapshot()
                .await
                .intervention_queue
                .iter()
                .map(|item| (item.message_id.get(), item.text.as_str()))
                .collect::<Vec<_>>(),
            vec![(124, "queued 124"), (125, "queued 125")]
        );
        let (durable, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &fixture.shared.provider,
            &fixture.shared.token_hash,
            fixture.channel,
        );
        assert_eq!(
            durable
                .iter()
                .map(|item| item.message_id.get())
                .collect::<Vec<_>>(),
            vec![124, 125]
        );
    })
    .await;
}

#[tokio::test]
async fn unresolved_deferred_aborted_and_missing_completion_preserve_the_actor() {
    with_isolated_runtime_root(|| async {
        for signal in [
            Some(BridgeCompletionSignal::Unresolved),
            Some(BridgeCompletionSignal::DeferredToCustody),
            Some(BridgeCompletionSignal::DeferredToOwner),
            Some(BridgeCompletionSignal::EntryAborted),
            None,
        ] {
            let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
            fixture.settle(false).await;
            let (tx, rx) = oneshot::channel();
            if let Some(signal) = signal {
                tx.send(signal).unwrap();
            } else {
                drop(tx);
            }
            lease.after_completion(rx, tokio::spawn(async {})).await;
            fixture.assert_same_actor_retained();
        }
    })
    .await;
}

#[tokio::test]
async fn finalized_completion_waits_for_the_producer_to_exit_before_removing_actor() {
    with_isolated_runtime_root(|| async {
        let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
        fixture.settle(false).await;
        let (release, blocked) = oneshot::channel::<()>();
        let producer = tokio::spawn(async move {
            blocked.await.unwrap();
        });
        let (tx, rx) = oneshot::channel();
        tx.send(BridgeCompletionSignal::Finalized).unwrap();
        let retirement = lease.after_completion(rx, producer);
        tokio::pin!(retirement);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut retirement)
                .await
                .is_err(),
            "a terminal bridge signal must not bypass the producer join"
        );
        fixture.assert_same_actor_retained();
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), retirement)
            .await
            .expect("producer exit must allow retirement to complete");
        assert!(fixture.shared.mailbox_peek(fixture.channel).is_none());
    })
    .await;
}

#[tokio::test]
async fn aborted_or_panicked_producer_preserves_the_actor_after_finalized() {
    with_isolated_runtime_root(|| async {
        for panic in [false, true] {
            let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
            fixture.settle(false).await;
            let producer = if panic {
                tokio::spawn(async {
                    panic!("intentional process producer failure");
                })
            } else {
                let task = tokio::spawn(async {
                    std::future::pending::<()>().await;
                });
                task.abort();
                task
            };
            let (tx, rx) = oneshot::channel();
            tx.send(BridgeCompletionSignal::Finalized).unwrap();
            lease.after_completion(rx, producer).await;
            fixture.assert_same_actor_retained();
        }
    })
    .await;
}

#[tokio::test]
async fn retained_or_unreadable_inflight_is_preserved_with_its_actor() {
    with_isolated_runtime_root(|| async {
        for malformed in [false, true] {
            let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
            fixture.settle(false).await;
            let mut row = inflight::InflightTurnState::new(
                fixture.shared.provider.clone(),
                fixture.channel.get(),
                None,
                7,
                123,
                456,
                "retained delivery evidence".into(),
                None,
                None,
                None,
                None,
                0,
            );
            row.turn_nonce = Some("foreign-retained-episode".into());
            inflight::save_inflight_state(&row).unwrap();
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &fixture.shared.provider,
                fixture.channel.get(),
            );
            if malformed {
                std::fs::write(&path, b"{broken inflight").unwrap();
            }
            let before = std::fs::read(&path).unwrap();
            if malformed {
                assert!(
                    inflight::load_inflight_state_read_only_result(
                        &fixture.shared.provider,
                        fixture.channel.get()
                    )
                    .is_err()
                );
            }
            finalize(lease).await;
            fixture.assert_same_actor_retained();
            assert_eq!(
                std::fs::read(path).unwrap(),
                before,
                "retirement must not alter durable evidence"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn admission_watcher_and_token_guards_preserve_the_actor() {
    with_isolated_runtime_root(|| async {
        for guard in ["restored_owner", "restart", "token_tmux"] {
            let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
            fixture.settle(false).await;
            let owner_name = format!("fixture-restored-owner-{}", fixture.channel);
            match guard {
                "restored_owner" => {
                    assert!(
                        fixture
                            .shared
                            .tmux_watchers
                            .restore_owner_channel_for_tmux_session(&owner_name, fixture.channel,)
                    );
                    assert_eq!(
                        fixture
                            .shared
                            .tmux_watchers
                            .authoritative_session_bindings(fixture.channel)
                            .len(),
                        1
                    );
                }
                "restart" => fixture.token.set_restart_mode(Some(
                    crate::services::discord::InflightRestartMode::DrainRestart,
                )),
                "token_tmux" => fixture
                    .token
                    .bind_unmanaged_session_name("fixture-name-only-tmux"),
                _ => {}
            }
            finalize(lease).await;
            fixture.assert_same_actor_retained();
        }
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn live_process_group_preserves_actor_until_the_captured_group_is_gone() {
    use crate::services::process::{ProcessGroupProbe, process_group_probe};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    with_isolated_runtime_root(|| async {
        let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
        let mut child = ChildGuard(
            Command::new("/bin/sleep")
                .arg("1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        fixture.token.store_child_pid(pid);
        assert_eq!(process_group_probe(pid), ProcessGroupProbe::Live);
        fixture.settle(false).await;
        let retirement = finalize(lease);
        tokio::pin!(retirement);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut retirement)
                .await
                .is_err(),
            "the original callback must wait for its live process group"
        );
        fixture.assert_same_actor_retained();
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "retirement must not terminate the producer"
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(retirement, async {
                while child.0.try_wait().unwrap().is_none() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(process_group_probe(pid), ProcessGroupProbe::Gone);
            });
        })
        .await
        .expect("the single original callback must retire after its own child naturally exits");
        assert!(fixture.shared.mailbox_peek(fixture.channel).is_none());
    })
    .await;
}

#[tokio::test]
async fn delayed_transition_unlock_retries_the_original_completion_callback() {
    with_isolated_runtime_root(|| async {
        let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
        fixture.settle(false).await;
        let transition = fixture
            .shared
            .session_transition_lock(fixture.channel)
            .lock_owned()
            .await;
        let retirement = finalize(lease);
        tokio::pin!(retirement);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut retirement)
                .await
                .is_err(),
            "the original completion callback must retry a busy admission transition"
        );
        fixture.assert_same_actor_retained();
        drop(transition);
        tokio::time::timeout(Duration::from_secs(3), retirement)
            .await
            .expect("unlocking admission must let the original callback remove its actor");
        assert!(fixture.shared.mailbox_peek(fixture.channel).is_none());
    })
    .await;
}

#[tokio::test]
async fn retry_never_removes_a_new_idle_actor_or_a_later_idle_claim() {
    use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;
    with_isolated_runtime_root(|| async {
        for replace_actor in [true, false] {
            let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
            fixture.settle(false).await;
            let transition = fixture
                .shared
                .session_transition_lock(fixture.channel)
                .lock_owned()
                .await;
            let retirement = finalize(lease);
            tokio::pin!(retirement);
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut retirement)
                    .await
                    .is_err()
            );
            let successor_actor = if replace_actor {
                assert_eq!(
                    fixture
                        .shared
                        .mailboxes
                        .remove_idle_entry(fixture.channel)
                        .await,
                    MailboxPurgeOutcome::Removed
                );
                let successor = fixture.shared.mailbox(fixture.channel);
                assert!(!successor.same_actor(&fixture.mailbox));
                successor
            } else {
                let successor = Arc::new(CancelToken::new());
                assert!(
                    fixture
                        .mailbox
                        .try_start_turn(successor.clone(), UserId::new(7), MessageId::new(124),)
                        .await
                );
                successor.mark_completion_cleanup();
                successor.cancelled.store(true, Ordering::Release);
                fixture.finish(&successor).await;
                fixture.mailbox.clone()
            };
            let observed = successor_actor.snapshot().await.claim_observation;
            drop(transition);
            tokio::time::timeout(Duration::from_secs(3), retirement)
                .await
                .expect("a superseded callback must stop rather than recapture the current actor");
            let retained = fixture
                .shared
                .mailbox_peek(fixture.channel)
                .expect("idle successor remains registered");
            assert!(retained.same_actor(&successor_actor));
            let snapshot = retained.snapshot().await;
            assert!(snapshot.cancel_token.is_none());
            assert_eq!(snapshot.claim_observation, observed);
        }
    })
    .await;
}

#[tokio::test]
async fn watcher_admitted_during_retry_preserves_the_original_actor() {
    use crate::services::discord::tmux_watcher_registry::{TmuxWatcherHandle, tmux_watcher_now_ms};
    use std::sync::atomic::{AtomicBool, AtomicI64};
    with_isolated_runtime_root(|| async {
        let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
        fixture.settle(false).await;
        let transition = fixture
            .shared
            .session_transition_lock(fixture.channel)
            .lock_owned()
            .await;
        let retirement = finalize(lease);
        tokio::pin!(retirement);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut retirement)
                .await
                .is_err()
        );
        let name = format!("fixture-watcher-admitted-during-retry-{}", fixture.channel);
        let cancel = Arc::new(AtomicBool::new(false));
        let paused = Arc::new(AtomicBool::new(true));
        let resume = Arc::new(std::sync::Mutex::new(Some(77)));
        fixture.shared.tmux_watchers.insert(
            fixture.channel,
            TmuxWatcherHandle {
                tmux_session_name: name.clone(),
                output_path: format!("/fixture/{name}.jsonl"),
                paused: paused.clone(),
                resume_offset: resume.clone(),
                cancel: cancel.clone(),
                pause_epoch: Arc::new(AtomicU64::new(3)),
                turn_delivered: Arc::new(AtomicBool::new(true)),
                last_heartbeat_ts_ms: Arc::new(AtomicI64::new(tmux_watcher_now_ms())),
            },
        );
        tokio::time::timeout(Duration::from_secs(3), retirement)
            .await
            .expect("watcher admission must stop the original retry while admission remains busy");
        fixture.assert_same_actor_retained();
        let retained = fixture
            .shared
            .tmux_watchers
            .get(&fixture.channel)
            .expect("new watcher remains registered");
        assert_eq!(retained.tmux_session_name, name);
        assert!(Arc::ptr_eq(&retained.cancel, &cancel));
        assert!(Arc::ptr_eq(&retained.paused, &paused));
        assert!(Arc::ptr_eq(&retained.resume_offset, &resume));
        assert!(!cancel.load(Ordering::Acquire));
        assert!(paused.load(Ordering::Acquire));
        assert_eq!(*resume.lock().unwrap(), Some(77));
        drop(retained);
        let (_, removed) = fixture
            .shared
            .tmux_watchers
            .remove(&fixture.channel)
            .expect("remove only the fixture's watcher after verifying it was untouched");
        assert!(Arc::ptr_eq(&removed.cancel, &cancel));
        drop(transition);
        tokio::time::sleep(Duration::from_millis(150)).await;
        fixture.assert_same_actor_retained();
    })
    .await;
}

#[tokio::test]
async fn delayed_episode_state_settlement_retries_the_original_completion_callback() {
    with_isolated_runtime_root(|| async {
        for pending_row in [false, true] {
            let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
            let row = if pending_row {
                fixture.settle(false).await;
                let mut row = inflight::InflightTurnState::new(
                    fixture.shared.provider.clone(),
                    fixture.channel.get(),
                    None,
                    7,
                    123,
                    456,
                    "late episode settlement".into(),
                    None,
                    None,
                    None,
                    None,
                    0,
                );
                row.turn_nonce = fixture.token.turn_nonce().map(str::to_owned);
                row.runtime_kind =
                    Some(crate::services::agent_protocol::RuntimeHandoffKind::ProcessBackend);
                inflight::save_inflight_state(&row).unwrap();
                Some(row)
            } else {
                None
            };
            let retirement = finalize(lease);
            tokio::pin!(retirement);
            assert!(
                tokio::time::timeout(Duration::from_millis(30), &mut retirement)
                    .await
                    .is_err(),
                "the original completion callback must wait for its exact episode to settle"
            );
            fixture.assert_same_actor_retained();
            if let Some(row) = row {
                assert_eq!(
                    inflight::clear_inflight_state_for_captured_episode(
                        &fixture.shared.provider,
                        fixture.channel.get(),
                        &inflight::InflightTurnIdentity::from_state(&row),
                        fixture.token.turn_nonce(),
                    ),
                    inflight::GuardedClearOutcome::Cleared
                );
            } else {
                fixture.settle(false).await;
            }
            tokio::time::timeout(Duration::from_secs(3), retirement)
                .await
                .expect("late episode settlement must let the original callback retire its actor");
            assert!(fixture.shared.mailbox_peek(fixture.channel).is_none());
        }
    })
    .await;
}

#[tokio::test]
async fn settlement_retry_budget_expires_with_the_original_actor_retained() {
    with_isolated_runtime_root(|| async {
        let (fixture, lease) = Fixture::new(ProviderKind::Codex).await;
        // The actor is idle, but its captured token has not received finalizer cleanup yet.
        fixture.finish(&fixture.token).await;
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(7), finalize(lease))
            .await
            .expect(
                "an unsettled episode must not keep its retirement callback alive indefinitely",
            );
        assert!(
            started.elapsed() >= Duration::from_secs(4),
            "the callback must actually retry"
        );
        fixture.assert_same_actor_retained();
        assert!(!fixture.token.cancelled.load(Ordering::Acquire));
        assert!(fixture.mailbox.snapshot().await.cancel_token.is_none());
    })
    .await;
}

#[tokio::test]
async fn capture_requires_fresh_process_routine_metadata_and_the_exact_active_token() {
    with_isolated_runtime_root(|| async {
        let (fixture, _) = Fixture::new(ProviderKind::Codex).await;
        for metadata in [
            None, Some(json!({})), Some(json!({"routine_id": " " , "execution_mode": "process"})),
            Some(json!({"execution_mode": "process"})),
            Some(json!({"routine_id": "x", "execution_strategy": "persistent", "execution_mode": "process"})),
            Some(json!({"routine_id": "x", "execution_strategy": "fresh", "execution_mode": "tmux"})),
            Some(json!({"routine_id": "x", "execution_strategy": "fresh"})),
        ] {
            assert!(capture((&fixture.shared, fixture.channel), &fixture.token, metadata.as_ref()).await.is_none(),
                "ineligible metadata cannot capture an actor: {metadata:?}");
        }
        let unrelated = Arc::new(CancelToken::new());
        assert!(capture((&fixture.shared, fixture.channel), &unrelated, Some(&process_metadata())).await.is_none());
        assert!(capture((&fixture.shared, ChannelId::new(NEXT_CHANNEL.fetch_add(1, Ordering::Relaxed))),
            &fixture.token, Some(&process_metadata())).await.is_none());
        fixture.settle(false).await;
        assert!(capture((&fixture.shared, fixture.channel), &fixture.token, Some(&process_metadata())).await.is_none());
    }).await;
}

#[path = "bridge_tests.rs"]
mod bridge_tests;
