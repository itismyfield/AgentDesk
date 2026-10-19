//! Complete runtime membership evidence for read-only Unbound judgments.
use std::sync::Arc;

use super::HealthRegistry;
use crate::services::discord::SharedData;

#[derive(Clone)]
pub(in crate::services::discord) struct UnboundRuntimeSnapshot {
    pub generation: u64,
    pub entries: Vec<(String, Arc<SharedData>)>,
}

impl HealthRegistry {
    /// Every registered runtime, including multiple instances of the same provider.
    pub(in crate::services::discord) async fn all_registered_shared(&self) -> Vec<Arc<SharedData>> {
        self.providers
            .lock()
            .await
            .iter()
            .map(|entry| entry.shared.clone())
            .collect()
    }

    pub(in crate::services::discord) async fn unbound_runtime_snapshot(
        &self,
    ) -> UnboundRuntimeSnapshot {
        let providers = self.providers.lock().await;
        UnboundRuntimeSnapshot {
            generation: self.registration_generation(),
            entries: providers
                .iter()
                .map(|entry| (entry.name.clone(), Arc::clone(&entry.shared)))
                .collect(),
        }
    }

    /// Keep registration and its generation update outside the synchronous publication.
    pub(in crate::services::discord) fn with_unbound_membership<R>(
        &self,
        expected_generation: u64,
        publish: impl FnOnce() -> R,
    ) -> Result<R, &'static str> {
        let _providers = self.providers.try_lock().map_err(|_| "providers_busy")?;
        if self.registration_generation() != expected_generation {
            return Err("membership");
        }
        Ok(publish())
    }
}

#[cfg(test)]
mod tests;
