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

#[cfg(unix)]
fn store() -> crate::services::session_host::policy::consent::ConsentStore {
    crate::services::session_host::policy::consent::ConsentStore::new(
        crate::config::runtime_root().map(|root| root.join("runtime/herdr_policy/consents")),
    )
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
            self.blocking(channels, &BootSessionHosts::default()).await
        }
        #[cfg(not(unix))]
        Err("user-default consent custody is unavailable on this platform".into())
    }

    #[cfg(unix)]
    async fn blocking(
        &self,
        channels: &BTreeSet<u64>,
        candidate: &BootSessionHosts,
    ) -> Result<Vec<u64>, String> {
        use crate::services::session_host::policy::consent::settlement::Resolution;
        if channels.is_empty() {
            return Ok(Vec::new());
        }
        let (channels, candidate) = (channels.clone(), candidate.config().clone());
        tokio::task::spawn_blocking(move || {
            let mut held = BTreeSet::new();
            for (consent, intents) in store().unresolved()? {
                for intent in intents
                    .iter()
                    .filter(|intent| channels.contains(&intent.channel))
                {
                    let returned = consent.resolution(intent).and_then(Resolution::returned_to);
                    if returned.is_none()
                        || returned != super::binding_digest(&candidate, intent.channel)
                    {
                        held.insert(intent.channel);
                    }
                }
            }
            Ok(held.into_iter().collect())
        })
        .await
        .map_err(|error| format!("consent hold inventory: {error}"))?
    }

    #[cfg(unix)]
    async fn adopted(
        &self,
        approved: &BootSessionHosts,
        channels: &BTreeSet<u64>,
    ) -> Result<(), String> {
        use crate::services::session_host::policy::consent::settlement::Resolution;
        let (channels, approved) = (channels.clone(), approved.config().clone());
        tokio::task::spawn_blocking(move || {
            let store = store();
            for (consent, intents) in store.unresolved()? {
                let adopted: Vec<_> = intents
                    .into_iter()
                    .filter_map(|intent| match consent.resolution(&intent) {
                        Some(Resolution::Returned { target })
                            if channels.contains(&intent.channel)
                                && super::binding_digest(&approved, intent.channel)
                                    == Some(target) =>
                        {
                            Some((intent, Resolution::Adopted { target }))
                        }
                        _ => None,
                    })
                    .collect();
                if !adopted.is_empty() {
                    store.settle(&consent, &adopted)?;
                }
            }
            Ok(())
        })
        .await
        .map_err(|error| format!("consent return adoption: {error}"))?
    }
}

#[cfg(all(test, unix))]
#[path = "consent_holds_tests.rs"]
mod tests;
