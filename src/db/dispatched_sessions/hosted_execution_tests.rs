use serde_json::{Value, json};

use super::*;

pub(crate) const TOKEN: &str = "discord_0123456789abcdef";

pub(crate) fn owner(channel_id: &str) -> HostedOwner {
    HostedOwner {
        provider: "claude".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: channel_id.into(),
        logical_key: "AgentDesk-claude-hosted".into(),
        owner_node: "test-node".into(),
        runtime_root: "/adk/runtime".into(),
    }
}

pub(crate) fn pending(owner: &HostedOwner, nonce: &str) -> HostedExecution {
    let source_ref = SourceRef {
        runtime_root: owner.runtime_root.clone(),
        channel: owner.channel_id.clone(),
        provider: owner.provider.clone(),
        logical_key: owner.logical_key.clone(),
        execution_nonce: nonce.into(),
        initial_source: None,
        baseline_event_seq: None,
    };
    HostedExecution::pending(owner.clone(), nonce.into(), source_ref)
}

pub(crate) fn location(pane_id: &str) -> HostedLocation {
    HostedLocation {
        host: "herdr".into(),
        execution_node: "test-node".into(),
        endpoint_config_key: "herdr.default".into(),
        socket_addr: "/adk/herdr.sock".into(),
        named_session: "agentdesk".into(),
        pane_id: pane_id.into(),
    }
}

pub(crate) fn expected(nonce: &str, root_pid: u32) -> ExpectedExecution {
    ExpectedExecution {
        binding_provider: "claude".into(),
        binding_nonce: nonce.into(),
        root: ProcessStamp {
            pid: root_pid,
            start: "1700000000".into(),
        },
        provider_process: ProcessStamp {
            pid: root_pid + 1,
            start: "1700000001".into(),
        },
        provenance: "launch".into(),
    }
}

pub(crate) fn record(owner: &HostedOwner, nonce: &str, state: HostedState) -> HostedExecution {
    let mut record = pending(owner, nonce);
    if state != HostedState::Pending {
        record.location = Some(location("pane-1"));
        record.expected = Some(expected(nonce, 100));
    }
    record.state = state;
    record
}

pub(crate) fn wire(record: &HostedExecution) -> Value {
    serde_json::to_value(record).unwrap()
}

pub(crate) fn future_schema(owner: &HostedOwner) -> Value {
    let mut raw = wire(&record(owner, "n-future", HostedState::Retired));
    raw["schema"] = json!(2);
    raw
}

#[test]
fn hosted_execution_decode_keeps_unreadable_payloads_unknown() {
    let owner = owner("100");
    assert_eq!(HostedRecord::decode(None), HostedRecord::Legacy);
    assert!(HostedRecord::decode(None).deletable());
    for state in [
        HostedState::Pending,
        HostedState::Bound,
        HostedState::Retired,
    ] {
        let known = record(&owner, "n1", state);
        let decoded = HostedRecord::decode(Some(&wire(&known)));
        assert_eq!(decoded, HostedRecord::Known(known));
        assert_eq!(
            decoded.deletable(),
            state == HostedState::Retired,
            "{state:?}"
        );
    }

    let bound = wire(&record(&owner, "n1", HostedState::Bound));
    let edit = |change: &dyn Fn(&mut Value)| {
        let mut raw = bound.clone();
        change(&mut raw);
        raw
    };
    let retired_with = |change: &dyn Fn(&mut Value)| {
        let mut raw = edit(change);
        raw["state"] = json!("retired");
        raw
    };
    let cases = [
        ("future schema", future_schema(&owner)),
        (
            "lost location key",
            edit(&|raw| drop(raw.as_object_mut().unwrap().remove("location"))),
        ),
        (
            "lost nested key",
            retired_with(&|raw| {
                drop(
                    raw["source_ref"]
                        .as_object_mut()
                        .unwrap()
                        .remove("baseline_event_seq"),
                )
            }),
        ),
        ("extra field", retired_with(&|raw| raw["lease"] = json!(1))),
        (
            "bound without location",
            edit(&|raw| raw["location"] = Value::Null),
        ),
        (
            "source nonce disagrees",
            retired_with(&|raw| raw["source_ref"]["execution_nonce"] = json!("n0")),
        ),
        (
            "evidence nonce disagrees",
            retired_with(&|raw| raw["expected"]["binding_nonce"] = json!("n0")),
        ),
        (
            "non-herdr location",
            retired_with(&|raw| raw["location"]["host"] = json!("tmux")),
        ),
        (
            "blank pane",
            retired_with(&|raw| raw["location"]["pane_id"] = json!(" ")),
        ),
        (
            "unknown state",
            edit(&|raw| raw["state"] = json!("adopted")),
        ),
        ("json null", Value::Null),
        ("array", json!([1, "bound"])),
        ("empty object", json!({})),
    ];
    for (label, raw) in cases {
        let decoded = HostedRecord::decode(Some(&raw));
        assert_eq!(decoded, HostedRecord::Unknown(raw), "{label}");
        assert!(!decoded.deletable(), "{label} must keep its row");
    }
}
