//! The sweeper task loop and the tick it runs: the row pass, then the durable drains. Orphan
//! message deletes are drained by every execution runtime's own retry task, not here.

use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::{
    INITIAL_DELAY_SECS, SWEEP_HEARTBEAT_INTERVAL_SWEEPS, SWEEP_INTERVAL_SECS, SharedData,
    StalledEditTracker, SweepPassReport, run_placeholder_sweep_pass,
};
use crate::services::provider::ProviderKind;

fn should_log_sweep_report(report: SweepPassReport, sweeps_since_heartbeat: u64) -> bool {
    report.stalled > 0
        || report.abandoned > 0
        || report.reclaimed_panels > 0
        || sweeps_since_heartbeat >= SWEEP_HEARTBEAT_INTERVAL_SWEEPS
}

/// Spawn the long-lived background task that runs the stall sweeper at the
/// configured interval until the runtime exits. Should be called once per
/// provider during dcserver bootstrap.
pub(in crate::services::discord) fn spawn_placeholder_sweeper(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
) {
    tokio::spawn(async move {
        let mut stalled_tracker = StalledEditTracker::default();
        let mut sweeps_since_heartbeat = 0u64;
        tokio::time::sleep(tokio::time::Duration::from_secs(INITIAL_DELAY_SECS)).await;
        loop {
            let tick =
                run_placeholder_sweeper_tick(&http, &shared, &provider, &mut stalled_tracker).await;
            sweeps_since_heartbeat = sweeps_since_heartbeat.saturating_add(1);
            if should_log_sweep_report(tick.pass, sweeps_since_heartbeat) || tick.drained_any() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧹 placeholder sweeper ({}): scanned={} stalled={} abandoned={} reclaimed_panels={} drained_abort_markers={} drained_abandon_requests={} swept_busy_retry_bindings={} swept_orphan_anchors={}",
                    provider.as_str(),
                    tick.pass.scanned,
                    tick.pass.stalled,
                    tick.pass.abandoned,
                    tick.pass.reclaimed_panels,
                    tick.drained_abort_markers,
                    tick.drained_abandon_requests,
                    tick.swept_busy_retry_bindings,
                    tick.swept_orphan_anchors
                );
                sweeps_since_heartbeat = 0;
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(SWEEP_INTERVAL_SECS)).await;
        }
    });
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct TickReport {
    pass: SweepPassReport,
    drained_abort_markers: usize,
    swept_orphan_anchors: usize,
    drained_abandon_requests: usize,
    swept_busy_retry_bindings: usize,
}

impl TickReport {
    fn drained_any(&self) -> bool {
        self.drained_abort_markers > 0
            || self.drained_abandon_requests > 0
            || self.swept_busy_retry_bindings > 0
            || self.swept_orphan_anchors > 0
    }
}

/// One sweeper tick: the row pass, then the durable-record drains in fixed order.
pub(in crate::services::discord) async fn run_placeholder_sweeper_tick(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    stalled_tracker: &mut StalledEditTracker,
) -> TickReport {
    let pass = run_placeholder_sweep_pass(http, shared, provider, stalled_tracker).await;
    // #3296: reconcile durable aborted-anchor markers — retry the ✅ for
    // markers a terminal commit already covered, and apply the TTL'd
    // `⏳ → ⚠` fallback for anchors nothing ever covered (held while a
    // live inflight for the session may still cover them). The sweeper
    // owns this reclaim so an aborted anchor always converges (#3282).
    let drained_abort_markers =
        super::super::tui_direct_abort_marker::sweep_expired(shared, provider).await;
    // #4278 orphan-`⏳` sweep (mechanism: turn_view_reconciler::orphan_sweep).
    let swept_orphan_anchors =
        super::super::turn_view_reconciler::sweep_orphan_tui_anchor_reactions(shared, provider)
            .await;
    // #3859: finalize placeholders stranded by a failure-path inflight
    // eviction (turn-task Drop / heartbeat-gap sweeper). Each durable
    // abandon-request is edited to its terminal "중단됨" card BY MESSAGE
    // ID — decoupled from the inflight lifecycle, so a re-adopt (new row
    // + new placeholder) never collides with it.
    let drained_abandon_requests =
        super::super::abandon_request_store::drain(http, shared, provider, &shared.token_hash)
            .await;
    // A busy-notice binding left without a turn ages out here; spent retry budgets and
    // unreadable records are kept, so age never re-arms a retry.
    let swept_busy_retry_bindings = super::super::busy_followup_retry_store::sweep_expired();
    TickReport {
        pass,
        drained_abort_markers,
        swept_orphan_anchors,
        drained_abandon_requests,
        swept_busy_retry_bindings,
    }
}
