use super::*;
use crate::services::turn_orchestrator::queued_head::compat::{self, SnapshotRead};
use serde_json::{Value, json};

const V2_BYTES: &str = "{\n  \"version\": 2,\n  \"items\": [],\n  \"head_injection\": null,\n  \"retained_inputs\": [],\n  \"terminal_identities\": [],\n  \"namespace\": \"marker-read\",\n  \"admission_sequence\": 0\n}";

fn v2() -> Value {
    serde_json::from_str(V2_BYTES).unwrap()
}

fn control() -> Value {
    let root = if cfg!(windows) {
        "C:/fixture"
    } else {
        "/fixture"
    };
    json!({
        "cleanup_id":"cleanup-a",
        "identity":{"primary":42,"queued_generation":7,
            "sources":[{"message_id":42,"queued_generation":7}]},
        "front_receipt":"ticket-a",
        "epoch":"epoch-a",
        "expected":{"kind":"legacy","row_id":17,"exact_session_key":"logical-a",
            "owner":{"provider":"codex","discord_token_hash":"fixture-token",
                "channel_id":"42","logical_key":"logical-a","owner_node":"fixture-node",
                "runtime_root":root}},
        "paths":[{"path":format!("{root}/canonical.kind"),"parent":[1,2],
            "entry":{"kind":"codex_tui","file":[1,3]}},
            {"path":format!("{root}/legacy.kind"),"parent":[1,4],"entry":{"kind":"absent"}}],
        "stage":{"kind":"outbox","event_id":"marker-read/cleanup-a/ticket-a/epoch-a"},
        "attempt":null
    })
}

fn v3() -> Value {
    let mut snapshot = v2();
    snapshot["version"] = json!(3);
    snapshot["a_control"] = json!([control()]);
    snapshot
}

fn read(value: Value) -> SnapshotV2 {
    let SnapshotRead::ReadOnlyV2(snapshot) = compat::inspect(&value.to_string()).unwrap() else {
        panic!("owned snapshot decoded as legacy")
    };
    *snapshot
}

#[test]
fn v2_empty_extension_preserves_existing_writer_bytes_and_unknown_field_compatibility() {
    let mut original = v2();
    original["old_optional_extension"] = json!({"kept_by_old_reader":true});
    let snapshot = read(original);
    assert_eq!(serde_json::to_string_pretty(&snapshot).unwrap(), V2_BYTES);
    assert!(!snapshot.has_control_debt());
    assert!(!snapshot.holds_input());
    assert_eq!(snapshot.version, 2);
}

#[test]
fn each_unfinished_marker_stage_is_readable_owned_debt_without_recreating_payload() {
    for stage in [
        json!({"kind":"intent"}),
        json!({"kind":"receipt"}),
        json!({"kind":"outbox","event_id":"stable-event"}),
    ] {
        let mut snapshot = v3();
        snapshot["a_control"][0]["stage"] = stage;
        let snapshot = read(snapshot);
        assert!(snapshot.holds_input());
        assert!(snapshot.items.is_empty());
        assert!(snapshot.head_injection.is_none());
        assert!(snapshot.retained_inputs.is_empty());
        assert_eq!(snapshot.control_identities().count(), 1);
        let identity = &snapshot.a_control[0].identity;
        assert!(snapshot.control_references_identity(identity));
        let mut other_generation = identity.clone();
        other_generation.queued_generation += 1;
        assert!(!snapshot.control_references_identity(&other_generation));
    }
}

#[test]
fn v3_rejects_unknown_schema_or_incomplete_control_instead_of_losing_debt() {
    let mut unknown_field = v3();
    unknown_field["future_required_record"] = json!({"must_not_drop":true});
    let mut unknown_stage = v3();
    unknown_stage["a_control"][0]["stage"] = json!({"kind":"future_stage"});
    let mut missing_namespace = v3();
    missing_namespace["namespace"] = Value::Null;
    let mut unknown_control_field = v3();
    unknown_control_field["a_control"][0]["discard_me"] = json!(true);
    let mut relative_path = v3();
    relative_path["a_control"][0]["paths"][0]["path"] = json!("relative.kind");
    for invalid in [
        unknown_field,
        unknown_stage,
        missing_namespace,
        unknown_control_field,
        relative_path,
    ] {
        assert!(compat::inspect(&invalid.to_string()).is_err(), "{invalid}");
    }
    let mut hidden_in_v2 = v3();
    hidden_in_v2["version"] = json!(2);
    assert!(compat::inspect(&hidden_in_v2.to_string()).is_err());
}

#[test]
fn cleared_v3_debt_roundtrips_without_changing_the_v2_empty_extension_format() {
    let mut snapshot = read(v3());
    snapshot.a_control.clear();
    let bytes = serde_json::to_string_pretty(&snapshot).unwrap();
    assert!(!bytes.contains("a_control"));
    let restored = read(serde_json::from_str(&bytes).unwrap());
    assert_eq!(restored.version, 3);
    assert!(!restored.holds_input());
    assert_eq!(serde_json::to_string_pretty(&restored).unwrap(), bytes);
    assert_eq!(serde_json::to_string_pretty(&read(v2())).unwrap(), V2_BYTES);
}

#[test]
fn nested_control_identity_and_attempt_extensions_are_not_silently_discarded() {
    let mut with_attempt = v3();
    with_attempt["a_control"][0]["attempt"] = json!({"nonce":"attempt-a", "provider":"codex",
        "target":{"host":"herdr","owner":"logical-a","root":"/fixture","execution_nonce":"exec-a"},
        "source":{"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null}});
    assert!(compat::inspect(&with_attempt.to_string()).is_ok());
    for pointer in [
        "/a_control/0/identity",
        "/a_control/0/identity/sources/0",
        "/a_control/0/attempt",
        "/a_control/0/attempt/target",
        "/a_control/0/attempt/source",
    ] {
        let mut extended = with_attempt.clone();
        extended.pointer_mut(pointer).unwrap()["future_required_identity"] = json!(true);
        assert!(compat::inspect(&extended.to_string()).is_err(), "{pointer}");
    }
}

#[test]
fn marker_history_references_match_exact_identity_receipt_and_attempt() {
    let mut snapshot = read(v3());
    let queued = AdmissionBind {
        receipt: "ticket-a".into(),
        at: BindAt::Queued {
            index: 0,
            item: "digest".into(),
        },
        context: None,
    };
    assert!(snapshot.control_references_bind(&queued));
    let different = AdmissionBind {
        receipt: "ticket-b".into(),
        ..queued.clone()
    };
    assert!(!snapshot.control_references_bind(&different));
    let attempt: AttemptIdentity = serde_json::from_value(json!({"nonce":"attempt-a",
        "provider":"codex","target":{"host":"herdr","owner":"logical-a",
            "root":"/fixture","execution_nonce":"execution-a"},
        "source":{"kind":"pre_source","expected_session_id":null,
            "expected_transcript":null,"binding_watermark":null}}))
    .unwrap();
    snapshot.a_control[0].front_receipt = None;
    snapshot.a_control[0].attempt = Some(attempt.clone());
    let bound = AdmissionBind {
        receipt: "ticket-other".into(),
        at: BindAt::Attempt {
            identity: snapshot.a_control[0].identity.clone(),
            attempt: attempt.clone(),
        },
        context: None,
    };
    assert!(snapshot.control_references_bind(&bound));
    let mut other_attempt = bound.clone();
    if let BindAt::Attempt { attempt, .. } = &mut other_attempt.at {
        attempt.target.execution_nonce = "other".into();
    }
    assert!(!snapshot.control_references_bind(&other_attempt));
    let settled = AdmissionBind {
        receipt: "ticket-other".into(),
        at: BindAt::Settled {
            attempt: attempt.nonce,
            source: None,
            settled_at_ms: 1,
        },
        context: None,
    };
    assert!(snapshot.control_references_bind(&settled));
    snapshot.a_control.clear();
    assert!(!snapshot.holds_input());
    assert!(!snapshot.control_references_bind(&settled));
}

#[test]
fn both_legacy_mutation_guards_preserve_a_readable_v3_file() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("42.json");
    let raw = serde_json::to_vec_pretty(&v3()).unwrap();
    std::fs::write(&path, &raw).unwrap();
    assert!(compat::require_clearable(&path).is_err());
    assert!(compat::require_legacy_write(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), raw);
}

#[test]
fn control_pins_only_its_exact_terminal_until_ack_and_keeps_the_existing_ttl_boundary() {
    use crate::services::turn_orchestrator::queued_head::v2_types::TerminalIdentity;
    let mut snapshot = read(v3());
    let pinned = snapshot.a_control[0].identity.clone();
    let mut unrelated = pinned.clone();
    unrelated.primary += 1;
    let mut recent = pinned.clone();
    recent.primary += 2;
    snapshot.terminal_identities = vec![
        TerminalIdentity {
            identity: pinned.clone(),
            settled_at_ms: 1,
        },
        TerminalIdentity {
            identity: unrelated.clone(),
            settled_at_ms: 1,
        },
        TerminalIdentity {
            identity: recent.clone(),
            settled_at_ms: 2,
        },
    ];
    let now = 86_400_001;
    snapshot.expire_terminals(now);
    assert_eq!(
        snapshot
            .terminal_identities
            .iter()
            .map(|t| &t.identity)
            .collect::<Vec<_>>(),
        [&pinned, &recent]
    );
    snapshot.a_control.clear();
    snapshot.expire_terminals(now);
    assert_eq!(snapshot.terminal_identities.len(), 1);
    assert_eq!(snapshot.terminal_identities[0].identity, recent);
}
