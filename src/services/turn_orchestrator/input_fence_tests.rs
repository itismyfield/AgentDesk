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
fn freeze_barrier_waits_for_actual_queued_writer() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    run(async {
        let channel = ChannelId::new(6_325_204);
        let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
        let _health =
            crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
        let population = fence::population_root().unwrap();
        let sidecar = population.join("discord_inflight/claude/6325204.json.lock");
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        let held = std::fs::File::create(sidecar).unwrap();
        held.lock().unwrap();
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let (send, queued) = tokio::sync::oneshot::channel();
        let observer = Arc::new(std::sync::Mutex::new(Some(send)));
        let observed = observer.clone();
        // The pause is the actor's actual canonical-lock contention, not a timer; its write was
        // admitted while the gate was open.
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
                .enqueue_observed(item(41), context(), None)
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), queued)
            .await
            .unwrap()
            .unwrap();
        let closing = Arc::new(gate.close().unwrap());
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
        held.unlock().unwrap();
        assert!(writer.await.unwrap().enqueued);
        barrier.await.unwrap().unwrap();
        assert_eq!(gate.mode(), Mode::Frozen);
        let (persisted, _) =
            load_channel_pending_queue_for_tests(&ProviderKind::Claude, "fence-test", channel);
        assert_eq!(
            persisted.len(),
            1,
            "the frozen population holds the writer's input"
        );
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
