//! Read-only marker cleanup debt shares the owner's queue identity and history.
use super::super::QueueIdentity;
use super::super::v2_types::{AdmissionBind, AttemptIdentity, BindAt, SnapshotV2};
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarkerControl {
    pub(crate) cleanup_id: String,
    pub(crate) identity: QueueIdentity,
    pub(crate) front_receipt: Option<String>,
    pub(crate) epoch: String,
    pub(crate) expected: MarkerRowExpectation,
    pub(crate) paths: Vec<MarkerPathSnapshot>,
    pub(crate) stage: MarkerStage,
    pub(crate) attempt: Option<AttemptIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MarkerRowExpectation {
    Missing {
        owner: HostedOwner,
    },
    Legacy {
        owner: HostedOwner,
        row_id: i64,
        exact_session_key: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MarkerPathSnapshot {
    pub(crate) path: PathBuf,
    pub(crate) parent: [u64; 2],
    pub(crate) entry: MarkerEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MarkerEntry {
    Absent,
    CodexTui { file: [u64; 2] },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum MarkerStage {
    Intent,
    Receipt,
    Outbox { event_id: String },
}

impl SnapshotV2 {
    pub(crate) fn has_control_debt(&self) -> bool {
        !self.a_control.is_empty()
    }

    pub(crate) fn control_identities(&self) -> impl Iterator<Item = &QueueIdentity> {
        self.a_control.iter().map(|control| &control.identity)
    }

    pub(crate) fn control_references_identity(&self, identity: &QueueIdentity) -> bool {
        self.control_identities().any(|old| old == identity)
    }

    pub(crate) fn control_references_bind(&self, bind: &AdmissionBind) -> bool {
        self.a_control.iter().any(|control| {
            control.front_receipt.as_deref() == Some(bind.receipt.as_str())
                || match &bind.at {
                    BindAt::Attempt { identity, attempt } => {
                        &control.identity == identity && control.attempt.as_ref() == Some(attempt)
                    }
                    BindAt::Settled { attempt, .. } => control
                        .attempt
                        .as_ref()
                        .is_some_and(|old| old.nonce == *attempt),
                    BindAt::Queued { .. } | BindAt::Opaque => false,
                }
        })
    }

    pub(crate) fn control_pinned_receipts(&self) -> Vec<String> {
        self.admission_binds
            .iter()
            .filter(|bind| self.control_references_bind(bind))
            .map(|bind| bind.receipt.clone())
            .collect()
    }

    pub(crate) fn expire_terminals(&mut self, now: u64) {
        let pinned: Vec<_> = self
            .terminal_identities
            .iter()
            .filter(|terminal| self.control_references_identity(&terminal.identity))
            .map(|terminal| terminal.identity.clone())
            .collect();
        self.terminal_identities.retain(|terminal| {
            now.saturating_sub(terminal.settled_at_ms) < 86_400_000
                || pinned.contains(&terminal.identity)
        });
    }

    pub(crate) fn prune_unpinned_binds(&mut self) {
        let pinned = self.control_pinned_receipts();
        let len = self.items.len();
        self.admission_binds.retain(|bind| {
            !matches!(bind.at, BindAt::Queued { index, .. } if index >= len)
                || pinned.contains(&bind.receipt)
        });
    }

    pub(crate) fn expire_unpinned_binds(&mut self, now: u64) {
        let pinned = self.control_pinned_receipts();
        self.admission_binds.retain(|bind| {
            !matches!(bind.at, BindAt::Settled { settled_at_ms, .. }
                if now.saturating_sub(settled_at_ms) >= 86_400_000)
                || pinned.contains(&bind.receipt)
        });
    }
}

pub(crate) fn decode(value: Value) -> Result<SnapshotV2, String> {
    let version = value["version"].as_u64();
    if version == Some(2) {
        if value.get("a_control").is_some() {
            return Err("marker control debt requires queue version 3".into());
        }
    } else if version == Some(3) {
        const FIELDS: &[&str] = &[
            "version",
            "items",
            "head_injection",
            "retained_inputs",
            "terminal_identities",
            "namespace",
            "admission_receipts",
            "admission_sequence",
            "admission_binds",
            "owed_drop_receipts",
            "a_control",
        ];
        let object = value.as_object().ok_or("queue V3 must be an object")?;
        if let Some(field) = object
            .keys()
            .find(|field| !FIELDS.contains(&field.as_str()))
        {
            return Err(format!("unknown queue V3 field: {field}"));
        }
    } else {
        return Err("unsupported owned queue version".into());
    }
    let raw_controls = value.get("a_control").cloned();
    let snapshot: SnapshotV2 = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if snapshot.version == 3 {
        let known_controls =
            serde_json::to_value(&snapshot.a_control).map_err(|e| e.to_string())?;
        if raw_controls
            .as_ref()
            .is_some_and(|raw| unknown_keys(raw, &known_controls))
        {
            return Err("unknown queue V3 marker control field".into());
        }
        if snapshot
            .namespace
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err("queue V3 requires a namespace".into());
        }
        for control in &snapshot.a_control {
            if control.cleanup_id.trim().is_empty()
                || control.epoch.trim().is_empty()
                || control.identity.sources.is_empty()
                || control.paths.is_empty()
                || control.paths.iter().any(|path| !path.path.is_absolute())
                || matches!(&control.stage, MarkerStage::Outbox { event_id } if event_id.trim().is_empty())
            {
                return Err("incomplete queue V3 marker control record".into());
            }
        }
    }
    Ok(snapshot)
}

fn unknown_keys(raw: &Value, known: &Value) -> bool {
    match (raw, known) {
        (Value::Object(raw), Value::Object(known)) => raw.iter().any(|(key, value)| {
            known
                .get(key)
                .is_none_or(|field| unknown_keys(value, field))
        }),
        (Value::Array(raw), Value::Array(known)) => raw
            .iter()
            .zip(known)
            .any(|(raw, known)| unknown_keys(raw, known)),
        _ => false,
    }
}

#[cfg(test)]
#[path = "a_read_view_tests.rs"]
mod tests;
