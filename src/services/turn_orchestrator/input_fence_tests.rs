use super::super::closed_verdict::MailboxRefusal;
use super::*;
struct Env(Option<std::ffi::OsString>);
impl Env {
    fn set(path: &std::path::Path) -> Self {
        let old = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", path);
        }
        Self(old)
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(old) => std::env::set_var("AGENTDESK_ROOT_DIR", old),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}
use crate::services::discord::input_runtime::fence::{Gate, Mode};

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("input {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}
fn context() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "fence-test", None)
}
fn run(work: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}

#[test]
fn c1_closed_retry_preserves_admission_when_closing_lands_between_actors() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_430);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let registry = Arc::new(ChannelMailboxRegistry::default());
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let stale = ChannelMailboxHandle {
            sender,
            recovery_done: Arc::new(RecoveryDoneSignal::new()),
        };
        registry.handles.insert(channel, stale.clone());
        GLOBAL_CHANNEL_MAILBOXES.insert(channel, stale);
        let (closing_tx, closing_rx) = tokio::sync::oneshot::channel();
        let actor_registry = registry.clone();
        let actor_gate = gate.clone();
        let fixture = tokio::spawn(async move {
            let (msg, _) = receiver.recv().await.unwrap();
            let ChannelMailboxMsg::Enqueue {
                reply,
                input_permit,
                ..
            } = msg
            else {
                panic!("enqueue expected")
            };
            input_permit
                .as_ref()
                .unwrap()
                .validate(&ProviderKind::Claude, channel.get())
                .unwrap();
            let closing = actor_gate.close().unwrap();
            actor_registry.remove_fixture_for_test(channel);
            closing_tx
                .send(closing)
                .unwrap_or_else(|_| panic!("closing receiver lost"));
            let _ = reply.send(EnqueueInterventionResult::refused(
                EnqueueRefusalReason::MailboxClosed,
                Vec::new(),
            ));
        });
        let result = registry
            .enqueue_with_closed_retry(channel, item(91), context(), None)
            .await;
        assert!(
            result.enqueued,
            "retry uses preclose permit on the fresh actor: {:?}",
            result.refusal_reason
        );
        assert!(result.persistence_error.is_none());
        let closing = closing_rx.await.unwrap();
        closing.drain().await;
        assert!(matches!(
            gate.admit(),
            Err(fence::Failure::Mode(Mode::Closing))
        ));
        let snapshot = registry.handle(channel).snapshot().await;
        assert_eq!(snapshot.intervention_queue.len(), 1);
        assert_eq!(
            snapshot.intervention_queue[0].message_id,
            MessageId::new(91)
        );
        let disk = root
            .path()
            .join("runtime/discord_pending_queue/claude/fence-test/6325430.json");
        assert!(
            std::fs::read(disk)
                .unwrap()
                .windows(2)
                .any(|bytes| bytes == b"91")
        );
        fixture.await.unwrap();
        registry.remove_fixture_for_test(channel);
    });
}

#[test]
fn c1_inherited_effect_restores_and_finishes_durably_during_closing() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_406);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        fence::effect::scope(Some(gate.admit().unwrap()), async {
            assert!(handle.enqueue(item(61), context()).await.enqueued);
            let taken = handle.take_next_soft(context()).await;
            let closing = gate.close().unwrap();
            assert!(
                handle
                    .restore_dequeued_head(
                        taken.intervention.unwrap(),
                        context(),
                        taken.dispatch_lease.unwrap()
                    )
                    .await
                    .enqueued
            );
            assert!(
                handle
                    .finish_turn(context())
                    .await
                    .persistence_error
                    .is_none()
            );
            let disk = root
                .path()
                .join("runtime/discord_pending_queue/claude/fence-test/6325406.json");
            assert!(
                std::fs::read(disk)
                    .unwrap()
                    .windows(2)
                    .any(|part| part == b"61")
            );
            let drain = closing.drain();
            tokio::pin!(drain);
            assert!(futures::poll!(drain.as_mut()).is_pending());
        })
        .await;
        assert_eq!(
            handle.enqueue(item(62), context()).await.refusal_reason,
            Some(EnqueueRefusalReason::InputModeFenced(Mode::Closing))
        );
    });
}

#[test]
fn c1_cancelled_mailbox_release_does_not_end_residual_bridge_effect() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_412);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let token = Arc::new(CancelToken::new());
        handle
            .restore_active_turn(token.clone(), UserId::new(7), MessageId::new(70))
            .await;
        assert!(handle.has_active_turn().await.unwrap());
        let permit = gate.admit().unwrap();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let bridge = tokio::spawn(fence::effect::detached(Some(permit.clone()), async move {
            entered_tx.send(()).unwrap();
            release_rx.await.unwrap();
            assert!(fence::effect::current().is_some());
        }));
        entered_rx.await.unwrap();
        fence::effect::scope(Some(permit.clone()), async {
            token.publish_cancel("c1 residual bridge fixture".to_owned());
            assert!(handle.finish_cancelled_turn().await.removed_token.is_some());
        })
        .await;
        assert!(!handle.has_active_turn().await.unwrap());
        drop(permit);
        let closing = gate.close().unwrap();
        let drain = closing.drain();
        tokio::pin!(drain);
        assert!(
            futures::poll!(drain.as_mut()).is_pending(),
            "slot release is not bridge IO completion"
        );
        release_tx.send(()).unwrap();
        bridge.await.unwrap();
        drain.await;
    });
}

#[test]
fn actual_actor_barrier_preserves_queue_and_refuses_enqueue_kickoff_finish_and_take() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_201);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(item(10), context()).await.enqueued);
        let closing = Arc::new(gate.close().unwrap());
        closing.drain().await;
        let ack = handle
            .freeze_input(closing.clone(), context())
            .await
            .unwrap();
        closing.freeze(ack).unwrap();
        assert_eq!(gate.mode(), Mode::Frozen);
        let path = root
            .path()
            .join("runtime/discord_pending_queue/claude/fence-test/6325201.json");
        let before = std::fs::read(&path).unwrap();
        let refused = handle.enqueue(item(11), context()).await;
        assert_eq!(
            refused.refusal_reason,
            Some(EnqueueRefusalReason::InputModeFenced(Mode::Frozen))
        );
        assert!(!refused.enqueued, "post-freeze mutation forbidden");
        assert_eq!(
            handle
                .recovery_kickoff(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    Some(MessageId::new(12))
                )
                .await,
            RecoveryKickoffResult::InputModeFenced(Mode::Frozen)
        );
        assert!(!handle.has_active_turn().await.unwrap());
        assert!(
            handle
                .finish_turn(context())
                .await
                .persistence_error
                .is_some()
        );
        assert!(matches!(
            handle.take_soft_matching_or_refused(context(), None).await,
            Err(MailboxRefusal::InputFenced(Failure::Mode(Mode::Frozen)))
        ));
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert_eq!(handle.snapshot().await.intervention_queue.len(), 1);
    });
}

#[test]
fn preclose_permit_enqueues_and_durable_requeues_while_new_work_is_refused() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_202);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let enqueue = gate.admit().unwrap();
        let requeue = gate.admit().unwrap();
        let closing = gate.close().unwrap();
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(
            handle
                .enqueue_observed_with_permit(item(21), context(), None, Some(enqueue))
                .await
                .enqueued
        );
        assert!(
            handle
                .requeue_front_with_permit(item(22), context(), None, Some(requeue))
                .await
                .enqueued
        );
        closing.drain().await;
        assert_eq!(
            handle.enqueue(item(23), context()).await.refusal_reason,
            Some(EnqueueRefusalReason::InputModeFenced(Mode::Closing))
        );
        assert_eq!(handle.snapshot().await.intervention_queue.len(), 2);
        let channel = ChannelId::new(6_325_207);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let candidate = Arc::new(CancelToken::new());
        let kickoff = handle.recovery_kickoff(candidate, UserId::new(7), Some(MessageId::new(24)));
        tokio::pin!(kickoff);
        // Enqueue the actual request before close without yielding to the actor.
        assert!(futures::poll!(kickoff.as_mut()).is_pending());
        let closing = gate.close().unwrap();
        assert_eq!(
            kickoff.await,
            RecoveryKickoffResult::InputModeFenced(Mode::Closing)
        );
        assert!(handle.snapshot().await.cancel_token.is_none());
        closing.drain().await;
    });
}

#[test]
fn off_path_has_no_sidecar_and_registry_does_not_remint_on_fence() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(6_325_203);
        assert!(
            registry
                .enqueue_with_closed_retry(channel, item(31), context(), None)
                .await
                .enqueued
        );
        assert!(fence::lookup(&ProviderKind::Claude, channel.get()).is_none());
        assert!(
            !root.path().join("runtime/discord_inflight").exists(),
            "off must not acquire a new lock"
        );
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let _closing = gate.close().unwrap();
        let result = registry
            .enqueue_with_closed_retry(channel, item(32), context(), None)
            .await;
        assert_eq!(
            result.refusal_reason,
            Some(EnqueueRefusalReason::InputModeFenced(Mode::Closing))
        );
        assert_eq!(
            registry
                .handle(channel)
                .snapshot()
                .await
                .intervention_queue
                .len(),
            1
        );
    });
}

#[test]
fn b2_late_protection_between_actor_selection_and_enter_has_no_scheduler_io() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_304);
        assert!(fence::channel_gate(channel.get()).is_none());
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let state = ChannelMailboxState::default();
        let (reply, _) = tokio::sync::oneshot::channel();
        let mut msg = ChannelMailboxMsg::RestartDrain {
            persistence: context(),
            reply,
        };
        assert!(matches!(
            enter(channel, &state, &mut msg),
            Err(Failure::Busy)
        ));
        assert!(!root.path().join("runtime").exists());
        assert_eq!(
            tokio::spawn(async {
                tokio::task::yield_now().await;
                1
            })
            .await
            .unwrap(),
            1
        );
        assert_eq!(gate.mode(), Mode::LegacyOpen);
    });
}

#[test]
fn b2_freeze_refuses_other_token_queue_or_marker_without_ack_or_mutation() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        for (index, extension) in ["json", "dispatch"].into_iter().enumerate() {
            let channel = ChannelId::new(6_325_305 + index as u64);
            let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
            let _health =
                crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
            let closing = Arc::new(gate.close().unwrap());
            let handle = ChannelMailboxRegistry::default().handle(channel);
            let path = fence::population_root().unwrap().join(format!(
                "discord_pending_queue/claude/other/{}.{extension}",
                channel.get()
            ));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"other-token-population").unwrap();
            assert!(matches!(
                handle.freeze_input(closing.clone(), context()).await,
                Err(Failure::Busy)
            ));
            assert_eq!(gate.mode(), Mode::Closing);
            assert_eq!(std::fs::read(&path).unwrap(), b"other-token-population");
            assert!(
                !path
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join(format!("fence-test/{}.json", channel.get()))
                    .exists()
            );
            std::fs::remove_file(path).unwrap();
            let ack = handle
                .freeze_input(closing.clone(), context())
                .await
                .unwrap();
            closing.freeze(ack).unwrap();
            assert_eq!(gate.mode(), Mode::Frozen);
        }
    });
}

#[test]
fn ownership_transferred_consumers_surface_fence_as_failure() {
    for failure in [
        Failure::ActorUnreachable,
        Failure::LockTimeout,
        Failure::Persistence,
    ] {
        assert!(!matches!(
            enqueue_reason(failure),
            EnqueueRefusalReason::InputModeFenced(_)
        ));
    }
}

#[test]
fn freeze_barrier_waits_for_actual_queued_writer_and_preserves_latest_disk_population() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_204);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let permit = gate.admit().unwrap();
        let closing = Arc::new(gate.close().unwrap());
        let population = fence::population_root().unwrap();
        let sidecar = population.join("discord_inflight/claude/6325204.json.lock");
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        let held = std::fs::File::create(sidecar).unwrap();
        held.lock().unwrap();
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let (send, queued) = tokio::sync::oneshot::channel();
        let observer = Arc::new(std::sync::Mutex::new(Some(send)));
        let observed = observer.clone();
        // The pause is the actor's actual canonical-lock contention, not a timer.
        fence::install_wait_observer(
            channel.get(),
            Box::new(move || {
                if let Some(send) = observed.lock().unwrap().take() {
                    let _ = send.send(());
                }
            }),
        );
        let writer_handle = handle.clone();
        let writer = tokio::spawn(async move {
            writer_handle
                .enqueue_observed_with_permit(item(41), context(), None, Some(permit))
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .unwrap()
            .unwrap();
        let barrier_handle = handle.clone();
        let barrier_closing = closing.clone();
        let barrier = tokio::spawn(async move {
            barrier_closing.drain().await;
            let ack = barrier_handle
                .freeze_input(barrier_closing.clone(), context())
                .await?;
            barrier_closing.freeze(ack)
        });
        assert_eq!(
            tokio::spawn(async {
                tokio::task::yield_now().await;
                1
            })
            .await
            .unwrap(),
            1
        );
        assert!(!writer.is_finished());
        assert!(!barrier.is_finished());
        // An independent pre-existing writer left a disk-only item before release.
        let queue = population.join("discord_pending_queue/claude/fence-test/6325204.json");
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        let wire = serde_json::json!({"author_id":7,"message_id":40,"source_message_ids":[40],"text":"input 40","channel_id":channel.get()});
        std::fs::write(&queue, serde_json::to_vec(&vec![wire]).unwrap()).unwrap();
        held.unlock().unwrap();
        assert!(writer.await.unwrap().enqueued);
        barrier.await.unwrap().unwrap();
        let before = std::fs::read(&queue).unwrap();
        assert_eq!(handle.snapshot().await.intervention_queue.len(), 2);
        let purge = handle.try_purge_queue(context(), false).await.unwrap();
        assert_eq!(purge.input_refusal, Some(Failure::Mode(Mode::Frozen)));
        assert_eq!(purge.drained, 0);
        assert_eq!(std::fs::read(queue).unwrap(), before);
    });
}

#[test]
fn freeze_rejects_active_lease_malformed_snapshot_and_wrong_identity_without_mutation() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_205);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(
            handle
                .try_start_turn(
                    Arc::new(CancelToken::new()),
                    UserId::new(7),
                    MessageId::new(51)
                )
                .await
        );
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let closing = Arc::new(gate.close().unwrap());
        assert!(matches!(
            handle.freeze_input(closing, context()).await,
            Err(Failure::Busy)
        ));
        assert!(handle.snapshot().await.cancel_token.is_some());
        let channel = ChannelId::new(6_325_208);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(item(52), context()).await.enqueued);
        let taken = handle.take_next_soft(context()).await;
        assert!(taken.dispatch_lease.is_some());
        let before = handle.snapshot().await;
        assert_eq!(before.pending_user_dispatch, Some(MessageId::new(52)));
        assert!(before.pending_user_dispatch_lease_held_by_caller);
        let marker = fence::population_root()
            .unwrap()
            .join("discord_pending_queue/claude/fence-test/6325208.dispatch");
        let bytes = std::fs::read(&marker).unwrap();
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let closing = Arc::new(gate.close().unwrap());
        assert!(matches!(
            handle.freeze_input(closing, context()).await,
            Err(Failure::Busy)
        ));
        assert_eq!(
            handle.snapshot().await.pending_user_dispatch,
            before.pending_user_dispatch
        );
        assert_eq!(std::fs::read(marker).unwrap(), bytes);
        drop(taken);
        let channel = ChannelId::new(6_325_209);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
            "successor".into(),
        )));
        assert!(
            handle
                .try_start_turn(successor.clone(), UserId::new(7), MessageId::new(53))
                .await
        );
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let _closing = gate.close().unwrap();
        let finish = handle
            .finish_turn_if_matches_episode_started_before(
                MessageId::new(53),
                Some("predecessor".into()),
                Instant::now(),
                context(),
            )
            .await;
        assert!(finish.removed_token.is_none());
        assert!(finish.persistence_error.is_some());
        assert!(Arc::ptr_eq(
            &handle.cancel_token().await.unwrap().unwrap(),
            &successor
        ));
        assert_eq!(
            handle.snapshot().await.active_turn_nonce.as_deref(),
            Some("successor")
        );
        let channel = ChannelId::new(6_325_206);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let closing = Arc::new(gate.close().unwrap());
        let queue = fence::population_root()
            .unwrap()
            .join("discord_pending_queue/claude/fence-test/6325206.json");
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        std::fs::write(&queue, b"invalid").unwrap();
        assert!(matches!(
            handle.freeze_input(closing.clone(), context()).await,
            Err(Failure::Persistence)
        ));
        assert_eq!(std::fs::read(&queue).unwrap(), b"invalid");
        let wrong = QueuePersistenceContext::new(&ProviderKind::Codex, "fence-test", None);
        assert!(matches!(
            handle.freeze_input(closing.clone(), wrong).await,
            Err(Failure::Busy)
        ));
        assert!(
            !fence::population_root()
                .unwrap()
                .join("discord_inflight/codex")
                .exists()
        );
        std::fs::remove_file(&queue).unwrap();
        let ack = handle
            .freeze_input(closing.clone(), context())
            .await
            .unwrap();
        closing.freeze(ack).unwrap();
    });
}

#[test]
fn c2_restart_drain_leaves_a_held_channel_alone_and_still_persists_legacy() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let (held, legacy) = (ChannelId::new(6_325_540), ChannelId::new(6_325_541));
        let gate = Gate::protect(ProviderKind::Claude, held.get()).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let registry = ChannelMailboxRegistry::default();
        registry
            .handle(held)
            .replace_queue(vec![item(6_325_542)], context())
            .await;
        registry
            .handle(legacy)
            .replace_queue(vec![item(6_325_543)], context())
            .await;
        let queue = |channel: ChannelId| {
            fence::population_root().unwrap().join(format!(
                "discord_pending_queue/claude/fence-test/{}.json",
                channel.get()
            ))
        };
        std::fs::remove_file(queue(legacy)).unwrap();
        let held_before = std::fs::read(queue(held)).unwrap();
        let closing = gate.close().unwrap();

        // Deferred restart and standby promotion retain the runtime on any drain error.
        let drain = registry
            .restart_drain_all(
                &ProviderKind::Claude,
                "fence-test",
                &dashmap::DashMap::new(),
            )
            .await;
        assert!(
            drain.persistence_errors.is_empty(),
            "{:?}",
            drain.persistence_errors
        );
        assert_eq!(drain.queued_count, 1);
        assert_eq!(std::fs::read(queue(held)).unwrap(), held_before);
        let (persisted, _) =
            load_channel_pending_queue_for_tests(&ProviderKind::Claude, "fence-test", legacy);
        assert_eq!(persisted.len(), 1);
        assert_eq!(gate.mode(), Mode::Closing);
        closing.drain().await;
    });
}
