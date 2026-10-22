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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settlement {
    pub key: SettlementKey,
    pub resolution: Resolution,
}

impl Consent {
    pub(crate) fn unresolved_intents(&self) -> Vec<IntentRef> {
        self.intents
            .iter()
            .filter(|intent| {
                !self
                    .settlements
                    .iter()
                    .any(|settled| settled.key.store == intent.store && settled.key.id == intent.id)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn releasable_intents(&self) -> Vec<IntentRef> {
        if self.state != State::Applied {
            return Vec::new();
        }
        self.unresolved_intents()
            .into_iter()
            .filter(|intent| {
                self.intent_previews.is_empty()
                    || self.intent_previews.iter().any(|held| {
                        held.store == intent.store
                            && held.id == intent.id
                            && held.preview == self.preview
                    })
            })
            .collect()
    }
}

impl ConsentStore {
    pub(crate) fn load_all(&self) -> Result<Vec<Consent>, String> {
        self.0.keys()?.into_iter().map(|name| {
            let bytes = self.0.read_stable(&name)?.ok_or_else(|| self.0.failure(&name, "snapshot disappeared during inventory; retry the read"))?;
            let raw: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|error| self.0.failure(&name, format!("consent_snapshot_corrupt: {error}")))?;
            if raw.get("schema").and_then(|value| value.as_u64()).is_some_and(|schema| schema > 1) {
                return Err(self.0.failure(&name, "consent_snapshot_newer: preserve this file and use a matching AgentDesk version"));
            }
            let key = serde_json::from_value::<super::ConsentKey>(raw.get("key").cloned().unwrap_or_default())
                .map_err(|error| self.0.failure(&name, format!("consent_snapshot_corrupt: {error}")))?
                .normalized().map_err(|error| self.0.failure(&name, error))?;
            if durable::key(&key)? != name {
                return Err(self.0.failure(&name, "consent_snapshot_corrupt: filename does not match consent key"));
            }
            self.decode(&bytes, &key)
        }).collect()
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
        self.update(expected, |current| {
            if !current.intents.contains(intent) {
                return Err("custody identity changed; preserve the unresolved intent".into());
            }
            if let Some(previous) = current
                .settlements
                .iter()
                .find(|previous| previous.key.store == intent.store && previous.key.id == intent.id)
            {
                return if previous.resolution == resolution {
                    Ok(())
                } else {
                    Err("custody already has a different confirmed settlement".into())
                };
            }
            if resolution == Resolution::Released && !current.releasable_intents().contains(intent)
            {
                return Err(
                    "release requires a durable Applied receipt for this intent preview".into(),
                );
            }
            current.settlements.push(Settlement {
                key: SettlementKey {
                    nonce: current.nonce.clone(),
                    store: intent.store.clone(),
                    id: intent.id.clone(),
                },
                resolution,
            });
            Ok(())
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

/// Both operations must be durable and idempotent by the full key, including after a process restart.
/// A release may succeed before consent acknowledgement fails; replay must never duplicate its payload.
pub(crate) trait CustodySink {
    fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String>;
    fn wake(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String>;
}

pub(crate) fn release(
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
        sink.release(&key, &intent)?;
        sink.wake(&key, &intent)?;
        current = store.acknowledge(&current, &intent, Resolution::Released)?;
    }
    Ok(current)
}

#[cfg(test)]
#[path = "settlement_tests.rs"]
mod tests;
