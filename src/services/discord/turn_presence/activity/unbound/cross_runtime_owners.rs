//! Registry and exact receipt observations only fill an otherwise unowned session name.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::routine_owner::{self, ReceiptRuntime, RoutineEvidence};
use crate::services::discord::{SharedData, tmux_watcher_registry::TmuxWatcherRegistryGuard};
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WatcherEvidence {
    pub local: BTreeMap<String, Option<u64>>,
    pub registered: Vec<BTreeMap<String, Option<u64>>>,
}

impl WatcherEvidence {
    pub(super) fn capture(
        _guard: &TmuxWatcherRegistryGuard,
        shared: &Arc<SharedData>,
        entries: &[(String, Arc<SharedData>)],
        names: &[String],
    ) -> Self {
        let names: BTreeSet<_> = names.iter().cloned().collect();
        let read = |shared: &SharedData| {
            names
                .iter()
                .map(|name| {
                    let owner = shared
                        .tmux_watchers
                        .owner_channel_for_tmux_session(name)
                        .map(|id| id.get());
                    (name.clone(), owner)
                })
                .collect()
        };
        Self {
            local: read(shared),
            registered: entries.iter().map(|(_, shared)| read(shared)).collect(),
        }
    }

    fn claims(&self, name: &str) -> BTreeSet<u64> {
        self.registered
            .iter()
            .filter_map(|owners| owners.get(name).copied().flatten())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OwnerEvidence {
    pub watchers: WatcherEvidence,
    pub routines: Result<RoutineEvidence, String>,
}

impl OwnerEvidence {
    pub(super) async fn collect(
        watchers: WatcherEvidence,
        pool: &sqlx::PgPool,
        entries: &[(String, Arc<SharedData>)],
    ) -> Result<Self, String> {
        if watchers.registered.len() != entries.len() {
            return Err("runtime ownership membership changed".into());
        }
        let runtimes: Result<Vec<_>, String> = entries
            .iter()
            .map(|(registered, shared)| {
                let provider = ProviderKind::from_str(registered)
                    .filter(ProviderKind::is_supported)
                    .filter(|provider| *provider == shared.provider)
                    .ok_or_else(|| "registered runtime provider identity unavailable".to_owned())?;
                Ok(ReceiptRuntime {
                    provider,
                    token_hash: shared.token_hash.clone(),
                })
            })
            .collect();
        let names: Vec<_> = watchers.local.keys().cloned().collect();
        let routines = match runtimes {
            Ok(runtimes) => routine_owner::routine_turn_evidence(pool, &runtimes, &names).await,
            Err(error) => Err(error),
        };
        Ok(Self { watchers, routines })
    }

    pub(super) fn effective_owners(&self, target: u64) -> BTreeMap<String, Option<u64>> {
        self.watchers
            .local
            .iter()
            .map(|(name, local)| {
                let owner = local.or_else(|| self.foreign_owner(name, target));
                (name.clone(), owner)
            })
            .collect()
    }

    fn foreign_owner(&self, name: &str, target: u64) -> Option<u64> {
        let claims = self.watchers.claims(name);
        // Every source's self claim uses the existing watcher-unbound reason, including canaries.
        if claims.contains(&target)
            || self
                .routines
                .as_ref()
                .is_ok_and(|proof| proof.self_claim(name, target))
        {
            return Some(target);
        }
        let routines = self.routines.as_ref().ok()?;
        let mut recorded = routines
            .for_name(name)
            .filter(|proof| !proof.rows.is_empty());
        let receipt = recorded.next();
        if recorded.next().is_some()
            || receipt.is_some_and(|proof| proof.invalid())
            || claims.len() > 1
        {
            return None;
        }
        let channels = receipt.map(|proof| proof.channels()).unwrap_or_default();
        if let Some(owner) = claims.first().copied() {
            return channels
                .iter()
                .all(|channel| *channel == owner)
                .then_some(owner);
        }
        // A single exact key keeps the existing conservative treatment of historical runs.
        channels.first().copied()
    }
}
