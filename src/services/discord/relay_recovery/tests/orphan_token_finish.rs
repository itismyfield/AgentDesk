use super::*;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

pub(in crate::services::discord) fn queued(message_id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation: crate::services::discord::runtime_store::load_generation(),
        source_message_ids: vec![MessageId::new(message_id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued behind orphan token {message_id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

async fn enqueue_three(shared: &Arc<SharedData>, provider: &ProviderKind, channel: ChannelId) {
    for id in 1..=3 {
        crate::services::discord::mailbox_enqueue_intervention(
            shared,
            provider,
            channel,
            queued(9_000 + id),
        )
        .await;
    }
}

/// Hands `channel`'s input to its V2 owner: one front an attempt carries and, when `waiting`, one
/// input behind it. The V1 queue stays empty.
pub(in crate::services::discord) async fn v2_attempted_front(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    waiting: bool,
) {
    use crate::services::turn_orchestrator::{PendingQueueItem, QueuePersistenceContext, v2_queue};
    let (mailboxes, hash) = shared.queue_fixture_parts();
    let handle = mailboxes.handle(channel);
    let context = QueuePersistenceContext::new(provider, hash, None);
    let (path, namespace) = v2_queue(&context, channel).unwrap();
    handle.v2_adopt(path, namespace).await.unwrap();
    let item = |id: u64| -> PendingQueueItem {
        serde_json::from_value(serde_json::json!({"author_id": 7, "message_id": id,
            "channel_id": channel.get(), "source_message_ids": [id],
            "queued_generation": 1, "text": format!("queued {id}")}))
        .unwrap()
    };
    // Clear of `channel + 1`, which fixtures use as the active turn's message.
    let (front, behind) = (item(channel.get() + 101), item(channel.get() + 102));
    let attempt = serde_json::json!({"nonce": "front", "provider": provider.as_str(),
        "target": {"host": "tmux", "owner": "front", "root": "/test", "execution_nonce": "e"},
        "source": {"kind": "pre_source", "expected_session_id": null,
            "expected_transcript": null, "binding_watermark": null}});
    let attempted = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        owner
            .enqueue_fixture(&front)
            .map_err(|e| format!("{e:?}"))?;
        let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
        owner.reserve_unit(attempt, |item| Some((String::new(), item.text.clone())))?;
        if !waiting {
            return Ok(true);
        }
        owner.enqueue_fixture(&behind).map_err(|e| format!("{e:?}"))
    });
    assert_eq!(attempted.await, Ok(true));
}

/// The decision the operator lane plans from its own snapshot, before apply.
async fn planned_decision(registry: &HealthRegistry, channel: ChannelId) -> RelayRecoveryDecision {
    run_relay_recovery_at(
        registry,
        Some("codex"),
        channel.get(),
        false,
        chrono::Utc::now().timestamp_millis(),
    )
    .await
    .expect("dry run should plan")
    .decision
}

#[tokio::test]
async fn orphan_token_apply_finishes_the_snapshot_episode_and_keeps_the_queue() {
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(5_996_301);
    let token = start_test_turn(&shared, channel, MessageId::new(5_996_310)).await;
    enqueue_three(&shared, &provider, channel).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let decision = planned_decision(&registry, channel).await;
    assert_eq!(
        decision.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );

    let result = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &decision,
        None,
        RelayRecoveryApplySource::Manual,
    )
    .await;

    assert_eq!(result.status, "applied");
    assert!(result.removed_mailbox_token);
    assert_eq!(result.post_mailbox_queue_depth, Some(3));
    let after = super::super::mailbox_snapshot(&shared, channel).await;
    assert!(after.cancel_token.is_none());
    assert_eq!(
        after.intervention_queue.len(),
        3,
        "finishing the orphan must not supersede the queued messages"
    );
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
    assert!(
        shared.restart.deferred_hook_channels.contains_key(&channel),
        "the preserved queue must be handed to a kickoff owner"
    );
}

// An orphan finish and a queue drain report the waiting depth of the V2 owner that holds the
// input: its attempted front is not waiting, and the empty V1 queue owns nothing.
#[tokio::test]
async fn orphan_and_drain_applies_report_the_v2_owner_waiting_depth() {
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(5_998_301);
    start_test_turn(&shared, channel, MessageId::new(5_998_310)).await;
    v2_attempted_front(&shared, &provider, channel, true).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);
    let orphan = planned_decision(&registry, channel).await;
    assert_eq!(
        orphan.action,
        RelayRecoveryActionKind::ClearOrphanPendingToken
    );
    let source = RelayRecoveryApplySource::Manual;

    let finished =
        apply_relay_recovery_decision(&registry, &shared, &provider, &orphan, None, source).await;
    assert_eq!(finished.status, "applied");
    assert_eq!(finished.post_mailbox_queue_depth, Some(1));

    let blocked = RelayHealthSnapshot {
        channel_id: channel.get(),
        queue_depth: 1,
        ..snapshot()
    };
    let drain = plan_relay_recovery(&blocked, RelayStallState::QueueBlocked, 1_000);
    assert_eq!(drain.action, RelayRecoveryActionKind::DrainPendingQueue);
    let drained =
        apply_relay_recovery_decision(&registry, &shared, &provider, &drain, None, source).await;
    assert_eq!(drained.status, "scheduled_pending_queue_drain");
    assert_eq!(drained.post_mailbox_queue_depth, Some(1));
}

#[tokio::test]
async fn orphan_token_apply_leaves_a_successor_episode_untouched() {
    let (_root_guard, _root_dir) = isolated_agentdesk_root();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(5_996_401);
    let anchor = MessageId::new(5_996_410);
    start_test_turn(&shared, channel, anchor).await;
    enqueue_three(&shared, &provider, channel).await;
    let stale = planned_decision(&registry, channel).await;

    // A->B under the same message id: B is backdated so only its nonce, not its
    // start instant, separates it from the planned episode.
    super::super::mailbox_finish_turn(&shared, &provider, channel).await;
    let successor = start_test_turn(&shared, channel, anchor).await;
    shared
        .mailbox(channel)
        .age_active_turn_for_test(std::time::Duration::from_secs(60))
        .await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let result = apply_relay_recovery_decision(
        &registry,
        &shared,
        &provider,
        &stale,
        None,
        RelayRecoveryApplySource::Manual,
    )
    .await;

    assert!(!result.removed_mailbox_token);
    assert_eq!(result.status, "orphan_token_episode_changed");
    let after = super::super::mailbox_snapshot(&shared, channel).await;
    assert!(
        after
            .cancel_token
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &successor))
    );
    assert_eq!(after.active_user_message_id, Some(anchor));
    assert_eq!(after.intervention_queue.len(), 3);
    assert!(!successor.cancelled.load(Ordering::Relaxed));
    assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn manual_recovery_finishes_a_measured_dead_orphan_and_promotes_its_queue() {
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().expect("runtime root");
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let _tmux = MissingSessionTmux::new_after_shared_test_env_lock();
    let provider = ProviderKind::Codex;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    let channel = ChannelId::new(5_996_501);
    let token = start_test_turn(&shared, channel, MessageId::new(5_996_510)).await;
    token.bind_unmanaged_session_name(&format!("plain-shell-5996-manual-{}", std::process::id()));
    enqueue_three(&shared, &provider, channel).await;
    shared.restart.global_active.store(1, Ordering::Relaxed);

    let response = run_relay_recovery_at(
        &registry,
        Some("codex"),
        channel.get(),
        true,
        chrono::Utc::now().timestamp_millis()
            + ORPHAN_PENDING_TOKEN_ADMISSION_GRACE.as_millis() as i64,
    )
    .await
    .expect("manual recovery should evaluate");

    assert_eq!(response.decision.evidence.tmux_alive, Some(false));
    assert!(response.applied, "{:?}", response.decision.auto_heal);
    let after = super::super::mailbox_snapshot(&shared, channel).await;
    assert!(after.cancel_token.is_none());
    assert_eq!(after.intervention_queue.len(), 3);
    assert!(token.cancelled.load(Ordering::Relaxed));
    assert!(shared.restart.deferred_hook_channels.contains_key(&channel));
}

/// A live private server makes a never-created session a measured negative.
#[cfg(unix)]
pub(in crate::services::discord) struct MissingSessionTmux {
    binary: std::path::PathBuf,
    socket: std::path::PathBuf,
    _env: [crate::config::TestEnvVarGuard; 3],
    _root: tempfile::TempDir,
}

#[cfg(unix)]
impl MissingSessionTmux {
    /// The caller must hold the shared environment lock until this fixture is dropped.
    pub(in crate::services::discord) fn new_after_shared_test_env_lock() -> Self {
        crate::config::test_env_lock::assert_shared_test_env_lock_held();
        use crate::config::TestEnvVarGuard as Guard;
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::Builder::new()
            .prefix("adk-missing-")
            .tempdir_in("/tmp")
            .expect("short private tmux root");
        let socket_dir = root
            .path()
            .join(format!("tmux-{}", unsafe { libc::getuid() }));
        std::fs::create_dir(&socket_dir).expect("private tmux socket directory");
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700))
            .expect("private socket permissions");
        let env = [
            Guard::capture_after_shared_test_env_lock("TMUX"),
            Guard::capture_after_shared_test_env_lock("TMUX_PANE"),
            Guard::set_path_after_shared_test_env_lock("TMUX_TMPDIR", root.path()),
        ];
        unsafe {
            std::env::remove_var("TMUX");
            std::env::remove_var("TMUX_PANE");
        }
        let command = crate::services::platform::binary_resolver::runtime_command("tmux")
            .expect("real tmux is required by the measured-missing fixture");
        let fixture = Self {
            binary: std::path::PathBuf::from(command.get_program()),
            socket: socket_dir.join("default"),
            _env: env,
            _root: root,
        };
        let started = fixture
            .command()
            .args([
                "new-session",
                "-d",
                "-s",
                "fixture-keeper",
                "exec sleep 600",
            ])
            .output()
            .expect("start private tmux server");
        assert!(
            started.status.success(),
            "private tmux keeper must start: {}",
            String::from_utf8_lossy(&started.stderr)
        );
        fixture
    }

    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.binary);
        command
            .args(["-f", "/dev/null", "-S"])
            .arg(&self.socket)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command
    }
}

#[cfg(unix)]
impl Drop for MissingSessionTmux {
    fn drop(&mut self) {
        // The explicit socket keeps cleanup inside this fixture even during unwinding.
        let _ = self.command().arg("kill-server").output();
    }
}
