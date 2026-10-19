use super::*;
use crate::services::turn_orchestrator::test_support::lock_test_env;

const TOKEN: &str = "queued-head-s2a";
const GENERATION: u64 = 5;

struct Root {
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Root {
    fn drop(&mut self) {
        unsafe {
            match &self.previous {
                Some(previous) => std::env::set_var("AGENTDESK_ROOT_DIR", previous),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

fn root() -> Root {
    let lock = lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", dir.path()) };
    Root {
        _dir: dir,
        previous,
        _lock: lock,
    }
}

fn run(work: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(work);
}

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn human(id: u64, author: u64, text: &str) -> Intervention {
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: GENERATION,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            MessageId::new(id),
            GENERATION,
        )],
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: true,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn path(channel: ChannelId) -> std::path::PathBuf {
    pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap()
}

fn bytes(channel: ChannelId) -> Vec<u8> {
    std::fs::read(path(channel)).unwrap()
}

fn disk(channel: ChannelId) -> serde_json::Value {
    serde_json::from_slice(&bytes(channel)).unwrap()
}

fn payloads(mut items: serde_json::Value) -> serde_json::Value {
    for item in items.as_array_mut().unwrap() {
        item.as_object_mut()
            .unwrap()
            .remove("created_at_wall_time_ms");
    }
    items
}

async fn reserve(handle: &ChannelMailboxHandle, head: &Intervention) -> HeadTicket {
    match handle
        .claim_queued_head(QueueIdentity::of(head), ctx())
        .await
    {
        QueuedHeadClaim::Reserved(ticket) => ticket,
        _ => panic!("expected durable head reservation"),
    }
}

async fn may_have_sent(handle: &ChannelMailboxHandle, ticket: HeadTicket) -> HeadTicket {
    match handle
        .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
        .await
    {
        HeadSettle::Committed(Some(ticket)) => ticket,
        _ => panic!("expected durable MayHaveSent"),
    }
}

async fn queue(handle: &ChannelMailboxHandle) -> Vec<(u64, String)> {
    handle
        .snapshot()
        .await
        .intervention_queue
        .into_iter()
        .map(|item| (item.message_id.get(), item.text))
        .collect()
}

#[test]
fn s2a_reserved_disk_mismatch_is_checked_before_orphan_reap() {
    let _root = root();
    run(async {
        for (index, mutation) in [
            "body",
            "generation",
            "primary",
            "zero_channel",
            "zero_author",
        ]
        .into_iter()
        .enumerate()
        {
            let channel = ChannelId::new(6_813_201 + index as u64);
            let head = human(201 + index as u64, 7, "original H");
            let live = ChannelMailboxRegistry::default().handle(channel);
            assert!(live.enqueue(head.clone(), ctx()).await.enqueued);
            let _ticket = reserve(&live, &head).await;
            let mut invalid = disk(channel);
            match mutation {
                "body" => invalid["items"][0]["text"] = "changed B".into(),
                "generation" => {
                    invalid["items"][0]["queued_generation"] = 6.into();
                    invalid["items"][0]["source_message_queued_generations"][0]["queued_generation"] =
                        6.into();
                }
                "zero_channel" => {
                    invalid["items"][0]["channel_id"] = 0.into();
                    invalid["head_injection"]["original"]["channel_id"] = 0.into();
                }
                "zero_author" => {
                    invalid["items"][0]["author_id"] = 0.into();
                    invalid["head_injection"]["original"]["author_id"] = 0.into();
                }
                _ => invalid["items"][0]["message_id"] = 999.into(),
            }
            std::fs::write(path(channel), serde_json::to_vec(&invalid).unwrap()).unwrap();
            let before = bytes(channel);
            let restart = ChannelMailboxRegistry::default().handle(channel);
            let taken = restart.take_next_soft(ctx()).await;
            assert!(taken.intervention.is_none(), "{mutation}");
            assert!(taken.persistence_error.is_some(), "{mutation}");
            let started = restart
                .try_start_turn_kinded_with_persistence(
                    Arc::new(CancelToken::new()),
                    UserId::new(8),
                    MessageId::new(998),
                    ActiveTurnKind::UserOrAgent,
                    TurnAdmissionOrder::Immediate,
                    ctx(),
                )
                .await;
            assert!(!started.started, "{mutation}");
            let enqueued = restart.enqueue(human(997, 8, "new B"), ctx()).await;
            assert!(
                !enqueued.enqueued && enqueued.persistence_error.is_some(),
                "{mutation}"
            );
            assert_eq!(bytes(channel), before, "{mutation}");
        }
    });
}

#[test]
fn s2a_adopted_actor_rechecks_a_corrupt_file_before_other_writes() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_211);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(human(211, 7, "A"), ctx()).await.enqueued);
        std::fs::write(path(channel), "[{").unwrap();
        let taken = handle.take_next_soft(ctx()).await;
        assert!(taken.intervention.is_none());
        assert!(taken.persistence_error.is_some());
        assert!(
            super::super::pending_queue_persistence::load_channel_pending_dispatch_marker_checked(
                &ProviderKind::Claude,
                TOKEN,
                channel,
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(bytes(channel), b"[{");
        let started = handle
            .try_start_turn_kinded_with_persistence(
                Arc::new(CancelToken::new()),
                UserId::new(8),
                MessageId::new(998),
                ActiveTurnKind::UserOrAgent,
                TurnAdmissionOrder::Immediate,
                ctx(),
            )
            .await;
        assert!(!started.started);
        assert!(handle.snapshot().await.cancel_token.is_none());
        assert!(handle.finish_turn(ctx()).await.persistence_error.is_some());
        assert_eq!(bytes(channel), b"[{");
        assert_eq!(queue(&handle).await, [(211, "A".into())]);
        assert!(
            handle
                .restart_drain(ctx())
                .await
                .persistence_error
                .is_some()
        );
        assert_eq!(bytes(channel), b"[{");
    });
}

/// Purge refuses an unresolved head before any mutation; an explicit clear discards it with the
/// queue and the active turn, and the held ticket's later settle revives neither.
#[test]
fn s2a_purge_refuses_an_unresolved_head_and_a_clear_discards_it() {
    let _root = root();
    run(async {
        for (index, phase) in ["reserved", "may_have_sent", "unknown"]
            .into_iter()
            .enumerate()
        {
            let channel = ChannelId::new(6_813_220 + index as u64);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel);
            let token = Arc::new(CancelToken::new());
            handle
                .restore_active_turn(token.clone(), UserId::new(9), MessageId::new(901))
                .await;
            let signal = registry.turn_finished(channel);
            let head = human(220 + index as u64, 7, "H");
            assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
            let ticket = reserve(&handle, &head).await;
            let held = match phase {
                "reserved" => Some(ticket),
                "may_have_sent" => Some(may_have_sent(&handle, ticket).await),
                _ => {
                    assert!(matches!(
                        handle
                            .settle_queued_head(ticket, HeadOutcome::Unconfirmed, ctx())
                            .await,
                        HeadSettle::Unknown
                    ));
                    None
                }
            };
            assert!(
                handle
                    .enqueue(human(230 + index as u64, 8, "B"), ctx())
                    .await
                    .enqueued
            );
            let before_queue = queue(&handle).await;
            let before = bytes(channel);
            let other = path(channel)
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("other-token")
                .join(format!("{}.json", channel.get()));
            std::fs::create_dir_all(other.parent().unwrap()).unwrap();
            std::fs::write(&other, "[]").unwrap();
            super::super::save_channel_pending_dispatch_marker(
                &ProviderKind::Claude,
                TOKEN,
                channel,
                &human(999, 10, "marker"),
                None,
            )
            .unwrap();
            let marker = path(channel).with_extension("dispatch");
            let marker_before = std::fs::read(&marker).unwrap();
            let purged = handle.purge_queue(ctx(), true).await;
            assert!(purged.queued_head_refused, "{phase}");
            assert_eq!((purged.drained, purged.disk_files_removed), (0, 0));
            assert!(!purged.cleared_active_anchor);
            assert_eq!(queue(&handle).await, before_queue, "{phase}");
            assert_eq!(bytes(channel), before, "{phase}");
            assert_eq!(std::fs::read(&marker).unwrap(), marker_before);
            let after = handle.snapshot().await;
            assert!(Arc::ptr_eq(after.cancel_token.as_ref().unwrap(), &token));
            assert_eq!(after.active_user_message_id, Some(MessageId::new(901)));
            assert_eq!(after.active_request_owner, Some(UserId::new(9)));

            let cleared = handle.clear(ctx()).await;
            assert_eq!(cleared.persistence, Default::default(), "{phase}");
            assert!(cleared.removed_token.is_some());
            let (head_id, behind) = (head.message_id, MessageId::new(230 + index as u64));
            let reclaimed = MessageId::new(999);
            for id in [head_id, behind, MessageId::new(901), reclaimed] {
                assert!(cleared.discarded_message_ids.contains(&id), "{phase}: {id}");
            }
            let kinds: Vec<_> = cleared.queue_exit_events.iter().map(|e| e.kind).collect();
            assert_eq!(kinds, [QueueExitKind::Superseded; 3], "{phase}");
            assert!(
                !marker.exists(),
                "{phase}: the marker's input left with the queue"
            );
            assert_eq!(
                bytes(channel),
                b"[]",
                "{phase}: the cleared snapshot is empty"
            );
            assert_eq!(std::fs::read(&other).unwrap(), b"[]");
            tokio::time::timeout(Duration::from_millis(25), signal.wait())
                .await
                .expect("a clear signals the released holder finished");
            if let Some(ticket) = held {
                let outcome = match phase {
                    "reserved" => HeadOutcome::MayHaveSent,
                    _ => HeadOutcome::Injected,
                };
                let settled = handle.settle_queued_head(ticket, outcome, ctx()).await;
                assert!(matches!(settled, HeadSettle::Stale), "{phase}");
            }
            assert!(queue(&handle).await.is_empty(), "{phase}");
            assert_eq!(bytes(channel), b"[]", "{phase}");
            let next = human(240 + index as u64, 8, "next");
            assert!(handle.enqueue(next.clone(), ctx()).await.enqueued);
            let taken = handle.take_next_soft(ctx()).await.intervention;
            assert_eq!(taken.map(|item| item.message_id), Some(next.message_id));
        }
    });
}

#[test]
fn s2a_not_sent_releases_only_the_reservation_and_never_requeues() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_231);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(231, 7, "exact H body");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert!(
            handle
                .enqueue(human(232, 8, "different B body"), ctx())
                .await
                .enqueued
        );
        let before = payloads(disk(channel)["items"].clone());
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::NotSent, ctx())
                .await,
            HeadSettle::Committed(None)
        ));
        assert_eq!(
            queue(&handle).await,
            [
                (231, "exact H body".into()),
                (232, "different B body".into())
            ]
        );
        let after = disk(channel);
        let items = if after.is_array() {
            &after
        } else {
            &after["items"]
        };
        assert_eq!(payloads(items.clone()), before);
        assert!(
            after
                .get("head_injection")
                .is_none_or(serde_json::Value::is_null)
        );
        let _second = reserve(&handle, &head).await;
        assert_eq!(queue(&handle).await.len(), 2);
    });
}

#[test]
fn s2a_save_failure_after_clear_still_signals_the_released_turn() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_280);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        assert!(
            handle
                .enqueue(human(280, 7, "queued B"), ctx())
                .await
                .enqueued
        );
        let holder = MessageId::new(281);
        assert!(
            handle
                .try_start_turn_kinded_with_persistence(
                    Arc::new(CancelToken::new()),
                    UserId::new(8),
                    holder,
                    ActiveTurnKind::UserOrAgent,
                    TurnAdmissionOrder::Immediate,
                    ctx(),
                )
                .await
                .started
        );
        let signal = registry.turn_finished(channel);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), signal.wait())
                .await
                .is_err()
        );
        crate::services::turn_orchestrator::pending_queue_persistence::save_fault::fail_next(
            channel,
        );
        let cleared = handle.clear(ctx()).await;
        assert_eq!(cleared.persistence.retried, ["queue_publish"]);
        assert!(cleared.removed_token.is_some());
        assert_eq!(cleared.discarded_message_ids[0], holder);
        assert!(cleared.discarded_message_ids.contains(&MessageId::new(280)));
        assert!(
            queue(&handle).await.is_empty(),
            "a failed save restores nothing"
        );
        tokio::time::timeout(Duration::from_millis(25), signal.wait())
            .await
            .expect("the released turn must wake its deferred monitor even after save failure");
    });
}

#[test]
fn s2a_terminal_sources_drop_only_the_recorded_generation() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_251);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(251, 7, "submitted H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        let ticket = may_have_sent(&handle, ticket).await;
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::Injected, ctx())
                .await,
            HeadSettle::Committed(None)
        ));
        let mut mixed = human(252, 8, "submitted H\nnew B");
        mixed.queued_generation = GENERATION + 1;
        mixed.source_message_queued_generations[0].queued_generation = GENERATION + 1;
        mixed.source_message_ids.insert(0, head.message_id);
        mixed.source_message_queued_generations.insert(
            0,
            SourceMessageQueuedGeneration::user_instruction(head.message_id, GENERATION),
        );
        mixed.source_text_segments = vec![
            SourceMessageTextSegment::new(head.message_id, "submitted H"),
            SourceMessageTextSegment::new(mixed.message_id, "new B"),
        ];
        super::tests::plant(TOKEN, channel, &mixed);
        assert!(
            handle
                .merge_restored_queue_items(vec![mixed], ctx())
                .await
                .persistence_error
                .is_none()
        );
        let snapshot = handle.snapshot().await;
        let remaining = &snapshot.intervention_queue[0];
        assert_eq!(
            (remaining.message_id.get(), remaining.text.as_str()),
            (252, "new B")
        );
        assert_eq!(remaining.source_message_ids, [MessageId::new(252)]);
        assert_eq!(
            remaining.source_message_queued_generations[0].queued_generation,
            GENERATION + 1
        );
        let mut newer_h = head.clone();
        newer_h.queued_generation = GENERATION + 1;
        newer_h.source_message_queued_generations[0].queued_generation = GENERATION + 1;
        newer_h.text = "new generation H".into();
        assert!(handle.enqueue(newer_h, ctx()).await.enqueued);
        assert_eq!(
            queue(&handle).await,
            [(252, "new B".into()), (251, "new generation H".into())]
        );
    });
}

#[test]
fn s2a_boot_loader_counts_quarantined_channels_even_without_restorable_items() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_261);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(261, 7, "legacy without nonce");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let legacy = bytes(channel);
        let _ticket = reserve(&handle, &head).await;
        let v1 = bytes(channel);
        let parent = path(channel).parent().unwrap().to_owned();
        let fixtures = [
            (6_813_262, legacy.as_slice()),
            (6_813_263, b"{\"version\":2,\"items\":[]}".as_slice()),
            (6_813_264, b"{\"version\":1,\"items\":[".as_slice()),
            (6_813_265, b"[{".as_slice()),
        ];
        for (id, body) in fixtures {
            std::fs::write(parent.join(format!("{id}.json")), body).unwrap();
        }
        let loaded = super::super::pending_queue_persistence::load_pending_queues_checked(
            &ProviderKind::Claude,
            TOKEN,
        );
        assert_eq!(loaded.queues.len(), 2);
        let mut quarantined: Vec<_> = loaded
            .quarantined
            .iter()
            .map(|(id, error)| {
                assert!(!error.is_empty());
                id.get()
            })
            .collect();
        quarantined.sort_unstable();
        assert_eq!(quarantined, [6_813_263, 6_813_264, 6_813_265]);
        assert_eq!(bytes(channel), v1);
        for (id, body) in fixtures {
            assert_eq!(
                std::fs::read(parent.join(format!("{id}.json"))).unwrap(),
                body
            );
        }
        std::fs::remove_file(path(channel)).unwrap();
        std::fs::remove_file(parent.join("6813262.json")).unwrap();
        let only_errors = super::super::pending_queue_persistence::load_pending_queues_checked(
            &ProviderKind::Claude,
            TOKEN,
        );
        assert!(only_errors.queues.is_empty());
        assert_eq!(only_errors.quarantined.len(), 3);
    });
}

/// Metadata changed outside its owner refuses a queue write; an explicit clear replaces it with an
/// empty queue that keeps the submitted sources.
#[test]
fn s2a_changed_metadata_refuses_a_queue_write_and_a_clear_replaces_it() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_271);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(271, 7, "H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let _ticket = reserve(&handle, &head).await;
        let mut changed = disk(channel);
        changed["head_injection"] = serde_json::Value::Null;
        changed["terminal_identities"] =
            serde_json::to_value(vec![QueueIdentity::of(&head)]).unwrap();
        std::fs::write(path(channel), serde_json::to_vec(&changed).unwrap()).unwrap();
        let before = bytes(channel);
        assert!(
            handle
                .enqueue(human(272, 8, "B"), ctx())
                .await
                .persistence_error
                .is_some()
        );
        assert_eq!(bytes(channel), before);
        let cleared = handle.clear(ctx()).await;
        assert_eq!(cleared.persistence, Default::default());
        assert!(cleared.discarded_message_ids.contains(&head.message_id));
        let written = disk(channel);
        assert_eq!(written["items"], serde_json::json!([]));
        assert!(written["head_injection"].is_null());
        assert_eq!(
            written["terminal_identities"],
            changed["terminal_identities"]
        );
    });
}

#[test]
fn s2a_force_purge_refusal_does_not_finalize_a_cancelled_holder() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_281);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let token = Arc::new(CancelToken::new());
        handle
            .restore_active_turn(token.clone(), UserId::new(9), MessageId::new(901))
            .await;
        let head = human(281, 7, "H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::Unconfirmed, ctx())
                .await,
            HeadSettle::Unknown
        ));
        assert!(handle.enqueue(human(282, 8, "B"), ctx()).await.enqueued);
        token.cancelled.store(true, Ordering::Relaxed);
        let before = bytes(channel);
        let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let key = format!("claude/{TOKEN}/test:inactive-fixture");
        assert!(
            crate::services::queue::force_purge_channel_mailbox(None, &target, Some(&key))
                .await
                .is_none()
        );
        assert_eq!(bytes(channel), before);
        assert_eq!(queue(&handle).await, [(281, "H".into()), (282, "B".into())]);
        assert!(Arc::ptr_eq(
            handle.snapshot().await.cancel_token.as_ref().unwrap(),
            &token
        ));
    });
}

#[test]
fn s2a_actorless_purge_preserves_valid_unknown_and_terminal_namespaces() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_291);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let head = human(291, 7, "H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::Unconfirmed, ctx())
                .await,
            HeadSettle::Unknown
        ));
        let unknown = bytes(channel);
        let terminal = serde_json::to_vec(&serde_json::json!({
            "version": 1, "items": [], "terminal_identities": [QueueIdentity::of(&head)],
        }))
        .unwrap();
        let base = path(channel).parent().unwrap().parent().unwrap().to_owned();
        let file = |token: &str| base.join(token).join(format!("{}.json", channel.get()));
        let fixtures: [(&str, &[u8]); 4] = [
            ("terminal", &terminal),
            ("broken", b"[{"),
            ("torn", b"{\"version\":1,\"items\":["),
            ("legacy", b"[]"),
        ];
        for (token, body) in fixtures {
            std::fs::create_dir_all(file(token).parent().unwrap()).unwrap();
            std::fs::write(file(token), body).unwrap();
        }
        registry.remove_fixture_for_test(channel);
        assert!(ChannelMailboxRegistry::global_handle(channel).is_none());
        let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let purged = crate::services::queue::force_purge_channel_mailbox(None, &target, None)
            .await
            .unwrap();
        assert_eq!(purged.disk_files_removed, 1);
        assert_eq!(bytes(channel), unknown);
        for (token, body) in fixtures.into_iter().filter(|(token, _)| *token != "legacy") {
            assert_eq!(std::fs::read(file(token)).unwrap(), body);
        }
        assert!(!file("legacy").exists());
    });
}

#[test]
fn s2a_a_missing_legacy_source_clock_does_not_change_the_frozen_payload() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_301);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let mut head = human(301, 7, "no nonce or source clock");
        head.source_message_queued_generations[0].enqueued_at_epoch_us = None;
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        let ticket = may_have_sent(&handle, ticket).await;
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::Injected, ctx())
                .await,
            HeadSettle::Committed(None)
        ));
        assert!(queue(&handle).await.is_empty());
    });
}

#[test]
fn s2a_generation_replacement_before_and_after_claim_cannot_change_its_owner() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_311);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let first = human(311, 7, "old generation");
        assert!(handle.enqueue(first.clone(), ctx()).await.enqueued);
        let expected = QueueIdentity::of(&first);
        assert!(
            handle
                .cancel_queued_primary_message(first.message_id, ctx())
                .await
                .removed
                .is_some()
        );
        let mut replacement = first.clone();
        replacement.queued_generation += 1;
        replacement.source_message_queued_generations[0].queued_generation += 1;
        replacement.text = "new generation".into();
        assert!(handle.enqueue(replacement.clone(), ctx()).await.enqueued);
        assert!(matches!(
            handle.claim_queued_head(expected, ctx()).await,
            QueuedHeadClaim::Contended
        ));
        let ticket = reserve(&handle, &replacement).await;
        let original = disk(channel)["head_injection"]["original"].clone();
        let mut late = replacement.clone();
        late.queued_generation += 1;
        late.source_message_queued_generations[0].queued_generation += 1;
        late.text = "late replacement".into();
        assert!(!handle.enqueue(late, ctx()).await.enqueued);
        assert_eq!(disk(channel)["head_injection"]["original"], original);
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::NotSent, ctx())
                .await,
            HeadSettle::Committed(None)
        ));
        assert_eq!(queue(&handle).await, [(311, "new generation".into())]);
    });
}

#[test]
fn s2a_missing_source_clocks_restore_a_valid_unknown_head_and_tail() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_321);
        let live = ChannelMailboxRegistry::default().handle(channel);
        let mut head = human(321, 7, "H without source clock");
        head.source_message_queued_generations[0].enqueued_at_epoch_us = None;
        assert!(live.enqueue(head.clone(), ctx()).await.enqueued);
        let ticket = reserve(&live, &head).await;
        let ticket = may_have_sent(&live, ticket).await;
        assert!(live.enqueue(human(322, 8, "B"), ctx()).await.enqueued);
        drop(ticket);
        let restart = ChannelMailboxRegistry::default().handle(channel);
        let taken = restart.take_next_soft(ctx()).await;
        assert!(taken.intervention.is_none());
        assert!(taken.persistence_error.is_none());
        assert!(restart.enqueue(human(323, 9, "C"), ctx()).await.enqueued);
        assert_eq!(
            queue(&restart).await,
            [
                (321, "H without source clock".into()),
                (322, "B".into()),
                (323, "C".into()),
            ]
        );
        assert_eq!(disk(channel)["head_injection"]["phase"], "unknown");
        assert!(restart.take_next_soft(ctx()).await.intervention.is_none());
    });
}

#[test]
fn s2a_changed_metadata_is_checked_before_a_dispatch_marker_is_written() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_331);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(331, 7, "A");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        let changed = serde_json::json!({
            "version": 1,
            "items": disk(channel),
            "terminal_identities": [QueueIdentity::of(&head)],
        });
        std::fs::write(path(channel), serde_json::to_vec(&changed).unwrap()).unwrap();
        let before = bytes(channel);
        let taken = handle.take_next_soft(ctx()).await;
        assert!(taken.intervention.is_none());
        assert!(taken.dispatch_lease.is_none());
        assert!(taken.persistence_error.is_some());
        assert!(!path(channel).with_extension("dispatch").exists());
        assert_eq!(bytes(channel), before);
        assert_eq!(queue(&handle).await, [(331, "A".into())]);
    });
}

#[test]
fn s2a_published_terminal_survives_a_returned_save_error() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_341);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(341, 7, "H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        assert!(handle.enqueue(human(342, 8, "B"), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        let ticket = may_have_sent(&handle, ticket).await;
        super::super::pending_queue_persistence::post_save_fault::fail_next(channel);
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::Injected, ctx())
                .await,
            HeadSettle::Unknown
        ));
        let committed = bytes(channel);
        assert!(disk(channel)["head_injection"].is_null());
        assert_eq!(disk(channel)["terminal_identities"][0]["primary"], 341);
        assert!(
            handle
                .enqueue(human(343, 9, "C"), ctx())
                .await
                .persistence_error
                .is_some()
        );
        assert_eq!(bytes(channel), committed);
        let restart = ChannelMailboxRegistry::default().handle(channel);
        assert!(restart.enqueue(human(343, 9, "C"), ctx()).await.enqueued);
        assert_eq!(
            queue(&restart).await,
            [(342, "B".into()), (343, "C".into())]
        );
        let taken = restart.take_next_soft(ctx()).await;
        assert_eq!(taken.intervention.unwrap().message_id.get(), 342);
    });
}

#[test]
fn s2a_downgrade_refuses_a_resolved_snapshot_with_a_dispatch_marker() {
    let _root = root();
    let channel = ChannelId::new(6_813_351);
    let head = human(351, 7, "resolved H");
    std::fs::create_dir_all(path(channel).parent().unwrap()).unwrap();
    std::fs::write(
        path(channel),
        serde_json::to_vec(&serde_json::json!({
            "version": 1, "items": [], "terminal_identities": [QueueIdentity::of(&head)],
        }))
        .unwrap(),
    )
    .unwrap();
    super::super::save_channel_pending_dispatch_marker(
        &ProviderKind::Claude,
        TOKEN,
        channel,
        &human(352, 8, "dispatch B"),
        None,
    )
    .unwrap();
    let snapshot = bytes(channel);
    let marker = path(channel).with_extension("dispatch");
    let marker_bytes = std::fs::read(&marker).unwrap();
    assert!(downgrade_channel_snapshot(&ProviderKind::Claude, TOKEN, channel, |_| true).is_err());
    assert_eq!(bytes(channel), snapshot);
    assert_eq!(std::fs::read(marker).unwrap(), marker_bytes);
}

#[test]
fn s2a_unknown_head_cannot_be_skipped_to_dispatch_its_tail() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_361);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let head = human(361, 7, "UNKNOWN H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        assert!(handle.enqueue(human(362, 8, "B"), ctx()).await.enqueued);
        let ticket = reserve(&handle, &head).await;
        assert!(matches!(
            handle
                .settle_queued_head(ticket, HeadOutcome::Unconfirmed, ctx())
                .await,
            HeadSettle::Unknown
        ));
        let before = bytes(channel);
        let taken = handle
            .take_soft_matching(ctx(), Some(MessageId::new(362)))
            .await;
        assert!(taken.intervention.is_none());
        assert!(taken.dispatch_lease.is_none());
        assert!(taken.persistence_error.is_none());
        assert!(!path(channel).with_extension("dispatch").exists());
        assert_eq!(bytes(channel), before);
        assert_eq!(
            queue(&handle).await,
            [(361, "UNKNOWN H".into()), (362, "B".into())]
        );
    });
}
