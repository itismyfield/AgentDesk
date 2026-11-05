use super::evidence;
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

pub(super) fn packaged(
    root: &std::path::Path,
    raw: &str,
    binary: &std::path::Path,
    head: &Value,
) -> Value {
    use std::{fs, io::Read};
    let load = || -> Option<(Value, Value)> {
        let read = |name: &str| -> Option<Vec<u8>> {
            let path = root.join("runtime").join(name);
            let meta = fs::symlink_metadata(&path).ok()?;
            if !meta.is_file() || meta.len() > 16 * 1024 * 1024 {
                return None;
            }
            fs::read(path).ok()
        };
        let source: Value = serde_json::from_slice(&read("release-source.json")?).ok()?;
        let attestation = &source["legacy_body_census"];
        let proof_bytes = read("legacy_body_census-test-evidence.json")?;
        let identity_bytes = read("legacy_body_census-test-identity.json")?;
        let proof: Value = serde_json::from_slice(&proof_bytes).ok()?;
        let identity: Value = serde_json::from_slice(&identity_bytes).ok()?;
        let mut digest = Sha256::new();
        let mut file = fs::File::open(binary).ok()?;
        let mut bytes = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut bytes).ok()?;
            if count == 0 {
                break;
            }
            digest.update(&bytes[..count]);
        }
        if source["repo_dirty"] != "false"
            || head.as_str().is_none()
            || source["repo_head"] != *head
            || attestation["available"] != true
            || attestation["runtime_binary_sha256"] != hex::encode(digest.finalize())
            || attestation["evidence_sha256"] != hex::encode(Sha256::digest(&proof_bytes))
            || attestation["identity_sha256"] != hex::encode(Sha256::digest(&identity_bytes))
            || identity["build_sha"] != *head
            || identity["dependency_hash"]
                != hex::encode(Sha256::digest(include_bytes!("../../../Cargo.lock")))
        {
            return None;
        }
        for key in ["ci_run_id", "ci_run_attempt"] {
            if attestation[key].as_str().is_none_or(str::is_empty)
                || attestation[key] != identity[key]
                || attestation[key] != proof[key]
            {
                return None;
            }
        }
        if attestation["ci_run_attempt"] != "1" {
            return None;
        }
        Some((proof, identity))
    };
    let inputs = load();
    inspect_with_evidence(
        raw,
        inputs.as_ref().map(|v| &v.0),
        inputs.as_ref().map(|v| &v.1),
    )
}

pub(super) fn snapshot_for_build(root: &std::path::Path, head: &Value) -> Value {
    // Artifact I/O and hashing run once on the journal writer, never on a send or health task.
    std::env::current_exe().map_or_else(
        |_| snapshot(),
        |binary| packaged(root, EMBEDDED, &binary, head),
    )
}

pub(super) fn inspect(raw: &str) -> Value {
    inspect_with_evidence(raw, None, None)
}

pub(super) fn inspect_with_evidence(
    raw: &str,
    proof: Option<&Value>,
    identity: Option<&Value>,
) -> Value {
    let data: Option<Value> = serde_json::from_str(raw).ok();
    let pending = (|| -> Option<usize> {
        let data = data.as_ref()?;
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
            let mut fields =
                BTreeSet::from(["id", "path", "entrypoint", "guard", "status", "verified_by"]);
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
    let hash = hex::encode(Sha256::digest(raw.as_bytes()));
    let evidence_linked = pending.is_some()
        && data
            .as_ref()
            .is_some_and(|data| evidence::verify(data, &hash, proof, identity));
    let coverage = instrumentation_complete && evidence_linked;
    json!({"coverage":coverage,"instrumentation_complete":instrumentation_complete,"evidence_linked":evidence_linked,"census_hash":hash,
        "evidence_error":if proof.is_none() { json!("execution_evidence_missing") } else if !evidence_linked { json!("execution_evidence_invalid_or_unlinked") } else { Value::Null },
        "pending":pending.unwrap_or(0),
        "error":if pending.is_some() { Value::Null } else { json!("manifest_invalid") }})
}
