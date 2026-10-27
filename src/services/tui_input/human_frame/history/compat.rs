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

#[cfg(test)]
fn v2_value(path: &Path) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (value.get("version")?.as_u64() == Some(2) && value.get("entries").is_none()).then_some(value)
}

#[cfg(test)]
pub(super) fn known_v2(path: &Path, hashes: &[String]) -> Option<bool> {
    let value = v2_value(path)?;
    let entries = value["live"].as_array()?.iter().chain(
        value["passive"]
            .as_array()?
            .iter()
            .filter_map(|p| p.get("entry")),
    );
    let bound = entries.into_iter().any(|e| {
        e["class_hash"]
            .as_str()
            .is_some_and(|h| hashes.iter().any(|x| x == h))
    });
    let planned = value
        .get("planned")
        .and_then(|p| p.as_array())
        .is_some_and(|ps| {
            ps.iter().any(|p| {
                p["frame"]["class_hash"]
                    .as_str()
                    .is_some_and(|h| hashes.iter().any(|x| x == h))
            })
        });
    let unbound = value["passive"].as_array()?.iter().any(|p| {
        p["registration"]["frame"]["class_hash"]
            .as_str()
            .is_some_and(|h| hashes.iter().any(|x| x == h))
    });
    Some(bound || planned || unbound)
}

#[cfg(test)]
pub(super) fn screen_frames_v2(path: &Path) -> Option<Vec<(String, String)>> {
    let value = v2_value(path)?;
    let entries = value["live"].as_array()?.iter().chain(
        value["passive"]
            .as_array()?
            .iter()
            .filter(|p| p["state"] == "retained")
            .filter_map(|p| p.get("entry")),
    );
    Some(
        entries
            .filter(|e| {
                e["provider"] == "claude" && e["events"].as_array().is_some_and(Vec::is_empty)
            })
            .filter_map(|e| {
                Some((
                    e["session"].as_str()?.into(),
                    e["screen_prefix"].as_str()?.into(),
                ))
            })
            .filter(|(_, prefix): &(String, String)| !prefix.is_empty())
            .collect(),
    )
}
