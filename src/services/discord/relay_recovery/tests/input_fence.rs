//! A reserved relay reattach completes its pinned rebind.

use super::*;

struct GapApplyBoundary(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl super::super::auto_heal_apply::ReservedEpisodeApplyBoundary for GapApplyBoundary {
    async fn after_reserve(&self, _episode: &circuit_breaker::RelayReattachEpisode) {
        self.0.notify_one();
        self.1.notified().await;
    }
}

/// An automatic reattach that passed its reservation completes its pinned rebind.
#[cfg(unix)]
#[tokio::test]
async fn reserved_relay_reattach_completes_its_pinned_rebind() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::discord::inflight;
    use crate::services::session_host::test_support::InjectedLivenessGuard;
    use crate::services::session_host::{HostLiveness, HostSessionRef};
    use std::os::unix::fs::PermissionsExt;
    let _guard = auto_heal_test_lock().lock().await;
    clear_auto_heal_attempts_for_tests();
    let root_dir = tempfile::tempdir().unwrap();
    let _root_guard = crate::config::set_agentdesk_root_for_test(root_dir.path());
    // A `tmux` that reports every pane alive and lists no sessions.
    let tmux_dir = tempfile::tempdir().unwrap();
    let script = tmux_dir.path().join("tmux");
    std::fs::write(
        &script,
        "#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in list-panes) echo 0 ;; esac; exit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _path =
        crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(tmux_dir.path());
    let provider = ProviderKind::Claude;
    let (registry, shared) = registry_with_shared(provider.clone()).await;
    registry
        .register_http(
            provider.as_str().to_string(),
            Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
        )
        .await;
    let channel = 6_325_535_u64;
    let tmux = format!("AgentDesk-claude-c2ra{channel}-{}-cc", std::process::id());
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux,
        RuntimeHandoffKind::ClaudeTui,
    )
    .unwrap();
    let session = format!("48fdb7f3-6325-4000-8000-{channel:012}");
    let transcript = root_dir.path().join(format!("{session}.jsonl"));
    std::fs::write(&transcript, vec![b'x'; 4_096]).unwrap();
    let mut orphan = inflight::InflightTurnState::new(
        provider.clone(),
        channel,
        None,
        0,
        0,
        channel + 7,
        String::new(),
        Some(session),
        Some(tmux.clone()),
        Some(transcript.display().to_string()),
        None,
        4_096,
    );
    orphan.turn_source = inflight::TurnSource::ExternalInput;
    orphan.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    assert!(inflight::save_inflight_state_if_absent(&orphan).unwrap());
    let state = inflight::load_inflight_state_read_only(&provider, channel).unwrap();
    let row = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &provider,
        channel,
    );
    let before = std::fs::read(&row).unwrap();
    let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux), HostLiveness::Live);
    let mut health = snapshot();
    health.provider = provider.as_str().to_string();
    health.channel_id = channel;
    let mut decision = plan_relay_recovery(
        &health,
        RelayStallState::TmuxAliveRelayDead,
        chrono::Utc::now().timestamp_millis(),
    );
    decision.action = RelayRecoveryActionKind::ReattachWatcher;
    decision.auto_heal.eligible = true;
    decision.auto_heal.skipped_reason = None;
    decision.affected.channel_id = channel;
    decision.affected.finalizer_turn_id = Some(state.effective_finalizer_turn_id());
    decision.affected.tmux_session = Some(tmux.clone());
    let (reached, resume) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    let boundary = GapApplyBoundary(reached.clone(), resume.clone());

    let apply = super::super::auto_heal_apply::apply_relay_recovery_plan_with_seams(
        &registry,
        &shared,
        &provider,
        decision,
        chrono::Utc::now().timestamp_millis(),
        RelayRecoveryApplySource::StallWatchdog,
        &circuit_breaker::PgCircuitAlertEnqueue,
        &boundary,
    );
    tokio::pin!(apply);
    tokio::select! {
        _ = &mut apply => panic!("the apply must reserve before it rebinds"),
        _ = reached.notified() => {}
    }
    resume.notify_one();
    let response = apply.await;

    let watcher = shared
        .tmux_watchers
        .remove(&ChannelId::new(channel))
        .map(|(_, watcher)| watcher);
    if let Some(watcher) = watcher.as_ref() {
        watcher.cancel.store(true, Ordering::Relaxed);
    }
    let result = response.apply_result.as_ref().expect("apply result");
    assert_eq!(result.reattach_watcher_spawned, Some(true), "{response:?}");
    assert!(watcher.is_some(), "the apply rebound a watcher");
    assert_ne!(
        std::fs::read(&row).unwrap(),
        before,
        "the rebind rewrote its row"
    );
}
