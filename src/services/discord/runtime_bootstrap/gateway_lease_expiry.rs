use std::time::Duration;

use crate::services::discord::shared_state::gateway_lease_diagnostics::Diagnostics;
use crate::services::provider::ProviderKind;

/// Repeat interval of the preferred node's long-wait warning.
const PREFERRED_WAIT_WARN_INTERVAL: Duration = Duration::from_secs(600);

/// Server-side idle expiry for gateway lease sessions; `None` (configured 0) never expires.
pub(super) fn gateway_lease_idle_expiry(
    cluster: &crate::config::ClusterConfig,
) -> Option<Duration> {
    (cluster.gateway_lease_idle_expiry_secs > 0)
        .then(|| Duration::from_secs(cluster.gateway_lease_idle_expiry_secs))
}

/// Warns with the last shared holder observation once the preferred node has waited longer than
/// the configured lease idle expiry (and grace); it records the wait, not its cause.
pub(super) struct PreferredWaitWarning {
    started: tokio::time::Instant,
    next: tokio::time::Instant,
    idle_expiry_secs: u64,
}

impl PreferredWaitWarning {
    pub(super) fn start() -> Self {
        let cluster = crate::config::snapshot()
            .map(|config| config.cluster.clone())
            .unwrap_or_default();
        let after = cluster
            .gateway_lease_idle_expiry_secs
            .max(cluster.gateway_yield_grace_secs)
            + 60;
        let started = tokio::time::Instant::now();
        Self {
            started,
            next: started + Duration::from_secs(after),
            idle_expiry_secs: cluster.gateway_lease_idle_expiry_secs,
        }
    }

    pub(super) fn observe(&mut self, diagnostics: &Diagnostics, provider: &ProviderKind) {
        if tokio::time::Instant::now() < self.next {
            return;
        }
        self.next += PREFERRED_WAIT_WARN_INTERVAL;
        let snapshot = diagnostics.projection();
        tracing::warn!(
            provider = provider.as_str(),
            conclusion = "unresolved",
            holder = ?snapshot.holder,
            snapshot_at = snapshot.snapshot_at,
            snapshot_status = ?snapshot.snapshot_status,
            pg_target = snapshot.pg_target,
            waited_secs = self.started.elapsed().as_secs(),
            configured_idle_expiry_secs = self.idle_expiry_secs,
            "GATEWAY-LEASE: preferred node has waited longer than the configured lease idle expiry; investigation remains unresolved"
        );
    }
}
