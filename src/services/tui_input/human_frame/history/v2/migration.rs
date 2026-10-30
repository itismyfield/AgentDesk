//! Reads the history file under the registry lock: a V1 store becomes passive evidence only in
//! the transaction that publishes it, and any other envelope is refused without a write.
use super::*;

/// What a missing history file reads as; it migrates to an empty V2 snapshot.
pub(super) const MISSING: &str = "{\"entries\":[]}";

/// One parse of the file: a V2 snapshot as written, or a V1 store as passive evidence.
pub(super) fn restore(raw: &str) -> Result<Snapshot, String> {
    match decode(raw)? {
        Decoded::Legacy(store) => Ok(Snapshot {
            version: 2,
            passive: store
                .entries
                .into_iter()
                .map(|entry| Passive {
                    registration: None,
                    state: if entry.events.is_empty() {
                        "retained"
                    } else {
                        "settled"
                    }
                    .into(),
                    entry: Some(entry),
                    encoded: None,
                })
                .collect(),
            ..Default::default()
        }),
        Decoded::V2(snapshot) => Ok(snapshot),
    }
}

pub(super) enum Decoded {
    Legacy(Store),
    V2(Snapshot),
}

/// The rollback reader's envelope check and the typed snapshot from the same single parse.
pub(super) fn decode(raw: &str) -> Result<Decoded, String> {
    let value = compat::parse(raw.as_bytes())?;
    if compat::envelope(&value)? {
        return serde_json::from_value(value)
            .map(Decoded::V2)
            .map_err(|e| e.to_string());
    }
    serde_json::from_value(value)
        .map(Decoded::Legacy)
        .map_err(|e| e.to_string())
}
