#![cfg(unix)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use poise::serenity_prelude::ChannelId;

use super::tests::{clear_redrive_test_state, watcher_handle};
use super::{HealthRegistry, stall_liveness};
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::relay_health::DurableFrontierObservation;
use crate::services::provider::ProviderKind;

const GRACE_SECS: i64 = stall_liveness::STALL_WATCHDOG_BACKLOG_NO_PROGRESS_GRACE_SECS as i64;

/// A durable zero-frontier refusal must use the production dispatcher.
/// Its counted retry distinguishes refusal from falling through to auto-reattach.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn redrive_entrypoint_counts_a_durable_zero_frontier_refusal_5986() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().expect("temp runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Codex;
    let channel_id = ChannelId::new(5_986_030);
    let tmux_session = "AgentDesk-codex-5986-refused-frontier";
    let output_path = tmp.path().join("agentdesk-5986-refused-frontier.jsonl");
    std::fs::File::create(&output_path)
        .expect("create capture fixture")
        .set_len(301_613)
        .expect("size capture fixture");
    let output_path = output_path.to_string_lossy().into_owned();
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", tmux_session])
        .status();
    assert!(
        std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", tmux_session])
            .status()
            .expect("start tmux fixture")
            .success(),
        "production snapshot must observe a live producer tmux session"
    );

    let shared = crate::services::discord::make_shared_data_for_tests();
    let resume_offset = Arc::new(Mutex::new(None));
    let turn_delivered = Arc::new(AtomicBool::new(true));
    shared.tmux_watchers.insert(
        channel_id,
        watcher_handle(
            tmux_session,
            &output_path,
            resume_offset.clone(),
            turn_delivered.clone(),
        ),
    );
    let started_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut inflight = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        None,
        0,
        5_986_031,
        0,
        "test".to_string(),
        None,
        Some(tmux_session.to_string()),
        Some(output_path.clone()),
        None,
        128,
    );
    inflight.started_at = started_at.clone();
    inflight.updated_at = started_at;
    inflight.last_watcher_relayed_offset = Some(128);
    crate::services::discord::inflight::save_inflight_state(&inflight)
        .expect("seed authoritative relayed row");
    clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);

    let registry = HealthRegistry::new();
    let snapshot = registry
        .snapshot_watcher_state_for_shared(&provider, shared.clone(), channel_id.get())
        .await
        .expect("pre-refusal production snapshot");
    assert_eq!(snapshot.last_relay_offset, 0);
    assert!(matches!(
        snapshot.durable_frontier,
        DurableFrontierObservation::GenerationUnresolved {
            relayed_start: 128,
            ..
        }
    ));
    let base = chrono::Utc::now().timestamp();
    assert!(
        !registry
            .redrive_undelivered_backlog_at(
                &provider,
                shared.clone(),
                channel_id,
                base - GRACE_SECS,
            )
            .await
            .expect("seed redrive grace"),
        "initial observation only seeds the no-progress grace"
    );
    assert!(
        !registry
            .redrive_undelivered_backlog_at(&provider, shared.clone(), channel_id, base)
            .await
            .expect("production redrive refusal"),
        "zero in-memory frontier contradicted by the durable row must refuse"
    );
    assert_eq!(
        *resume_offset.lock().unwrap(),
        None,
        "a refused frontier must never be sent to the live watcher"
    );
    assert!(
        turn_delivered.load(Ordering::Acquire),
        "the refusal must not disarm the bridge delivery marker"
    );

    assert_eq!(
        shared
            .redrive_attempt_decision(&provider, channel_id, &snapshot, base + 30)
            .attempt,
        Some(2),
        "the refusal dispatch arm must advance the bounded retry schedule"
    );

    crate::services::discord::inflight::clear_inflight_state(&provider, channel_id.get());
    clear_redrive_test_state(&shared, &provider, channel_id, tmux_session);
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", tmux_session])
        .status();
}
