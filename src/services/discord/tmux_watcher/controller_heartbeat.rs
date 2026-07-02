use std::sync::Arc;

use crate::services::discord::outbound::turn_output_controller as toc;
use crate::services::discord::{
    DeliveryLeaseCell, DeliveryLeaseHeartbeat, DeliveryLeaseKey, LeaseHolder,
};

/// #3089 A4: adapts the watcher's `DeliveryLeaseHeartbeat` to [`toc::PostHeartbeat`].
/// Holds the `Arc` (the controller drives the lease behind a borrowed `&cell`) and
/// spawns the SAME `DeliveryLeaseHeartbeat::spawn` the legacy watcher used
/// (tmux_watcher.rs:6015, #3041 §3 / #3151 — identical renew cadence); the guard
/// Drop aborts the renew task BEFORE the inline commit (#3151 ordering). Mirrors
/// A2b's `SinkPostHeartbeat`.
pub(in crate::services::discord) struct WatcherPostHeartbeat {
    pub(in crate::services::discord) cell: Arc<DeliveryLeaseCell>,
}

impl toc::PostHeartbeat for WatcherPostHeartbeat {
    fn start(
        &self,
        holder: LeaseHolder,
        key: DeliveryLeaseKey,
    ) -> Box<dyn toc::PostHeartbeatGuard> {
        Box::new(WatcherPostHeartbeatGuard {
            _heartbeat: DeliveryLeaseHeartbeat::spawn(self.cell.clone(), holder, key),
        })
    }
}

struct WatcherPostHeartbeatGuard {
    _heartbeat: DeliveryLeaseHeartbeat,
}

impl toc::PostHeartbeatGuard for WatcherPostHeartbeatGuard {}
