//! Coalesced wakeups accelerate durable work; periodic scans repair lost wakes.
use crate::services::provider::ProviderKind;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Notify;

pub(crate) fn recovery_wakeup(provider: &ProviderKind) -> Arc<Notify> {
    static WAKEUPS: OnceLock<Mutex<HashMap<String, Arc<Notify>>>> = OnceLock::new();
    super::durable::lock(WAKEUPS.get_or_init(Mutex::default))
        .entry(provider.as_str().to_string())
        .or_default()
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn wake_before_sleep_is_retained_and_provider_scoped() {
        let wake = recovery_wakeup(&ProviderKind::Codex);
        wake.notify_one();
        tokio::time::timeout(std::time::Duration::from_millis(50), wake.notified())
            .await
            .unwrap();
        assert!(!Arc::ptr_eq(&wake, &recovery_wakeup(&ProviderKind::Claude)));
    }
}
