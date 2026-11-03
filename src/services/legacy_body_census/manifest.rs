use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::LazyLock};

const EMBEDDED: &str = include_str!("../../../scripts/tui_o_body_probes.json");

pub(super) const EXPECTED_PROBE_ROWS: usize = 68;
const EXPECTED_TRANSPORT_ROWS: usize = 18;

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
        let rows = data["probes"]
            .as_array()
            .filter(|rows| rows.len() == EXPECTED_PROBE_ROWS)?;
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
        let transport = data["transport_probes"]
            .as_array()
            .filter(|rows| rows.len() == EXPECTED_TRANSPORT_ROWS)?;
        for row in transport {
            for key in ["id", "path", "entrypoint"] {
                row[key].as_str().filter(|s| !s.is_empty())?;
            }
            if !ids.insert(row["id"].as_str()?)
                || !matches!(row["op"].as_str()?, "POST" | "EDIT" | "DELETE")
                || !matches!(
                    row["guard"].as_str()?,
                    "RAW_OPERATION_OBSERVED_ONCE" | "O_CREATED_ACK_ONCE"
                )
            {
                return None;
            }
            match row["status"].as_str()? {
                "VERIFIED" => {}
                "PENDING_0a3" | "PENDING_0b2" => pending += 1,
                _ => return None,
            }
        }
        let claims = data["claim_probes"]
            .as_array()
            .filter(|rows| rows.len() == 3)?;
        for (id, path, entrypoint, guard) in [
            (
                "CLAIM_GATE_REASON",
                "src/services/tui_o/cutover/channel_gate.rs",
                "decide_with_snapshot",
                "CLAIM_REASON_AT_DECISION",
            ),
            (
                "CLAIM_ADOPTION_REASON",
                "src/services/tui_o/channel_policy/adoption.rs",
                "claim_body_from",
                "CLAIM_REASON_UNDER_STATE_LOCK",
            ),
            (
                "CLAIM_SITE_NONE_HELD",
                "src/services/tui_o/cutover/channel_gate.rs",
                "claim_then_send_at/claim_then_send_held_at",
                "SITE_NONE_HELD_INHERITED_ONLY",
            ),
        ] {
            let row = claims.iter().find(|row| row["id"] == id)?;
            let mut fields = BTreeSet::from(["id", "path", "entrypoint", "guard", "status"]);
            if id == "CLAIM_SITE_NONE_HELD" {
                fields.extend(["consumer_path", "consumer_function", "site"]);
            }
            if row
                .as_object()?
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
                != fields
            {
                return None;
            }
            if !ids.insert(id)
                || row["path"] != path
                || row["entrypoint"] != entrypoint
                || row["guard"] != guard
            {
                return None;
            }
            if id == "CLAIM_SITE_NONE_HELD"
                && (row["consumer_path"] != "src/services/discord/standby_relay.rs"
                    || row["consumer_function"] != "run_standby_relay"
                    || row["site"] != "W06")
            {
                return None;
            }
            match row["status"].as_str()? {
                "VERIFIED" => {}
                "PENDING_0a2" => pending += 1,
                _ => return None,
            }
        }
        Some(pending)
    })();
    let instrumentation_complete = pending == Some(0);
    // Execution evidence needs its own verifier before instrumentation can certify coverage.
    let coverage = false;
    json!({"coverage":coverage,"instrumentation_complete":instrumentation_complete,"census_hash":hex::encode(Sha256::digest(raw.as_bytes())),
        "pending":pending.unwrap_or(0),
        "error":if pending.is_some() { Value::Null } else { json!("manifest_invalid") }})
}
