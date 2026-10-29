//! Runtime membership and exact owner evidence pinned across asynchronous observations.
use super::super::cross_runtime_owners::{OwnerEvidence, WatcherEvidence};
use super::*;
use crate::services::discord::health::{HealthRegistry, UnboundRuntimeSnapshot};
use crate::services::discord::tmux_watcher_registry::{
    owner_observation_epoch, with_owner_publication,
};

#[derive(Clone)]
pub(super) struct Membership {
    registry: Option<Arc<HealthRegistry>>,
    snapshot: UnboundRuntimeSnapshot,
}

impl Membership {
    pub(super) async fn collect(shared: &Arc<SharedData>) -> Result<Self, String> {
        let registry = shared.health_registry.upgrade();
        let snapshot = match &registry {
            Some(registry) => {
                let snapshot = registry.unbound_runtime_snapshot().await;
                if !snapshot
                    .entries
                    .iter()
                    .any(|(_, runtime)| Arc::ptr_eq(runtime, shared))
                {
                    return Err(rejected("membership"));
                }
                snapshot
            }
            None => UnboundRuntimeSnapshot {
                generation: 0,
                entries: vec![(shared.provider.as_str().into(), shared.clone())],
            },
        };
        Ok(Self { registry, snapshot })
    }

    fn same(&self, other: &Self) -> bool {
        match (&self.registry, &other.registry) {
            (Some(old), Some(new)) if Arc::ptr_eq(old, new) => {}
            (None, None) => {}
            _ => return false,
        }
        self.snapshot.generation == other.snapshot.generation
            && self.snapshot.entries.len() == other.snapshot.entries.len()
            && self
                .snapshot
                .entries
                .iter()
                .zip(&other.snapshot.entries)
                .all(|((old_name, old), (new_name, new))| {
                    old_name == new_name && Arc::ptr_eq(old, new)
                })
    }

    fn current_registry(&self, shared: &SharedData) -> bool {
        match (&self.registry, shared.health_registry.upgrade()) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, &new),
            (None, None) => true,
            _ => false,
        }
    }

    fn with_current<R>(&self, publish: impl FnOnce() -> R) -> Result<R, &'static str> {
        match &self.registry {
            Some(registry) => registry.with_unbound_membership(self.snapshot.generation, publish),
            None => Ok(publish()),
        }
    }
}

#[derive(Clone)]
pub(super) struct OwnerInputs {
    shared: Weak<SharedData>,
    membership: Membership,
    epoch: u64,
    evidence: OwnerEvidence,
    completed_scheduled: Result<Vec<completed_scheduled::Proof>, String>,
    ports: Arc<LivePorts>,
    nonces: BTreeMap<String, Result<Option<Vec<u8>>, String>>,
}

fn nonce(name: &str) -> Result<Option<Vec<u8>>, String> {
    fn present(bytes: Vec<u8>) -> Result<Option<Vec<u8>>, String> {
        let text = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
        if text.trim().is_empty() {
            return Err("blank spawn nonce".into());
        }
        Ok(Some(bytes))
    }
    let canonical = marker_path(name, "spawn_nonce");
    let legacy =
        std::path::PathBuf::from(tmux_common::legacy_tmp_session_path(name, "spawn_nonce"));
    match std::fs::read(&canonical) {
        Ok(bytes) => present(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && canonical != legacy => {
            match std::fs::read(legacy) {
                Ok(bytes) => present(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

impl OwnerInputs {
    pub(super) async fn collect(
        shared: &Arc<SharedData>,
        membership: Membership,
        epoch: u64,
        names: &[String],
        ports: Arc<LivePorts>,
    ) -> Result<Self, String> {
        let watchers = membership
            .with_current(|| {
                with_owner_publication(epoch, |guard| {
                    WatcherEvidence::capture(guard, shared, &membership.snapshot.entries, names)
                })
            })
            .and_then(std::convert::identity)
            .map_err(rejected)?;
        let pool = shared.pg_pool.as_ref().ok_or("database unavailable")?;
        let evidence = OwnerEvidence::collect(watchers, pool, &membership.snapshot.entries).await?;
        let completed_scheduled =
            completed_scheduled::collect(pool, &evidence.routines, ports.clone()).await;
        let nonces = names
            .iter()
            .map(|name| (name.clone(), nonce(name)))
            .collect();
        let result = Self {
            shared: Arc::downgrade(shared),
            membership,
            epoch,
            evidence,
            completed_scheduled,
            ports,
            nonces,
        };
        result.with_current(|| ()).map_err(rejected)?;
        Ok(result)
    }

    pub(super) fn same(&self, other: &Self) -> bool {
        self.membership.same(&other.membership)
            && self.epoch == other.epoch
            && self.evidence == other.evidence
            && self.completed_scheduled == other.completed_scheduled
            && self.nonces == other.nonces
    }

    pub(super) fn effective_owners(&self, target: u64) -> BTreeMap<String, Option<u64>> {
        let mut owners = self.evidence.effective_owners(target);
        for (name, owner) in &mut owners {
            if *owner != Some(target)
                && self.evidence.watchers.local.get(name) == Some(&None)
                && self.nonces.get(name).is_some_and(Result::is_err)
            {
                *owner = None;
            }
        }
        if let Ok(proofs) = &self.completed_scheduled {
            for proof in proofs {
                if self.completed_scheduled_admitted(proof)
                    && owners.get(&proof.name).is_some_and(Option::is_none)
                {
                    owners.insert(proof.name.clone(), Some(proof.channel));
                }
            }
        }
        owners
    }

    fn completed_scheduled_admitted(&self, proof: &completed_scheduled::Proof) -> bool {
        self.nonce_matches(&proof.name, &proof.binding.execution_nonce)
            && self.admits(&proof.name, proof.channel)
    }

    pub(super) fn completed_scheduled_names(&self, channel: u64) -> Vec<&str> {
        self.completed_scheduled
            .iter()
            .flatten()
            .filter(|proof| proof.channel == channel && self.completed_scheduled_admitted(proof))
            .map(|proof| proof.name.as_str())
            .collect()
    }

    /// Whether every runtime's watcher owner and routine receipt for `name` was read and names no
    /// channel but `target`; a conflicting claim or an unread source leaves the owner unresolved.
    pub(super) fn admits(&self, name: &str, target: u64) -> bool {
        let watchers = &self.evidence.watchers;
        let own =
            |owner: Option<&Option<u64>>| owner.is_some_and(|o| o.is_none_or(|o| o == target));
        let Ok(routines) = &self.evidence.routines else {
            return false;
        };
        // A name no registered runtime keyed was never queried, which is not an empty receipt.
        let mut receipts = routines.for_name(name).peekable();
        receipts.peek().is_some()
            && receipts.all(|r| !r.invalid() && r.channels().iter().all(|c| *c == target))
            && own(watchers.local.get(name))
            && !watchers.registered.is_empty()
            && watchers
                .registered
                .iter()
                .all(|owners| own(owners.get(name)))
            && matches!(self.nonces.get(name), Some(Ok(_)))
    }

    pub(super) fn local_conflict(&self, name: &str) -> bool {
        self.evidence.local_conflict(name)
    }

    fn nonce_matches(&self, name: &str, nonce: &str) -> bool {
        matches!(self.nonces.get(name), Some(Ok(Some(bytes))) if bytes == nonce.as_bytes())
    }

    pub(super) fn with_current<R>(&self, publish: impl FnOnce() -> R) -> Result<R, &'static str> {
        let shared = self.shared.upgrade().ok_or("membership")?;
        if !self.membership.current_registry(&shared) {
            return Err("membership");
        }
        self.membership
            .with_current(|| {
                with_owner_publication(self.epoch, |guard| {
                    let names: Vec<_> = self.evidence.watchers.local.keys().cloned().collect();
                    let fresh = WatcherEvidence::capture(
                        guard,
                        &shared,
                        &self.membership.snapshot.entries,
                        &names,
                    );
                    if fresh != self.evidence.watchers {
                        return Err("owner_epoch");
                    }
                    if self
                        .completed_scheduled
                        .as_ref()
                        .is_ok_and(|proofs| proofs.iter().any(|proof| !proof.sequence_current()))
                    {
                        return Err("owner_epoch");
                    }
                    Ok(publish())
                })
            })
            .and_then(std::convert::identity)
            .and_then(std::convert::identity)
    }

    pub(super) fn current(&self) -> bool {
        self.with_current(|| ()).map_err(rejected).is_ok()
    }

    pub(super) fn files_current(&self) -> bool {
        let scheduled_current = match &self.completed_scheduled {
            Ok(proofs) => self
                .ports
                .scoped(|| proofs.iter().all(completed_scheduled::Proof::current)),
            Err(_) => true,
        };
        scheduled_current
            && self
                .nonces
                .iter()
                .all(|(name, observed)| nonce(name) == *observed)
    }

    pub(super) fn nonces_valid<'a>(&self, mut names: impl Iterator<Item = &'a str>) -> bool {
        names.all(|name| matches!(self.nonces.get(name), Some(Ok(_))))
    }
}

pub(super) fn epoch() -> Result<u64, String> {
    owner_observation_epoch().ok_or_else(|| rejected("owner_epoch"))
}

pub(super) fn rejected(reason: &'static str) -> String {
    hold_leaf::superseded(reason);
    reason.into()
}

pub(super) fn superseded_error(reason: &str) -> bool {
    matches!(
        reason,
        "owner_epoch"
            | "membership"
            | "providers_busy"
            | "watch_busy"
            | "registry_busy"
            | "core_busy"
    )
}

#[cfg(test)]
#[path = "newlive_ownership_file_tests.rs"]
mod file_tests;
