//! One `tmux -V` at startup through the shared probe runtime, so a node that cannot run the
//! health tmux probe at all reports degraded instead of staying green on unwitnessed reads.

use std::sync::OnceLock;
use std::time::Duration;

use super::session_enrichment::{HealthProbeRuntime, ProbeFault, ProbeOutcome};
use super::{HealthRegistry, HealthStatus};
use crate::services::platform::tmux::health_probe::HealthTmuxAnswer;

/// The probe's own wait; generous, since its one answer stands for the whole process.
const PREFLIGHT_BUDGET: Duration = Duration::from_secs(10);
/// From this long after boot, no answer yet is itself a fault.
const PENDING_GRACE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Preflight {
    Ready,
    Unready(&'static str),
}

/// What the startup probe found; empty, so pending, until it answers.
#[derive(Default)]
struct PreflightSlot(OnceLock<Preflight>);

static PREFLIGHT: PreflightSlot = PreflightSlot(OnceLock::new());

impl PreflightSlot {
    async fn run(&self, runtime: &HealthProbeRuntime) -> Preflight {
        let verdict = match runtime.preflight(PREFLIGHT_BUDGET).await {
            ProbeOutcome::Answered(HealthTmuxAnswer::Present) => Preflight::Ready,
            ProbeOutcome::Answered(_) => Preflight::Unready("failed"),
            ProbeOutcome::Unwitnessed(fault) => Preflight::Unready(fault_label(fault)),
        };
        let _ = self.0.set(verdict);
        verdict
    }

    fn reason(&self, uptime: Duration) -> Option<String> {
        match self.0.get() {
            Some(Preflight::Ready) => None,
            Some(Preflight::Unready(fault)) => Some(format!("health_tmux_probe_unready:{fault}")),
            None if uptime >= PENDING_GRACE => Some("health_tmux_probe_pending".to_string()),
            None => None,
        }
    }

    fn detail(&self) -> serde_json::Value {
        match self.0.get() {
            Some(Preflight::Ready) => serde_json::json!({ "preflight": "ready" }),
            Some(Preflight::Unready(fault)) => {
                serde_json::json!({ "preflight": "unready", "fault": fault })
            }
            None => serde_json::json!({ "preflight": "pending" }),
        }
    }

    fn attach(
        &self,
        uptime: Duration,
        status: &mut HealthStatus,
        reasons: &mut Vec<serde_json::Value>,
        json: &mut serde_json::Value,
    ) {
        if let Some(reason) = self.reason(uptime) {
            *status = status.worsen(HealthStatus::Degraded);
            reasons.push(serde_json::Value::String(reason));
        }
        // The public body drops this key; only the detail body carries it.
        json["health_tmux_probe"] = self.detail();
    }
}

fn fault_label(fault: ProbeFault) -> &'static str {
    match fault {
        ProbeFault::Saturated => "saturated",
        ProbeFault::Expired => "expired",
        ProbeFault::Cancelled => "cancelled",
        ProbeFault::PathUnready => "path_unready",
        ProbeFault::Refused => "refused",
        ProbeFault::Spawn => "spawn",
        ProbeFault::Io => "io",
        ProbeFault::Overflow => "overflow",
        ProbeFault::Panicked => "panicked",
        ProbeFault::OwnerLost => "owner_lost",
    }
}

/// Starts the startup probe; call once after the runtime PATH is published.
pub(crate) fn spawn() {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("health tmux probe preflight has no runtime; health reports it pending");
        return;
    };
    runtime.spawn(async {
        match PREFLIGHT.run(&HealthProbeRuntime::shared()).await {
            Preflight::Ready => tracing::info!("health tmux probe preflight ready"),
            Preflight::Unready(fault) => tracing::warn!(
                fault,
                "health tmux probe preflight unready; health reports degraded"
            ),
        }
    });
}

/// Degrades health while the startup probe is unready or overdue, and names it in detail.
pub(crate) fn attach_tmux_probe_preflight(
    registry: &HealthRegistry,
    status: &mut HealthStatus,
    reasons: &mut Vec<serde_json::Value>,
    json: &mut serde_json::Value,
) {
    PREFLIGHT.attach(registry.started_at.elapsed(), status, reasons, json);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn attached(slot: &PreflightSlot, uptime: Duration) -> (HealthStatus, serde_json::Value) {
        let (mut status, mut reasons) = (HealthStatus::Healthy, Vec::new());
        let mut json = serde_json::json!({});
        slot.attach(uptime, &mut status, &mut reasons, &mut json);
        json["degraded_reasons"] = serde_json::Value::Array(reasons);
        (status, json)
    }

    /// A prepared PATH without tmux spawns nothing, and health says so instead of staying green.
    #[tokio::test]
    async fn a_cold_path_preflight_degrades_health_instead_of_staying_green() {
        let runtime = HealthProbeRuntime::over_fake_tmux(4, true);
        let slot = PreflightSlot::default();
        assert_eq!(slot.run(&runtime).await, Preflight::Unready("path_unready"));
        assert_eq!(runtime.admitted_and_spawned(), (1, 0));
        let (status, json) = attached(&slot, Duration::ZERO);
        assert_eq!(status, HealthStatus::Degraded);
        assert_eq!(
            json["degraded_reasons"],
            serde_json::json!(["health_tmux_probe_unready:path_unready"])
        );
        assert_eq!(json["health_tmux_probe"]["preflight"], "unready");
    }

    /// The preflight takes a slot of the capped runtime, and a full runtime leaves it unready.
    #[tokio::test]
    async fn preflight_spawns_through_the_shared_capped_runtime() {
        let runtime = HealthProbeRuntime::over_fake_tmux(1, false);
        let slot = PreflightSlot::default();
        assert_eq!(slot.run(&runtime).await, Preflight::Ready);
        assert_eq!(runtime.admitted_and_spawned(), (1, 1));
        let (status, json) = attached(&slot, Duration::from_secs(3_600));
        assert_eq!(status, HealthStatus::Healthy);
        assert_eq!(json["health_tmux_probe"]["preflight"], "ready");

        let full = HealthProbeRuntime::over_fake_tmux(0, false);
        let slot = PreflightSlot::default();
        assert_eq!(slot.run(&full).await, Preflight::Unready("saturated"));
        assert_eq!(full.admitted_and_spawned(), (0, 0));
        let (status, _) = attached(&slot, Duration::ZERO);
        assert_eq!(status, HealthStatus::Degraded);
    }

    /// A fresh process has no answer yet: green while it boots, degraded from 60 s on.
    #[test]
    fn an_unanswered_preflight_degrades_from_sixty_seconds_after_boot() {
        let slot = PreflightSlot::default();
        let (status, json) = attached(&slot, PENDING_GRACE - Duration::from_millis(1));
        assert_eq!(status, HealthStatus::Healthy);
        assert_eq!(json["health_tmux_probe"]["preflight"], "pending");
        let (status, json) = attached(&slot, PENDING_GRACE);
        assert_eq!(status, HealthStatus::Degraded);
        assert_eq!(
            json["degraded_reasons"],
            serde_json::json!(["health_tmux_probe_pending"])
        );
    }
}
