//! Only changed user-default bindings consult consent custody during migration.
use super::{BootSessionHosts, HoldSource};
use crate::config::session_hosts::EndpointMode;
use std::collections::BTreeSet;

pub(super) fn affected_user_default(
    approved: &BootSessionHosts,
    candidate: &BootSessionHosts,
    changes: &BTreeSet<u64>,
) -> BTreeSet<u64> {
    changes
        .iter()
        .copied()
        .filter(|id| {
            [approved, candidate].into_iter().any(|boot| {
                boot.channels
                    .get(id)
                    .and_then(|mapped| boot.config.herdr.endpoints.get(&mapped.key))
                    .is_some_and(|endpoint| endpoint.mode == EndpointMode::UserDefault)
            })
        })
        .collect()
}

pub(crate) struct ConsentHolds;
impl HoldSource for ConsentHolds {
    const INTRODUCED: bool = true;

    async fn unresolved(&self, channels: &BTreeSet<u64>) -> Result<Vec<u64>, String> {
        if channels.is_empty() {
            return Ok(Vec::new());
        }
        #[cfg(unix)]
        {
            let channels = channels.clone();
            tokio::task::spawn_blocking(move || {
                let store = crate::services::session_host::policy::consent::ConsentStore::new(
                    crate::config::runtime_root()
                        .map(|root| root.join("runtime/herdr_policy/consents")),
                );
                Ok(store
                    .unresolved()?
                    .into_iter()
                    .flat_map(|(_, intents)| intents)
                    .map(|intent| intent.channel)
                    .filter(|id| channels.contains(id))
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect())
            })
            .await
            .map_err(|error| format!("consent hold inventory: {error}"))?
        }
        #[cfg(not(unix))]
        Err("user-default consent custody is unavailable on this platform".into())
    }
}

#[cfg(all(test, unix))]
#[path = "consent_holds_tests.rs"]
mod tests;
