use super::*;

#[tokio::test]
async fn closing_drains_only_existing_permits_and_rejects_redirect() {
    let gate = Gate::protect(ProviderKind::Claude, 6_325_101).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    let closing = gate.close().unwrap();
    assert!(matches!(gate.admit(), Err(Failure::Mode(Mode::Closing))));
    assert_eq!(permit.validate(&ProviderKind::Claude, 6_325_101), Ok(()));
    assert_eq!(
        permit.validate(&ProviderKind::Codex, 6_325_101),
        Err(Failure::StalePermit)
    );
    assert_eq!(
        permit.validate(&ProviderKind::Claude, 6_325_102),
        Err(Failure::StalePermit)
    );
    let drain = closing.drain();
    tokio::pin!(drain);
    assert!(
        futures::poll!(drain.as_mut()).is_pending(),
        "prepare-before-drain is forbidden"
    );
    drop(permit);
    drain.await;
}

#[test]
fn canonical_borrowing_and_supervisor_contention_never_prepare() {
    let root = tempfile::tempdir().unwrap();
    let gate = Gate::protect(ProviderKind::Claude, 6_325_103).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    let guard = closing.population(root.path()).unwrap();
    assert!(matches!(
        closing.population(root.path()),
        Err(Failure::Busy)
    ));
    let mut calls = 0;
    guard
        .borrowed(root.path(), &ProviderKind::Claude, 6_325_103, || {
            calls += 1;
            Ok(())
        })
        .unwrap();
    assert!(
        guard
            .borrowed(root.path(), &ProviderKind::Codex, 6_325_103, || {
                calls += 1;
                Ok(())
            })
            .is_err()
    );
    assert!(
        guard
            .borrowed(root.path(), &ProviderKind::Claude, 6_325_104, || {
                calls += 1;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(calls, 1);
    drop(guard);
    assert!(
        root.path()
            .join("discord_inflight/claude/6325103.json.lock")
            .exists(),
        "sidecar must never unlink"
    );
    assert!(closing.population(root.path()).is_ok());
}

#[test]
fn ordinary_writer_waits_for_release_and_timeout_is_typed() {
    let root = tempfile::tempdir().unwrap();
    let gate = Gate::protect(ProviderKind::Claude, 6_325_105).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    let held = PopulationGuard::try_acquire(root.path(), &ProviderKind::Claude, 6_325_105).unwrap();
    let mut held = Some(held);
    let start = Instant::now();
    let mut waits = 0;
    let guard = PopulationGuard::writer_wait(
        root.path(),
        &ProviderKind::Claude,
        6_325_105,
        &permit,
        || start,
        || {
            waits += 1;
            drop(held.take());
        },
    )
    .unwrap();
    assert_eq!(waits, 1, "first contention must wait, not Held");
    drop(guard);
    let _held =
        PopulationGuard::try_acquire(root.path(), &ProviderKind::Claude, 6_325_105).unwrap();
    let mut clock = 0;
    let result = PopulationGuard::writer_wait(
        root.path(),
        &ProviderKind::Claude,
        6_325_105,
        &permit,
        || {
            clock += 1;
            start
                + if clock == 1 {
                    Duration::ZERO
                } else {
                    WRITE_LOCK_DEADLINE
                }
        },
        || panic!("deadline reached"),
    );
    assert!(matches!(result, Err(Failure::LockTimeout)));
    assert_eq!(WRITE_LOCK_DEADLINE, Duration::from_secs(3));
}

#[test]
fn stale_epoch_is_not_mode_or_duplicate() {
    let gate = Gate::protect(ProviderKind::Claude, 6_325_106).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    gate.state.lock().unwrap().epoch += 1;
    assert_eq!(
        permit.validate(&ProviderKind::Claude, 6_325_106),
        Err(Failure::StalePermit)
    );
}

#[test]
fn off_lookup_never_waits_for_gate_state_and_handback_can_release_and_restore() {
    let gate = Gate::protect(ProviderKind::Claude, 6_325_107).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    assert_eq!(
        closing.release_protection_after_handback(),
        Err(Failure::Busy)
    );
    gate.record_failure(&[], Failure::Mode(Mode::Closing));
    assert!(
        health_reasons()
            .iter()
            .any(|reason| reason.contains("channel=6325107"))
    );
    gate.state.lock().unwrap().mode = Mode::Handback;
    closing.release_protection_after_handback().unwrap();
    assert!(
        !health_reasons()
            .iter()
            .any(|reason| reason.contains("channel=6325107"))
    );
    let state = gate.state.lock().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        send.send((
            lookup(&ProviderKind::Claude, 6_325_107).is_none(),
            channel_gate(6_325_107).is_none(),
            lookup(&ProviderKind::Claude, 6_325_999).is_none(),
        ))
        .unwrap();
    });
    let result = receive.recv_timeout(Duration::from_secs(1));
    drop(state);
    worker.join().unwrap();
    assert_eq!(
        result.unwrap(),
        (true, true, true),
        "off lookup must not wait for a gate mutex"
    );
    gate.restore_protection().unwrap();
    assert!(Arc::ptr_eq(
        &gate,
        &lookup(&ProviderKind::Claude, 6_325_107).unwrap()
    ));
    assert!(matches!(gate.admit(), Ok(_)));
    assert_eq!(
        closing.release_protection_after_handback(),
        Err(Failure::Busy)
    );
}

#[test]
fn borrowed_handback_reads_latest_bytes_without_reacquiring_and_rejects_redirect() {
    use crate::services::tui_input::handover::EnqueueOutcome;
    use crate::services::turn_orchestrator::input_handback::{
        Destination, enqueue, enqueue_borrowed,
    };
    let root = tempfile::tempdir().unwrap();
    let channel = 6_325_108;
    let destination = Destination {
        root: root.path(),
        provider: &ProviderKind::Claude,
        token_hash: "borrowed",
        channel,
        authorized: true,
        active_sources: &[],
    };
    let item = |id| serde_json::json!({"author_id":7,"message_id":id,"source_message_ids":[id],"text":"same","channel_id":channel});
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    let guard = closing.population(root.path()).unwrap();
    assert!(enqueue(&destination, &item(2)).is_err());
    let queue = root.path().join(format!(
        "discord_pending_queue/claude/borrowed/{channel}.json"
    ));
    std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
    std::fs::write(&queue, serde_json::to_vec(&vec![item(1)]).unwrap()).unwrap();
    #[cfg(windows)]
    let original = std::fs::read(&queue).unwrap();
    let outcome = enqueue_borrowed(&destination, &item(2), &guard).unwrap();
    #[cfg(not(windows))]
    {
        assert_eq!(outcome, EnqueueOutcome::Persisted);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&queue).unwrap()).unwrap(),
            serde_json::json!([item(1), item(2)])
        );
    }
    #[cfg(windows)]
    {
        // Without a parent-directory flush, handback must refuse without writing.
        assert_eq!(outcome, EnqueueOutcome::Rejected);
        assert_eq!(std::fs::read(&queue).unwrap(), original);
    }
    let wrong = Destination {
        provider: &ProviderKind::Codex,
        ..destination
    };
    assert!(enqueue_borrowed(&wrong, &item(3), &guard).is_err());
    assert!(!root.path().join("discord_pending_queue/codex").exists());
    let mut pinned = item(3);
    pinned["blob_pins"] = serde_json::json!([]);
    assert!(enqueue_borrowed(&destination, &pinned, &guard).is_err());
    let bytes = std::fs::read(&queue).unwrap();
    std::fs::write(queue.with_extension("dispatch"), b"invalid").unwrap();
    let invalid_dispatch = enqueue_borrowed(&destination, &item(3), &guard);
    #[cfg(not(windows))]
    assert!(invalid_dispatch.is_err());
    #[cfg(windows)]
    assert_eq!(invalid_dispatch.unwrap(), EnqueueOutcome::Rejected);
    assert_eq!(std::fs::read(queue).unwrap(), bytes);
}

#[test]
fn b2_scope_raw_inflight_reentry_is_busy_without_second_fd_wait() {
    use crate::services::discord::inflight::{
        lock_inflight_state_path, try_lock_inflight_state_path,
    };
    let root = tempfile::tempdir().unwrap();
    let channel = 6_325_301;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    let scope =
        PopulationScope::writer(root.path(), &ProviderKind::Claude, channel, &permit).unwrap();
    let path = root
        .path()
        .join(format!("discord_inflight/claude/{channel}.json"));
    for result in [
        lock_inflight_state_path(&path),
        try_lock_inflight_state_path(&path),
    ] {
        assert_eq!(
            result.err().as_deref(),
            Some("input fence: Busy"),
            "scope reentry must refuse rather than flock a second fd"
        );
    }
    assert!(matches!(
        PopulationScope::writer(root.path(), &ProviderKind::Claude, channel, &permit),
        Err(Failure::Busy)
    ));
    assert!(!path.exists());
    drop(scope);
    let owned = lock_inflight_state_path(&path).unwrap();
    assert_eq!(
        try_lock_inflight_state_path(&path).err().as_deref(),
        Some("input fence: Busy")
    );
    assert_eq!(
        lock_inflight_state_path(&path).err().as_deref(),
        Some("input fence: Busy")
    );
    std::thread::spawn(move || drop(owned)).join().unwrap();
    assert!(
        lock_inflight_state_path(&path).is_ok(),
        "cross-thread drop expires origin marker"
    );
    gate.clear_failure_for_test();
}

#[test]
fn b2_borrowed_row_keeps_episode_cas_and_never_reacquires_sidecar() {
    use crate::services::discord::inflight::{
        BorrowedInflightRow, GuardedClearOutcome, InflightEpisodePin, InflightTurnState,
        operator_disposition_remove_borrowed,
    };
    let root = tempfile::tempdir().unwrap();
    let channel = 6_325_302;
    let closing = Closing::frozen_for_test(ProviderKind::Claude, channel);
    let guard = closing.frozen_population(root.path()).unwrap();
    let borrowed =
        BorrowedInflightRow::new(&guard, root.path(), &ProviderKind::Claude, channel).unwrap();
    assert!(BorrowedInflightRow::new(&guard, root.path(), &ProviderKind::Codex, channel).is_err());
    let path = root
        .path()
        .join(format!("discord_inflight/claude/{channel}.json"));
    let row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        8,
        0,
        "input".into(),
        None,
        Some("b2-cas".into()),
        None,
        None,
        0,
    );
    let pin = InflightEpisodePin::from_state(&row);
    let mut successor = row.clone();
    successor.turn_nonce = Some("successor".into());
    let bytes = serde_json::to_vec(&successor).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        operator_disposition_remove_borrowed(&borrowed, &pin).0,
        GuardedClearOutcome::UserMsgMismatch
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    assert_eq!(
        operator_disposition_remove_borrowed(&borrowed, &pin).0,
        GuardedClearOutcome::Cleared
    );
    assert!(!path.exists());
    let second = std::fs::File::open(path.with_extension("json.lock")).unwrap();
    assert!(matches!(
        second.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
}

#[test]
fn b2_scheduler_late_protection_refuses_inflight_before_any_io() {
    use crate::services::discord::inflight::lock_inflight_state_path;
    let root = tempfile::tempdir().unwrap();
    let channel = 6_325_303;
    assert!(lookup(&ProviderKind::Claude, channel).is_none());
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let path = root
        .path()
        .join(format!("discord_inflight/claude/{channel}.json"));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            assert_eq!(
                lock_inflight_state_path(&path).err().as_deref(),
                Some("input fence: Busy")
            );
            assert!(
                !root.path().join("discord_inflight").exists(),
                "late protection must not do scheduler IO"
            );
            assert_eq!(
                tokio::spawn(async {
                    tokio::task::yield_now().await;
                    1
                })
                .await
                .unwrap(),
                1
            );
            tokio::task::spawn_blocking(move || {
                blocking(|| lock_inflight_state_path(&path).map(drop))
            })
            .await
            .unwrap()
            .unwrap();
        });
    gate.clear_failure_for_test();
}

#[test]
fn b2_inflight_writer_waits_then_persists_and_closing_refuses_without_io() {
    use crate::services::discord::inflight::{
        InflightTurnState, save_inflight_state_create_new, save_inflight_state_if_absent,
    };
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let _env = Env::set(temp.path());
    let channel = 6_325_312;
    let root = population_root().unwrap();
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let held = PopulationGuard::try_acquire(&root, &ProviderKind::Claude, channel).unwrap();
    let state = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        8,
        0,
        "input".into(),
        None,
        Some("b2-writer".into()),
        None,
        None,
        0,
    );
    let path = root.join(format!("discord_inflight/claude/{channel}.json"));
    let (waiting, receive) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let writer_state = state.clone();
    let worker = std::thread::spawn(move || {
        WAIT_OBSERVER.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                waiting.send(()).unwrap();
                released.recv().unwrap();
            }))
        });
        save_inflight_state_create_new(&writer_state)
    });
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        !path.exists(),
        "contention cannot publish or report a duplicate"
    );
    drop(held);
    release.send(()).unwrap();
    worker.join().unwrap().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(
        serde_json::from_slice::<InflightTurnState>(&bytes)
            .unwrap()
            .user_msg_id,
        8
    );
    assert!(!save_inflight_state_if_absent(&state).unwrap());
    let closing = gate.close().unwrap();
    assert!(save_inflight_state_create_new(&state).is_err());
    assert!(save_inflight_state_if_absent(&state).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    gate.clear_failure_for_test();
    drop(closing);
}

struct Env(Option<std::ffi::OsString>);
impl Env {
    fn set(path: &Path) -> Self {
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

#[test]
fn queue_primitives_share_canonical_sidecar_wait_and_borrow_without_scheduler_stall() {
    use crate::services::turn_orchestrator::{
        Intervention, InterventionMode, remove_channel_pending_queue_files_all_tokens,
        save_channel_queue,
    };
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let temp = tempfile::tempdir().unwrap();
    let _env = Env::set(temp.path());
    let channel = ChannelId::new(6_325_109);
    let item = Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(1),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(1)],
        source_message_queued_generations: vec![],
        source_text_segments: vec![],
        text: "input".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: vec![],
        voice_announcement: None,
    };
    let root = population_root().unwrap();
    let held = PopulationGuard::try_acquire(&root, &ProviderKind::Claude, channel.get()).unwrap();
    save_channel_queue(
        &ProviderKind::Claude,
        "primitive",
        channel,
        &[item.clone()],
        None,
    )
    .unwrap();
    let gate = Gate::protect(ProviderKind::Claude, channel.get()).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let path = root.join("discord_pending_queue/claude/primitive/6325109.json");
        let before = std::fs::read(&path).unwrap();
        assert!(
            save_channel_queue(&ProviderKind::Claude, "primitive", channel, &[], None).is_err(),
            "protected scheduler writer must refuse before changing latest population"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let (send, receive) = tokio::sync::oneshot::channel();
        let work = tokio::task::spawn_blocking(move || {
            WAIT_OBSERVER.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    let _ = send.send(());
                }))
            });
            blocking(|| {
                save_channel_queue(&ProviderKind::Claude, "primitive", channel, &[item], None)
            })
        });
        tokio::time::timeout(Duration::from_secs(1), receive)
            .await
            .unwrap()
            .unwrap();
        let heartbeat = tokio::spawn(async {
            tokio::task::yield_now().await;
            1
        });
        assert_eq!(heartbeat.await.unwrap(), 1);
        assert!(
            !work.is_finished(),
            "writer must wait for canonical sidecar release"
        );
        drop(held);
        work.await.unwrap().unwrap();
    });
    let path = root.join("discord_pending_queue/claude/primitive/6325109.json");
    let bytes = std::fs::read(&path).unwrap();
    let closing = gate.close().unwrap();
    assert!(save_channel_queue(&ProviderKind::Claude, "primitive", channel, &[], None).is_err());
    assert_eq!(
        remove_channel_pending_queue_files_all_tokens(&ProviderKind::Claude, channel),
        0
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let scope = PopulationScope::hold(closing.population(&root).unwrap());
    save_channel_queue(&ProviderKind::Claude, "primitive", channel, &[], None).unwrap();
    assert!(!path.exists());
    drop(scope);
    assert!(
        closing.population(&root).is_ok(),
        "scope drop must release its guard"
    );
    gate.clear_failure_for_test();
}

#[test]
fn released_registration_and_stale_closing_cannot_issue_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let channel = 6_325_110;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    gate.state.lock().unwrap().mode = Mode::Handback;
    closing.release_protection_after_handback().unwrap();
    assert!(matches!(
        Gate::protect(ProviderKind::Claude, channel),
        Err(Failure::StalePermit)
    ));
    assert!(matches!(
        closing.population(root.path()),
        Err(Failure::Busy)
    ));
    assert!(!root.path().join("discord_inflight").exists());
    gate.restore_protection().unwrap();
    assert!(matches!(
        closing.population(root.path()),
        Err(Failure::Busy)
    ));
    let fresh = gate.close().unwrap();
    assert!(matches!(
        closing.population(root.path()),
        Err(Failure::Busy)
    ));
    let guard = fresh.population(root.path()).unwrap();
    let scope = PopulationScope::hold(guard);
    drop(scope);
    assert!(fresh.population(root.path()).is_ok());
    drop(fresh);
    assert_eq!(
        gate.mode(),
        Mode::Closing,
        "dropping Closing must never reopen admission"
    );
}

#[test]
fn admitted_writer_scope_finishes_after_close_without_new_admission() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _env = Env::set(root.path());
    let channel = 6_325_111;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();
    let closing = gate.close().unwrap();
    let scope = PopulationScope::writer(
        &population_root().unwrap(),
        &ProviderKind::Claude,
        channel,
        &permit,
    )
    .unwrap();
    crate::services::turn_orchestrator::save_channel_queue(
        &ProviderKind::Claude,
        "admitted",
        poise::serenity_prelude::ChannelId::new(channel),
        &[],
        None,
    )
    .unwrap();
    drop(scope);
    drop(permit);
    futures::executor::block_on(closing.drain());
    assert!(matches!(gate.admit(), Err(Failure::Mode(Mode::Closing))));
}

#[test]
fn off_failure_health_does_not_change_actual_snapshot() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let channel = 6_325_112;
    let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _health = crate::services::discord::input_runtime::fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    gate.record_failure(&[71], Failure::Mode(Mode::Closing));
    gate.state.lock().unwrap().mode = Mode::Handback;
    closing.release_protection_after_handback().unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let registry = crate::services::discord::health::HealthRegistry::new();
            let before = crate::services::discord::health::build_health_snapshot(&registry).await;
            let before = serde_json::to_value(before).unwrap();
            record_failure(&ProviderKind::Claude, channel, &[72], Failure::Persistence);
            let after = crate::services::discord::health::build_health_snapshot(&registry).await;
            let after = serde_json::to_value(after).unwrap();
            assert_eq!(before["status"], after["status"]);
            assert_eq!(before["degraded_reasons"], after["degraded_reasons"]);
            assert!(
                after["degraded_reasons"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| !r.as_str().unwrap().contains("channel=6325112"))
            );
        });
}
