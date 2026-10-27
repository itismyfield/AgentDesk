//! The rollback reader recognizes V2 without flattening it into a writable V1 store.
use super::*;

#[derive(Serialize, Deserialize)]
pub(super) struct HistoryV2 {
    pub(super) version: u32,
    pub(super) live: Vec<Entry>,
    pub(super) passive: Vec<serde_json::Value>,
}

pub(super) enum HistoryRead {
    Legacy(Store),
    ReadOnlyV2(HistoryV2),
}

pub(super) fn inspect(raw: &[u8]) -> Result<HistoryRead, String> {
    let value: serde_json::Value = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    if value.get("version").is_some() {
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(2)
            || value.get("entries").is_some()
        {
            return Err("unsupported frame history envelope".into());
        }
        let snapshot: HistoryV2 = serde_json::from_value(value).map_err(|e| e.to_string())?;
        return Ok(HistoryRead::ReadOnlyV2(snapshot));
    }
    serde_json::from_value(value)
        .map(HistoryRead::Legacy)
        .map_err(|e| e.to_string())
}

pub(super) fn read_legacy(path: &Path) -> Result<Store, String> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(e.to_string()),
    };
    match inspect(&raw)? {
        HistoryRead::Legacy(store) => Ok(store),
        HistoryRead::ReadOnlyV2(snapshot) => Err(format!(
            "frame history V2 is read-only ({} live, {} passive)",
            snapshot.live.len(),
            snapshot.passive.len()
        )),
    }
}
