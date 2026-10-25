//! Custody owners confirm their durable effects before consent metadata can stop holding an intent.

use super::{Consent, ConsentStore, IntentRef, State, durable};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettlementKey {
    pub nonce: String,
    pub store: String,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Resolution {
    Released,
    Discarded,
    /// Reserved for the legacy binding `target`; held until that binding is the durable approval.
    Returned {
        target: [u8; 32],
    },
    /// `target` is the durable approval; the original may still wait in its custody.
    Adopted {
        target: [u8; 32],
    },
    /// After adoption custody no longer holds the original, so nothing remains to return.
    Closed {
        target: [u8; 32],
    },
}

impl Resolution {
    /// The legacy binding of a returned intent; such intents never release under this consent.
    pub(crate) fn returned_to(self) -> Option<[u8; 32]> {
        match self {
            Self::Returned { target } | Self::Adopted { target } | Self::Closed { target } => {
                Some(target)
            }
            Self::Released | Self::Discarded => None,
        }
    }

    /// Clear may discard an open return; a return retargets until adopted, then only closes.
    fn may_follow(self, previous: Self) -> bool {
        match (previous, self) {
            (Self::Returned { .. }, Self::Returned { .. } | Self::Discarded) => true,
            (Self::Returned { target }, Self::Adopted { target: next })
            | (Self::Adopted { target }, Self::Closed { target: next }) => target == next,
            (Self::Adopted { .. }, Self::Discarded) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settlement {
    pub key: SettlementKey,
    pub resolution: Resolution,
}

impl Consent {
    pub(crate) fn resolution(&self, intent: &IntentRef) -> Option<Resolution> {
        self.settlements
            .iter()
            .find(|settled| settled.key.store == intent.store && settled.key.id == intent.id)
            .map(|settled| settled.resolution)
    }

    /// Open returns stay unresolved: they hold the channel until their legacy binding closes them.
    pub(crate) fn unresolved_intents(&self) -> Vec<IntentRef> {
        self.intents
            .iter()
            .filter(|intent| {
                matches!(
                    self.resolution(intent),
                    None | Some(Resolution::Returned { .. } | Resolution::Adopted { .. })
                )
            })
            .cloned()
            .collect()
    }

    /// Unsettled intents that this consent may still release; returned ones never are.
    fn awaiting(&self) -> impl Iterator<Item = &IntentRef> {
        self.intents
            .iter()
            .filter(|intent| self.resolution(intent).is_none())
    }

    pub(crate) fn releasable_intents(&self) -> Vec<IntentRef> {
        if self.state != State::Applied {
            return Vec::new();
        }
        self.awaiting()
            .filter(|intent| {
                self.intent_previews.is_empty()
                    || self.intent_previews.iter().any(|held| {
                        held.store == intent.store
                            && held.id == intent.id
                            && held.preview == self.preview
                    })
            })
            .cloned()
            .collect()
    }

    /// An Applied retry opens a new generation only for awaiting input bound to another preview.
    pub(crate) fn needs_new_generation(&self) -> bool {
        self.releasable_intents().is_empty()
            && self.awaiting().any(|intent| {
                self.intent_previews.iter().any(|held| {
                    held.store == intent.store
                        && held.id == intent.id
                        && held.preview != self.preview
                })
            })
    }

    fn settle(&mut self, intent: &IntentRef, resolution: Resolution) -> Result<(), String> {
        if !self.intents.contains(intent) {
            return Err("custody identity changed; preserve the unresolved intent".into());
        }
        if let Some(previous) = self
            .settlements
            .iter_mut()
            .find(|previous| previous.key.store == intent.store && previous.key.id == intent.id)
        {
            if previous.resolution != resolution {
                if !resolution.may_follow(previous.resolution) {
                    return Err("custody already has a different confirmed settlement".into());
                }
                previous.resolution = resolution;
            }
            return Ok(());
        }
        if resolution == Resolution::Released && !self.releasable_intents().contains(intent) {
            return Err(
                "release requires a durable Applied receipt for this intent preview".into(),
            );
        }
        if matches!(
            resolution,
            Resolution::Adopted { .. } | Resolution::Closed { .. }
        ) {
            return Err(
                "a return closes only after it was reserved for that legacy binding".into(),
            );
        }
        self.settlements.push(Settlement {
            key: SettlementKey {
                nonce: self.nonce.clone(),
                store: intent.store.clone(),
                id: intent.id.clone(),
            },
            resolution,
        });
        Ok(())
    }
}

impl ConsentStore {
    fn inventory_keys(&self) -> Result<Vec<String>, String> {
        let initial = self.0.keys();
        if !matches!(&initial, Err(error) if error.contains(": policy_publication_pending:")) {
            return initial;
        }
        // Wait on an active publisher's existing lock; an abandoned marker still refuses the read.
        let root = self
            .0
            .0
            .as_deref()
            .ok_or_else(|| self.0.failure("", "store root unavailable"))?;
        for entry in std::fs::read_dir(root).map_err(|error| self.0.failure("", error))? {
            let path = entry.map_err(|error| self.0.failure("", error))?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "pending")
            {
                let name = path
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| self.0.failure("", "invalid pending snapshot name"))?;
                self.0.read_stable(name)?;
            }
        }
        self.0.keys()
    }

    pub(crate) fn load_all(&self) -> Result<Vec<Consent>, String> {
        self.load_each()?.into_iter().collect()
    }

    /// Inventory uncertainty is global; an identified file's error does not hide its neighbors.
    pub(crate) fn load_each(&self) -> Result<Vec<Result<Consent, String>>, String> {
        Ok(self.inventory_keys()?.into_iter().map(|name| {
            let bytes = self.0.read_stable(&name)?.ok_or_else(|| self.0.failure(&name, "snapshot disappeared during inventory; retry the read"))?;
            let raw: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| self.0.failure(&name, format!("consent_snapshot_corrupt: {error}")))?;
            if raw.get("schema").and_then(|value| value.as_u64()).is_some_and(|schema| schema > u64::from(super::SCHEMA)) {
                return Err(self.0.failure(&name, "consent_snapshot_newer: preserve this file and use a matching AgentDesk version"));
            }
            let key = serde_json::from_value::<super::ConsentKey>(raw.get("key").cloned().unwrap_or_default())
                .map_err(|error| self.0.failure(&name, format!("consent_snapshot_corrupt: {error}")))?
                .normalized().map_err(|error| self.0.failure(&name, error))?;
            if durable::key(&key)? != name {
                return Err(self.0.failure(&name, "consent_snapshot_corrupt: filename does not match consent key"));
            }
            self.decode(&bytes, &key)
        }).collect())
    }

    pub(crate) fn unresolved(&self) -> Result<Vec<(Consent, Vec<IntentRef>)>, String> {
        Ok(self
            .load_all()?
            .into_iter()
            .filter_map(|consent| {
                let intents = consent.unresolved_intents();
                (!intents.is_empty()).then_some((consent, intents))
            })
            .collect())
    }

    fn acknowledge(
        &self,
        expected: &Consent,
        intent: &IntentRef,
        resolution: Resolution,
    ) -> Result<Consent, String> {
        self.settle(expected, &[(intent.clone(), resolution)])
    }

    /// Applies every transition in one durable update, or none of them.
    pub(crate) fn settle(
        &self,
        expected: &Consent,
        changes: &[(IntentRef, Resolution)],
    ) -> Result<Consent, String> {
        self.update(expected, |current| {
            changes
                .iter()
                .try_for_each(|(intent, resolution)| current.settle(intent, *resolution))
        })
    }

    /// The caller must have confirmed this exact reference was durably discarded by its custody owner.
    pub(crate) fn acknowledge_discard(
        &self,
        expected: &Consent,
        intent: &IntentRef,
    ) -> Result<Consent, String> {
        self.acknowledge(expected, intent, Resolution::Discarded)
    }
}

/// Both operations remain durable and idempotent by (store, id) after restart or acknowledgement failure.
/// Nonce identifies the receipt; changing it must never repeat a payload.
#[async_trait::async_trait]
pub(crate) trait CustodySink: Send {
    async fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String>;
    async fn wake(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String>;
}

pub(crate) async fn release(
    store: &ConsentStore,
    expected: &Consent,
    sink: &mut impl CustodySink,
) -> Result<Consent, String> {
    let mut current = store.load(&expected.key)?.ok_or("consent missing")?;
    if current.nonce != expected.nonce
        || current.preview != expected.preview
        || current.apply != expected.apply
        || current.decision != expected.decision
        || current.state != State::Applied
    {
        return Err("release requires the current durable Applied receipt".into());
    }
    for intent in current.releasable_intents() {
        let key = SettlementKey {
            nonce: current.nonce.clone(),
            store: intent.store.clone(),
            id: intent.id.clone(),
        };
        sink.release(&key, &intent).await?;
        sink.wake(&key, &intent).await?;
        current = store.acknowledge(&current, &intent, Resolution::Released)?;
    }
    Ok(current)
}

#[cfg(test)]
#[path = "settlement_tests.rs"]
pub(crate) mod tests;
