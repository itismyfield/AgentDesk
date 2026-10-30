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

/// What the V2 writer always opens with (`v2::Snapshot::encode`); a V1 store opens with
/// `entries`, and any other version or spacing takes the full read.
const V2_PREFIX: &[u8] = b"{\"version\":2,";

#[cfg(test)]
thread_local! {
    /// (full-document parses, prefix bytes read, envelope inspections) on this thread, for the
    /// cost tests.
    pub(super) static COST: std::cell::Cell<(usize, usize, usize)> =
        const { std::cell::Cell::new((0, 0, 0)) };
}

/// The one place history text becomes a value.
pub(super) fn parse(raw: &[u8]) -> Result<serde_json::Value, String> {
    #[cfg(test)]
    COST.with(|cost| {
        let (parses, bytes, inspects) = cost.get();
        cost.set((parses + 1, bytes, inspects));
    });
    serde_json::from_slice(raw).map_err(|e| e.to_string())
}

/// `inspect`'s envelope check on a parsed value: whether it is V2.
pub(super) fn envelope(value: &serde_json::Value) -> Result<bool, String> {
    if value.get("version").is_none() {
        return Ok(false);
    }
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(2)
        || value.get("entries").is_some()
    {
        return Err("unsupported frame history envelope".into());
    }
    Ok(true)
}

pub(super) fn inspect(raw: &[u8]) -> Result<HistoryRead, String> {
    #[cfg(test)]
    COST.with(|cost| {
        let (parses, bytes, inspects) = cost.get();
        cost.set((parses, bytes, inspects + 1));
    });
    let value = parse(raw)?;
    if envelope(&value)? {
        let snapshot: HistoryV2 = serde_json::from_value(value).map_err(|e| e.to_string())?;
        return Ok(HistoryRead::ReadOnlyV2(snapshot));
    }
    serde_json::from_value(value)
        .map(HistoryRead::Legacy)
        .map_err(|e| e.to_string())
}

/// Whether the file opens with the V2 writer's envelope, reading at most 16 bytes. It only
/// refuses early what the full read refuses anyway; it never vouches for a valid V2 file.
pub(super) fn v2_prefix(path: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let mut head = [0u8; 16];
    let mut file = std::fs::File::open(path)?;
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    #[cfg(test)]
    COST.with(|cost| {
        let (parses, bytes, inspects) = cost.get();
        cost.set((parses, bytes + filled, inspects));
    });
    Ok(head[..filled].starts_with(V2_PREFIX))
}

pub(super) fn read_legacy(path: &Path) -> Result<Store, String> {
    match v2_prefix(path) {
        Ok(true) => return Err("frame history V2 is read-only".into()),
        Ok(false) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(e.to_string()),
    }
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

fn v2_value(path: &Path) -> Option<serde_json::Value> {
    let value = parse(&std::fs::read(path).ok()?).ok()?;
    (value.get("version")?.as_u64() == Some(2) && value.get("entries").is_none()).then_some(value)
}

pub(super) fn known_v2(path: &Path, hashes: &[String]) -> Option<bool> {
    let value = v2_value(path)?;
    // An abandoned frame was proven never pasted, so it marks no native row as injected.
    let passive = || {
        value["passive"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["state"] != "abandoned")
    };
    let entries = value["live"]
        .as_array()?
        .iter()
        .chain(passive().filter_map(|p| p.get("entry")));
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
    let unbound = passive().any(|p| {
        p["registration"]["frame"]["class_hash"]
            .as_str()
            .is_some_and(|h| hashes.iter().any(|x| x == h))
    });
    Some(bound || planned || unbound)
}

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
