//! The stale-sweep port: the destructive warrant an automatic stale-turn sweep asks of the
//! provider runtimes the Discord health registry holds.

use futures::future::BoxFuture;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AxisBSite {
    #[cfg(unix)]
    ProbeAutoHealReattach,
    WatchdogStaleIdle,
    WatchdogExplicitBackground,
    #[cfg(unix)]
    RelayDeadReattach,
    #[cfg(unix)]
    ProbeAutoHeal,
    PolicyTickStaleSweep,
    BootReconcileSweep,
}

impl AxisBSite {
    pub(crate) fn is_stale_sweep(self) -> bool {
        matches!(self, Self::PolicyTickStaleSweep | Self::BootReconcileSweep)
    }
}

/// Whether an automatic sweep at `Site` may clear one stale session, judged by the registry.
pub(crate) trait StaleSweepPort<Site>: Sync {
    fn automatic_stale_sweep_warrants<'a>(
        &'a self,
        session_key: &'a str,
        provider_name: &'a str,
        site: Site,
    ) -> BoxFuture<'a, bool>;
}

/// `registry` as the sweep port, for a caller holding the concrete registry.
pub(crate) fn stale_sweep_port<Site, R: StaleSweepPort<Site>>(
    registry: Option<&R>,
) -> Option<&dyn StaleSweepPort<Site>> {
    registry.map(|registry| registry as &dyn StaleSweepPort<Site>)
}
