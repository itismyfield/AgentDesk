//! Hosted execution rows the upper-crate tests build and compare against.

use serde_json::{Value, json};

use super::*;

pub const TOKEN: &str = "discord_0123456789abcdef";

pub fn owner(channel_id: &str) -> HostedOwner {
    HostedOwner {
        provider: "claude".into(),
        discord_token_hash: TOKEN.into(),
        channel_id: channel_id.into(),
        logical_key: "AgentDesk-claude-hosted".into(),
        owner_node: "test-node".into(),
        runtime_root: "/adk/runtime".into(),
    }
}

pub fn pending(owner: &HostedOwner, nonce: &str) -> HostedExecution {
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

pub fn location(pane_id: &str) -> HostedLocation {
    HostedLocation {
        host: "herdr".into(),
        execution_node: "test-node".into(),
        endpoint_config_key: "herdr.default".into(),
        socket_addr: "/adk/herdr.sock".into(),
        named_session: "agentdesk".into(),
        pane_id: pane_id.into(),
    }
}

pub fn expected(nonce: &str, root_pid: u32) -> ExpectedExecution {
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

pub fn record(owner: &HostedOwner, nonce: &str, state: HostedState) -> HostedExecution {
    let mut record = pending(owner, nonce);
    if state != HostedState::Pending {
        record.location = Some(location("pane-1"));
        record.expected = Some(expected(nonce, 100));
    }
    record.state = state;
    record
}

pub fn wire(record: &HostedExecution) -> Value {
    serde_json::to_value(record).unwrap()
}

pub fn future_schema(owner: &HostedOwner) -> Value {
    let mut raw = wire(&record(owner, "n-future", HostedState::Retired));
    raw["schema"] = json!(2);
    raw
}

impl HostedObservation {
    pub fn known_fixture(record: HostedExecution) -> Self {
        let raw = Some(serde_json::to_value(&record).expect("fixture serializes"));
        Self {
            session_id: 0,
            raw,
            record: HostedRecord::Known(record),
        }
    }
}
