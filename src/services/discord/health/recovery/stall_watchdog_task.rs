//! Periodic repair plus immediate wakeup after a committed provider-error intent.
use super::*;
pub fn spawn_stall_watchdog(registry: Arc<HealthRegistry>, provider: ProviderKind) {
    tokio::spawn(async move {
        let wake = crate::services::agent_recovery::recovery_wakeup(&provider);
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(STALL_WATCHDOG_INITIAL_DELAY_SECS)) => {},
            _ = wake.notified() => {},
        }
        loop {
            let cleaned = run_stall_watchdog_pass(&registry, &provider).await;
            if cleaned > 0 {
                tracing::info!(
                    provider = provider.as_str(),
                    cleaned,
                    "stall-watchdog pass completed"
                );
            }
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(STALL_WATCHDOG_INTERVAL_SECS)) => {},
                _ = wake.notified() => {},
            }
        }
    });
}
