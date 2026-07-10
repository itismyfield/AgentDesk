use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

use poise::serenity_prelude::ChannelId;

use super::{RelayRecoveryApplyResult, RelayRecoveryDecision, SharedData};

pub(super) const AUTO_HEAL_RESTART_CONFIRM_GRACE_SECS: i64 = 120;
const AUTO_HEAL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(1);
const AUTO_HEAL_CONFIRM_STABLE_FOR: Duration = Duration::from_millis(200);
const AUTO_HEAL_CONFIRM_POLL: Duration = Duration::from_millis(25);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReattachConfirmation {
    NotRequired,
    Confirmed,
    StartupGrace,
    Failed,
}

#[derive(Clone)]
struct SpawnedWatcherProbe {
    owner_channel_id: ChannelId,
    tmux_session: String,
    output_path: String,
    cancel: Arc<AtomicBool>,
    heartbeat: Arc<AtomicI64>,
    baseline_heartbeat_ms: i64,
}

pub(super) async fn classify_reattach_confirmation(
    shared: &SharedData,
    decision: &RelayRecoveryDecision,
    apply_result: &RelayRecoveryApplyResult,
    process_started_at_unix: i64,
    now_unix: i64,
) -> ReattachConfirmation {
    if decision.action != super::RelayRecoveryActionKind::ReattachWatcher
        || apply_result.reattach_watcher_spawned != Some(true)
    {
        return ReattachConfirmation::NotRequired;
    }
    let confirmed = match decision.affected.tmux_session.as_deref() {
        Some(tmux_session) if !tmux_session.is_empty() => {
            confirm_spawned_watcher(
                shared,
                decision.channel_id,
                tmux_session,
                decision.evidence.last_relay_offset,
            )
            .await
        }
        _ => false,
    };
    confirmation_after_probe(confirmed, process_started_at_unix, now_unix)
}

fn confirmation_after_probe(
    confirmed: bool,
    process_started_at_unix: i64,
    now_unix: i64,
) -> ReattachConfirmation {
    if confirmed {
        ReattachConfirmation::Confirmed
    } else if startup_confirm_grace_active(process_started_at_unix, now_unix) {
        ReattachConfirmation::StartupGrace
    } else {
        ReattachConfirmation::Failed
    }
}

fn startup_confirm_grace_active(process_started_at_unix: i64, now_unix: i64) -> bool {
    now_unix.saturating_sub(process_started_at_unix) < AUTO_HEAL_RESTART_CONFIRM_GRACE_SECS
}

async fn confirm_spawned_watcher(
    shared: &SharedData,
    channel_id: u64,
    tmux_session: &str,
    baseline_frontier: u64,
) -> bool {
    let Some(probe) = spawned_watcher_probe(shared, ChannelId::new(channel_id), tmux_session)
    else {
        return false;
    };
    let deadline = Instant::now() + AUTO_HEAL_CONFIRM_TIMEOUT;
    let mut heartbeat_stable_since = None;
    loop {
        if shared.committed_relay_offset(ChannelId::new(channel_id)) > baseline_frontier {
            return true;
        }
        if !spawned_watcher_still_current(shared, &probe) {
            return false;
        }
        if probe.heartbeat.load(Ordering::Acquire) > probe.baseline_heartbeat_ms {
            let stable_since = heartbeat_stable_since.get_or_insert_with(Instant::now);
            if stable_since.elapsed() >= AUTO_HEAL_CONFIRM_STABLE_FOR {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(AUTO_HEAL_CONFIRM_POLL).await;
    }
}

fn spawned_watcher_probe(
    shared: &SharedData,
    fallback_channel_id: ChannelId,
    tmux_session: &str,
) -> Option<SpawnedWatcherProbe> {
    let owner_channel_id = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(tmux_session)
        .unwrap_or(fallback_channel_id);
    let watcher = shared.tmux_watchers.get(&owner_channel_id)?;
    if watcher.tmux_session_name != tmux_session || watcher.cancel.load(Ordering::Acquire) {
        return None;
    }
    Some(SpawnedWatcherProbe {
        owner_channel_id,
        tmux_session: watcher.tmux_session_name.clone(),
        output_path: watcher.output_path.clone(),
        cancel: watcher.cancel.clone(),
        heartbeat: watcher.last_heartbeat_ts_ms.clone(),
        baseline_heartbeat_ms: watcher.last_heartbeat_ts_ms.load(Ordering::Acquire),
    })
}

fn spawned_watcher_still_current(shared: &SharedData, probe: &SpawnedWatcherProbe) -> bool {
    let Some(watcher) = shared.tmux_watchers.get(&probe.owner_channel_id) else {
        return false;
    };
    watcher.tmux_session_name == probe.tmux_session
        && watcher.output_path == probe.output_path
        && Arc::ptr_eq(&watcher.cancel, &probe.cancel)
        && Arc::ptr_eq(&watcher.last_heartbeat_ts_ms, &probe.heartbeat)
        && !watcher.cancel.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::{TmuxWatcherHandle, make_shared_data_for_tests};

    fn watcher_handle(tmux_session: &str, heartbeat: Arc<AtomicI64>) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session.to_string(),
            output_path: "/tmp/agentdesk-4423-confirm.jsonl".to_string(),
            paused: Arc::new(AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: heartbeat,
        }
    }

    #[tokio::test]
    async fn relay_recovery_spawn_confirm_requires_stable_heartbeat_advance() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_423_201);
        let tmux_session = "AgentDesk-codex-4423-confirm";
        let heartbeat = Arc::new(AtomicI64::new(100));
        shared
            .tmux_watchers
            .insert(channel, watcher_handle(tmux_session, heartbeat.clone()));
        let advance = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            heartbeat.store(101, Ordering::Release);
        });

        assert!(confirm_spawned_watcher(&shared, channel.get(), tmux_session, 0).await);
        advance.await.expect("heartbeat task");
    }

    #[test]
    fn relay_recovery_restart_first_turn_gets_at_least_120_second_grace() {
        let started_at = 10_000;
        assert_eq!(
            confirmation_after_probe(false, started_at, started_at + 119),
            ReattachConfirmation::StartupGrace
        );
        assert_eq!(
            confirmation_after_probe(false, started_at, started_at + 120),
            ReattachConfirmation::Failed
        );
    }
}
