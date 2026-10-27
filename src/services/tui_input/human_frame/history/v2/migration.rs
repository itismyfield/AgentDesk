//! Reads the history file under the registry lock: a V1 store becomes passive evidence only in
//! the transaction that publishes it, and any other envelope is refused without a write.
use super::*;

/// What a missing history file reads as; it migrates to an empty V2 snapshot.
pub(super) const MISSING: &str = "{\"entries\":[]}";

pub(super) fn restore(raw: &str) -> Result<Snapshot, String> {
    match compat::inspect(raw.as_bytes())? {
        compat::HistoryRead::Legacy(store) => Ok(Snapshot {
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
        compat::HistoryRead::ReadOnlyV2(_) => {
            serde_json::from_str::<Snapshot>(raw).map_err(|e| e.to_string())
        }
    }
}
