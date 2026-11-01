use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::LazyLock};

const EMBEDDED: &str = include_str!("../../../scripts/tui_o_body_probes.json");

pub(super) fn snapshot() -> Value {
    static SNAPSHOT: LazyLock<Value> = LazyLock::new(|| inspect(EMBEDDED));
    SNAPSHOT.clone()
}

pub(super) fn inspect(raw: &str) -> Value {
    let pending = (|| -> Option<usize> {
        let data: Value = serde_json::from_str(raw).ok()?;
        if data["schema_version"] != 1 {
            return None;
        }
        let rows = data["probes"].as_array().filter(|rows| !rows.is_empty())?;
        let mut ids = BTreeSet::new();
        let mut pending = 0;
        for row in rows {
            for key in ["id", "path", "function", "primitive"] {
                row[key].as_str().filter(|s| !s.is_empty())?;
            }
            if !ids.insert(row["id"].as_str()?)
                || row["line"].as_u64()? == 0
                || !matches!(row["op"].as_str()?, "POST" | "EDIT" | "POST_OR_EDIT")
                || !matches!(row["classification"].as_str()?, "BODY_CAPABLE" | "NONBODY")
            {
                return None;
            }
            match row["status"].as_str()? {
                "VERIFIED" => {}
                "PENDING_0a3" | "PENDING_0b1" | "PENDING_0b2" | "PENDING_0c1" | "PENDING_0c2" => {
                    pending += 1
                }
                _ => return None,
            }
        }
        Some(pending)
    })();
    let coverage = pending == Some(0);
    #[cfg(test)]
    let coverage = coverage || super::tests::mutant("coverage_true");
    json!({"coverage":coverage,"census_hash":hex::encode(Sha256::digest(raw.as_bytes())),
        "pending":pending.unwrap_or(0),
        "error":if pending.is_some() { Value::Null } else { json!("manifest_invalid") }})
}
