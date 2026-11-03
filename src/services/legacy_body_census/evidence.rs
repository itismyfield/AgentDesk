use serde_json::Value;
use std::collections::BTreeSet;

pub(super) fn linked_ids(manifest: &Value) -> Option<BTreeSet<&str>> {
    let mut linked = BTreeSet::new();
    for group in ["probes", "transport_probes", "claim_probes"] {
        for row in manifest[group].as_array()? {
            let ids = row["verified_by"]
                .as_array()
                .filter(|ids| !ids.is_empty())?;
            let mut unique = BTreeSet::new();
            for id in ids {
                let id = id
                    .as_str()
                    .filter(|id| !id.is_empty() && id.contains("::"))?;
                if !unique.insert(id) {
                    return None;
                }
                linked.insert(id);
            }
            if let Some(recording) = row["recording_test"].as_str()
                && !unique
                    .iter()
                    .any(|id| id.rsplit("::").next() == Some(recording))
            {
                return None;
            }
        }
    }
    (!linked.is_empty()).then_some(linked)
}

fn digest(value: &Value, length: usize) -> bool {
    value.as_str().is_some_and(|value| {
        value.len() == length
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

// Identity comes from the build/package verifier, never from the supplied evidence itself.
pub(super) fn verify(
    manifest: &Value,
    hash: &str,
    proof: Option<&Value>,
    identity: Option<&Value>,
) -> bool {
    (|| -> Option<()> {
        let (proof, identity) = (proof?, identity?);
        if proof["schema_version"] != 1
            || proof["policy_version"] != 1
            || !digest(&identity["build_sha"], 40)
            || !digest(&identity["dependency_hash"], 64)
            || proof["build_sha"] != identity["build_sha"]
            || proof["dependency_hash"] != identity["dependency_hash"]
            || proof["census_hash"] != hash
            || !digest(&identity["binary_sha256"], 64)
            || proof["binary_sha256"] != identity["binary_sha256"]
        {
            return None;
        }
        let required = proof["required_test_ids"].as_array()?;
        let mut planned = BTreeSet::new();
        for id in required {
            if !planned.insert(id.as_str().filter(|id| !id.is_empty())?) {
                return None;
            }
        }
        let expected = identity["required_test_ids"]
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<BTreeSet<_>>>()?;
        if expected.len() != identity["required_test_ids"].as_array()?.len()
            || planned != expected
            || !linked_ids(manifest)?.is_subset(&planned)
        {
            return None;
        }
        let mut executed = BTreeSet::new();
        for test in proof["tests"].as_array()? {
            let id = test["id"].as_str()?;
            if !planned.contains(id)
                || !executed.insert(id)
                || test["executed"].as_u64() != Some(1)
                || test["failed"].as_u64() != Some(0)
                || test["skipped"].as_u64() != Some(0)
                || test["binary_sha256"] != proof["binary_sha256"]
            {
                return None;
            }
        }
        (executed == planned).then_some(())
    })()
    .is_some()
}
