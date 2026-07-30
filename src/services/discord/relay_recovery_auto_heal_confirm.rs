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
    RelayEmissionInFlight,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpawnedWatcherConfirmation {
    Confirmed,
    RelayEmissionInFlight,
    Unconfirmed,
    WatcherGone,
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
        || apply_result.status != "reattached_watcher"
    {
        return ReattachConfirmation::NotRequired;
    }
    let probe_result = match decision.affected.tmux_session.as_deref() {
        Some(tmux_session) if !tmux_session.is_empty() => {
            let baseline_frontier = shared
                .committed_relay_offset(ChannelId::new(decision.channel_id))
                .max(decision.evidence.last_relay_offset);
            confirm_spawned_watcher(shared, decision.channel_id, tmux_session, baseline_frontier)
                .await
        }
        _ => SpawnedWatcherConfirmation::WatcherGone,
    };
    confirmation_after_probe(probe_result, process_started_at_unix, now_unix)
}

fn confirmation_after_probe(
    probe_result: SpawnedWatcherConfirmation,
    process_started_at_unix: i64,
    now_unix: i64,
) -> ReattachConfirmation {
    match probe_result {
        SpawnedWatcherConfirmation::Confirmed => ReattachConfirmation::Confirmed,
        SpawnedWatcherConfirmation::RelayEmissionInFlight => {
            ReattachConfirmation::RelayEmissionInFlight
        }
        SpawnedWatcherConfirmation::Unconfirmed
            if startup_confirm_grace_active(process_started_at_unix, now_unix) =>
        {
            ReattachConfirmation::StartupGrace
        }
        SpawnedWatcherConfirmation::Unconfirmed | SpawnedWatcherConfirmation::WatcherGone => {
            ReattachConfirmation::Failed
        }
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
) -> SpawnedWatcherConfirmation {
    let Some(probe) = spawned_watcher_probe(shared, ChannelId::new(channel_id), tmux_session)
    else {
        return SpawnedWatcherConfirmation::WatcherGone;
    };
    let deadline = Instant::now() + AUTO_HEAL_CONFIRM_TIMEOUT;
    let mut heartbeat_stable_since = None;
    loop {
        if !spawned_watcher_still_current(shared, &probe) {
            return SpawnedWatcherConfirmation::WatcherGone;
        }
        // The frontier is channel-scoped rather than watcher/episode-scoped. The
        // current check prevents an already replaced watcher from taking credit,
        // but another relay actor can still advance it while this watcher remains
        // current. Exact attribution belongs to #5022.
        if shared.committed_relay_offset(ChannelId::new(channel_id)) > baseline_frontier {
            return SpawnedWatcherConfirmation::Confirmed;
        }
        if probe.heartbeat.load(Ordering::Acquire) > probe.baseline_heartbeat_ms {
            let stable_since = heartbeat_stable_since.get_or_insert_with(Instant::now);
            if stable_since.elapsed() >= AUTO_HEAL_CONFIRM_STABLE_FOR {
                return SpawnedWatcherConfirmation::Confirmed;
            }
        }
        if Instant::now() >= deadline {
            return confirmation_at_deadline(shared, ChannelId::new(channel_id), &probe);
        }
        tokio::time::sleep(AUTO_HEAL_CONFIRM_POLL).await;
    }
}

fn confirmation_at_deadline(
    shared: &SharedData,
    channel_id: ChannelId,
    probe: &SpawnedWatcherProbe,
) -> SpawnedWatcherConfirmation {
    if !spawned_watcher_still_current(shared, probe) {
        SpawnedWatcherConfirmation::WatcherGone
    } else if shared.relay_emission_in_flight(channel_id) {
        SpawnedWatcherConfirmation::RelayEmissionInFlight
    } else {
        SpawnedWatcherConfirmation::Unconfirmed
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

        assert_eq!(
            confirm_spawned_watcher(&shared, channel.get(), tmux_session, 0).await,
            SpawnedWatcherConfirmation::Confirmed
        );
        advance.await.expect("heartbeat task");
    }

    #[tokio::test]
    async fn channel_frontier_existing_before_probe_is_not_spawn_confirmation() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_423_207);
        let tmux_session = "AgentDesk-codex-4423-frontier-baseline";
        shared
            .tmux_relay_coord(channel)
            .confirmed_end_offset
            .store(512, Ordering::Release);
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session, Arc::new(AtomicI64::new(100))),
        );
        let decision = super::super::plan_relay_recovery(
            &super::super::RelayHealthSnapshot {
                provider: "codex".to_string(),
                channel_id: channel.get(),
                active_turn: super::super::RelayActiveTurn::Foreground,
                tmux_session: Some(tmux_session.to_string()),
                tmux_alive: Some(true),
                watcher_attached: false,
                watcher_attached_stale: false,
                watcher_owner_channel_id: None,
                watcher_owns_live_relay: false,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(4_423_217),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(4_423_227),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: None,
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_outbound_activity_ms: None,
                last_capture_offset: Some(512),
                last_relay_offset: 0,
                unread_bytes: Some(512),
                desynced: true,
                stale_thread_proof: false,
            },
            super::super::RelayStallState::TmuxAliveRelayDead,
            1_000,
        );
        let apply_result = RelayRecoveryApplyResult {
            status: "reattached_watcher",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: Some(true),
            reattach_watcher_replaced: Some(false),
            reattach_initial_offset: Some(0),
            reattach_error: None,
        };

        assert_ne!(
            classify_reattach_confirmation(&shared, &decision, &apply_result, 0, 1_000).await,
            ReattachConfirmation::Confirmed,
            "frontier committed before this probe must not confirm the spawned watcher"
        );
    }

    #[test]
    fn relay_recovery_same_watcher_emission_in_flight_is_not_confirmation_failure() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_423_202);
        let tmux_session = "AgentDesk-codex-4423-emitting";
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session, Arc::new(AtomicI64::new(100))),
        );
        let probe = spawned_watcher_probe(&shared, channel, tmux_session).expect("watcher probe");
        shared
            .tmux_relay_coord(channel)
            .relay_slot
            .store(128, Ordering::Release);

        assert_eq!(
            confirmation_at_deadline(&shared, channel, &probe),
            SpawnedWatcherConfirmation::RelayEmissionInFlight
        );
    }

    #[tokio::test]
    async fn relay_recovery_repaired_round_without_spawn_still_requires_probe() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_423_206);
        let tmux_session = "AgentDesk-codex-4423-repaired-reuse";
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session, Arc::new(AtomicI64::new(100))),
        );
        let decision = super::super::plan_relay_recovery(
            &super::super::RelayHealthSnapshot {
                provider: "codex".to_string(),
                channel_id: channel.get(),
                active_turn: super::super::RelayActiveTurn::Foreground,
                tmux_session: Some(tmux_session.to_string()),
                tmux_alive: Some(true),
                watcher_attached: false,
                watcher_attached_stale: false,
                watcher_owner_channel_id: None,
                watcher_owns_live_relay: false,
                bridge_inflight_present: true,
                bridge_current_msg_id: Some(4_423_216),
                mailbox_has_cancel_token: true,
                mailbox_active_user_msg_id: Some(4_423_226),
                mailbox_turn_started_at_ms: None,
                queue_depth: 0,
                pending_discord_callback_msg_id: None,
                pending_thread_proof: false,
                parent_channel_id: None,
                thread_channel_id: None,
                last_relay_ts_ms: None,
                last_outbound_activity_ms: None,
                last_capture_offset: Some(128),
                last_relay_offset: 0,
                unread_bytes: Some(128),
                desynced: true,
                stale_thread_proof: false,
            },
            super::super::RelayStallState::TmuxAliveRelayDead,
            1_000,
        );
        let apply_result = RelayRecoveryApplyResult {
            status: "reattached_watcher",
            removed_thread_proofs: 0,
            removed_mailbox_token: false,
            post_mailbox_has_cancel_token: None,
            post_mailbox_queue_depth: None,
            reattach_watcher_spawned: Some(false),
            reattach_watcher_replaced: Some(false),
            reattach_initial_offset: Some(0),
            reattach_error: None,
        };

        assert_eq!(
            classify_reattach_confirmation(&shared, &decision, &apply_result, 0, 200).await,
            ReattachConfirmation::Failed
        );
    }

    #[test]
    fn relay_recovery_same_watcher_without_emission_fails_at_deadline() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_423_203);
        let tmux_session = "AgentDesk-codex-4423-not-emitting";
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session, Arc::new(AtomicI64::new(100))),
        );
        let probe = spawned_watcher_probe(&shared, channel, tmux_session).expect("watcher probe");

        assert_eq!(
            confirmation_at_deadline(&shared, channel, &probe),
            SpawnedWatcherConfirmation::Unconfirmed
        );
    }

    #[test]
    fn relay_recovery_cancelled_watcher_fails_even_with_channel_emission() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_423_204);
        let tmux_session = "AgentDesk-codex-4423-cancelled-emitting";
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session, Arc::new(AtomicI64::new(100))),
        );
        let probe = spawned_watcher_probe(&shared, channel, tmux_session).expect("watcher probe");
        probe.cancel.store(true, Ordering::Release);
        shared
            .tmux_relay_coord(channel)
            .relay_slot
            .store(128, Ordering::Release);

        assert_eq!(
            confirmation_at_deadline(&shared, channel, &probe),
            SpawnedWatcherConfirmation::WatcherGone
        );
        assert_eq!(
            confirmation_after_probe(SpawnedWatcherConfirmation::WatcherGone, 10_000, 10_001),
            ReattachConfirmation::Failed,
            "startup grace must not consume a round after the writer disappeared"
        );
    }

    #[test]
    fn relay_recovery_replaced_watcher_fails_even_with_channel_emission() {
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(4_423_205);
        let tmux_session = "AgentDesk-codex-4423-replaced-emitting";
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session, Arc::new(AtomicI64::new(100))),
        );
        let probe = spawned_watcher_probe(&shared, channel, tmux_session).expect("watcher probe");
        shared.tmux_watchers.insert(
            channel,
            watcher_handle(tmux_session, Arc::new(AtomicI64::new(100))),
        );
        shared
            .tmux_relay_coord(channel)
            .relay_slot
            .store(128, Ordering::Release);

        assert_eq!(
            confirmation_at_deadline(&shared, channel, &probe),
            SpawnedWatcherConfirmation::WatcherGone
        );
    }

    #[test]
    fn relay_recovery_restart_first_turn_gets_at_least_120_second_grace() {
        let started_at = 10_000;
        assert_eq!(
            confirmation_after_probe(
                SpawnedWatcherConfirmation::Unconfirmed,
                started_at,
                started_at + 119
            ),
            ReattachConfirmation::StartupGrace
        );
        assert_eq!(
            confirmation_after_probe(
                SpawnedWatcherConfirmation::Unconfirmed,
                started_at,
                started_at + 120
            ),
            ReattachConfirmation::Failed
        );
    }
}
