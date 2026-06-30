//! #2436 (#2427 B wire) heartbeat-gap → explicit inflight cleanup.
//!
//! Background safety net that bridges the gap between `TmuxWatcherHandle::heartbeat_stale()`
//! (defined in `mod.rs`) and explicit inflight eviction. Previously the
//! watcher map could carry a "silently hung" watcher entry whose loop
//! had stopped advancing `last_heartbeat_ts_ms` while its inflight row
//! continued to look healthy on disk — the only thing that eventually
//! cleaned that up was `placeholder_sweeper`'s time-based 5-minute
//! abandon heuristic.
//!
//! This sweeper runs at a faster cadence than `placeholder_sweeper`
//! (`SWEEP_INTERVAL_SECS`) and walks `shared.tmux_watchers`: for each
//! entry whose `heartbeat_stale()` reports true it
//!   1. resolves the watcher's `(provider, channel_id)` from its tmux
//!      session name,
//!   2. skips foreign providers (the sweeper is spawned **per
//!      provider** so we never act on another bot's watchers),
//!   3. tries to clear the matching inflight row via
//!      `inflight::clear_inflight_state_if_matches` — that helper
//!      keeps planned-restart rows, rebind-origin rows, and stale-turn
//!      rows from being deleted out from under a new turn,
//!   4. signals `cancel` on the watcher entry so the loop exits at the
//!      next poll instead of staying registered.
//!
//! The threshold is `mod.rs::TMUX_WATCHER_STALE_HEARTBEAT_MS` (60s as
//! of this commit), which `heartbeat_stale()` already encapsulates. We
//! deliberately stay *below* `placeholder_sweeper::ABANDON_THRESHOLD_SECS`
//! so this explicit signal fires before the time heuristic — i.e. the
//! placeholder sweeper only ever sees rows that B+C+D+A all missed.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::SharedData;
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

/// Polling interval. Picked lower than `placeholder_sweeper`'s 30s so
/// the explicit eviction lands before the time-based sweeper has a
/// chance to look at the row.
pub(crate) const HEARTBEAT_SWEEP_INTERVAL_SECS: u64 = 15;

/// Initial delay so recovery has a chance to revive watchers and
/// publish the first heartbeat before we start judging staleness.
/// Aligns with `placeholder_sweeper::INITIAL_DELAY_SECS` so the two
/// sweepers come online at roughly the same time.
pub(crate) const HEARTBEAT_SWEEP_INITIAL_DELAY_SECS: u64 = 180;

/// Result of one pass — for testability and the heartbeat log.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HeartbeatSweepReport {
    pub scanned: usize,
    pub heartbeat_stale: usize,
    pub evicted: usize,
    pub cancelled: usize,
    /// Codex review HIGH on PR #2460: surface cleanup IO failures so the
    /// operator can see the safety-net (placeholder_sweeper 1800s) is the
    /// only thing recovering — instead of every error being silently
    /// bucketed as Missing/no-op.
    pub io_errors: usize,
}

/// Single pass over the tmux watcher registry. Returns counts for the
/// pass — the long-running task wrapper logs them on transitions.
fn run_heartbeat_sweep_pass_inner(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> HeartbeatSweepReport {
    let mut report = HeartbeatSweepReport::default();

    // Snapshot the registry keys + the data we need so we hold the
    // dashmap iterator only for the immutable scan, then act outside.
    // Cancelling and inflight FS removal are both cheap and don't
    // touch the registry, but keeping the iterator alive across the
    // file-system call would block other registry mutations longer
    // than necessary.
    struct Candidate {
        tmux_session_name: String,
        provider: ProviderKind,
        channel_id: u64,
    }
    let mut candidates: Vec<Candidate> = Vec::new();

    for entry in shared.tmux_watchers.iter() {
        report.scanned += 1;
        if !entry.value().heartbeat_stale() {
            continue;
        }
        // Already cancelled? skip — the registry will drop it on the
        // next iteration of the owning task's loop. Acting again would
        // double-emit observability events.
        if entry.value().cancel.load(Ordering::Relaxed) {
            continue;
        }
        report.heartbeat_stale += 1;
        let tmux_session_name = entry.value().tmux_session_name.clone();
        let Some((session_provider, _)) =
            parse_provider_and_channel_from_tmux_name(&tmux_session_name)
        else {
            continue;
        };
        // Sweeper is spawned per provider — never touch another bot's
        // watcher entries. The registry can contain multiple providers
        // during the deprecation tail of a hot-swap, so this guard is
        // load-bearing.
        if session_provider != *provider {
            continue;
        }
        let Some(owner_channel) = shared
            .tmux_watchers
            .owner_channel_for_tmux_session(&tmux_session_name)
        else {
            continue;
        };
        let channel_id = owner_channel.get();
        if channel_id == 0 {
            continue;
        }
        candidates.push(Candidate {
            tmux_session_name,
            provider: session_provider,
            channel_id,
        });
    }

    for candidate in candidates {
        // Cleanup is best-effort and double-bookkept: even when the
        // inflight row is missing (legitimate turn just completed) we
        // still want to cancel the watcher so the registry slot does
        // not linger.
        let inflight_state =
            super::inflight::load_inflight_state(&candidate.provider, candidate.channel_id);
        let Some(state) = inflight_state.as_ref() else {
            // No row to evict, but still cancel the watcher so the
            // registry self-heals on the next iteration of its task
            // loop.
            cancel_watcher_by_session_name(shared, &candidate.tmux_session_name);
            report.cancelled += 1;
            continue;
        };
        let dispatch_id_for_event = state.dispatch_id.clone();
        let user_msg_id = state.user_msg_id;
        let was_planned_restart = state.restart_mode.is_some();
        let was_rebind_origin = state.rebind_origin;

        // #3859: a stale-watcher row may still own a live "🔄 처리 중" placeholder.
        // Deleting the row alone strands that card forever, so route through the
        // abandon-request helper: same ownership guards as
        // `clear_inflight_state_if_matches`, but it durably records the
        // placeholder for the sweeper to finalize to "중단됨" BEFORE deleting the
        // row (channel still freed immediately).
        let outcome = super::inflight::request_inflight_abandon_if_matches(
            &candidate.provider,
            candidate.channel_id,
            user_msg_id,
            &shared.token_hash,
        );
        match outcome {
            super::inflight::GuardedClearOutcome::Cleared => {
                report.evicted += 1;
                crate::services::observability::emit_inflight_lifecycle_event(
                    candidate.provider.as_str(),
                    candidate.channel_id,
                    dispatch_id_for_event.as_deref(),
                    None,
                    None,
                    "evict_heartbeat_gap",
                    serde_json::json!({
                        "reason": "watcher_heartbeat_stale",
                        "user_msg_id": user_msg_id,
                        "tmux_session_name": candidate.tmux_session_name,
                    }),
                );
            }
            super::inflight::GuardedClearOutcome::UserMsgMismatch
            | super::inflight::GuardedClearOutcome::Missing => {
                if !was_planned_restart && !was_rebind_origin {
                    tracing::debug!(
                        "[heartbeat_sweeper] guard rejected eviction for {}/{} \
                         outcome={:?} — row changed or vanished mid-pass",
                        candidate.provider.as_str(),
                        candidate.channel_id,
                        outcome,
                    );
                }
            }
            super::inflight::GuardedClearOutcome::PlannedRestartSkipped
            | super::inflight::GuardedClearOutcome::RebindOriginSkipped => {
                tracing::debug!(
                    "[heartbeat_sweeper] guard preserved {}/{} ({:?}) — \
                     not the B wire's job to evict",
                    candidate.provider.as_str(),
                    candidate.channel_id,
                    outcome,
                );
            }
            super::inflight::GuardedClearOutcome::IoError => {
                // Codex review HIGH on PR #2460: cleanup IO error must not
                // be silently swallowed. Skip the watcher cancel so the
                // inflight row remains visible to the next sweeper tick;
                // the 1800s safety-net still bounds worst case, but the
                // explicit warn surfaces broken filesystems before that.
                tracing::warn!(
                    "[heartbeat_sweeper] cleanup IoError for {}/{} — \
                     skipping watcher cancel; sweeper will retry next tick",
                    candidate.provider.as_str(),
                    candidate.channel_id,
                );
                report.io_errors += 1;
                continue;
            }
        }
        cancel_watcher_by_session_name(shared, &candidate.tmux_session_name);
        report.cancelled += 1;
    }

    report
}

/// Look up the tmux watcher by session name and set its `cancel`
/// flag. The watcher's owning task picks this up at the next poll and
/// exits.
fn cancel_watcher_by_session_name(shared: &Arc<SharedData>, tmux_session_name: &str) {
    // The registry's primary key for direct lookup is the tmux session
    // name itself; resolve via the iterator to keep the existing
    // ownership invariants (lock + index updates) inside the registry.
    for entry in shared.tmux_watchers.iter() {
        if entry.key() != tmux_session_name {
            continue;
        }
        entry.value().cancel.store(true, Ordering::Relaxed);
        return;
    }
}

/// Spawn the long-lived heartbeat-gap sweeper task. Should be called
/// once per `(provider, SharedData)` from `runtime_bootstrap` —
/// alongside `placeholder_sweeper::spawn_placeholder_sweeper`.
pub(crate) fn spawn_heartbeat_sweeper(shared: Arc<SharedData>, provider: ProviderKind) {
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(
            HEARTBEAT_SWEEP_INITIAL_DELAY_SECS,
        ))
        .await;
        loop {
            let report = run_heartbeat_sweep_pass_inner(&shared, &provider);
            if report.heartbeat_stale > 0 {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 💓 heartbeat sweeper ({}): scanned={} heartbeat_stale={} evicted={} cancelled={}",
                    provider.as_str(),
                    report.scanned,
                    report.heartbeat_stale,
                    report.evicted,
                    report.cancelled,
                );
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(
                HEARTBEAT_SWEEP_INTERVAL_SECS,
            ))
            .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_sweep_report_default_is_zero() {
        let report = HeartbeatSweepReport::default();
        assert_eq!(report.scanned, 0);
        assert_eq!(report.heartbeat_stale, 0);
        assert_eq!(report.evicted, 0);
        assert_eq!(report.cancelled, 0);
    }

    #[test]
    fn heartbeat_sweep_initial_delay_below_placeholder_abandon() {
        // The B-wire sweeper must come online before the
        // placeholder_sweeper can land its safety-net abandon. With
        // the #2438 bump to 1800s abandon, our 180s initial delay
        // gives B over 27 minutes of head-start at any one moment.
        assert!(
            HEARTBEAT_SWEEP_INITIAL_DELAY_SECS
                < super::super::placeholder_sweeper::ABANDON_THRESHOLD_SECS
        );
    }

    #[test]
    fn heartbeat_sweep_interval_below_placeholder_sweep_interval() {
        // The explicit signal MUST fire faster than the time-based
        // safety net so the placeholder sweeper never gets to
        // act on a row that the heartbeat sweeper could have cleared.
        assert!(
            HEARTBEAT_SWEEP_INTERVAL_SECS < super::super::placeholder_sweeper::SWEEP_INTERVAL_SECS
        );
    }
}
