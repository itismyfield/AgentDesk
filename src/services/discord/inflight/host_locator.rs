//! Optional inflight host locator. An unknown or truncated locator keeps the row and
//! becomes `Unknown` with its original JSON, so a re-save never downgrades it to tmux.
#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::services::session_host::{HostKind, HostedRuntimeLocator};

#[derive(Debug, Clone, PartialEq)]
pub(in crate::services::discord) enum PersistedHostLocator {
    Known(HostedRuntimeLocator),
    /// Unknown host, missing/mistyped/blank field or an unrecognized key; the
    /// value is kept verbatim and must never admit a tmux-only path.
    Unknown(serde_json::Value),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocatorWire {
    host_kind: String,
    host_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_node: Option<String>,
}

fn non_blank(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(|text| !text.trim().is_empty())
}

impl PersistedHostLocator {
    fn from_raw(raw: serde_json::Value) -> Self {
        // serde reads a positional array into a struct; only an object can be a locator.
        if !raw.is_object() {
            return Self::Unknown(raw);
        }
        let known = serde_json::from_value::<LocatorWire>(raw.clone())
            .ok()
            .and_then(|wire| {
                let host_kind = HostKind::from_persisted(&wire.host_kind)?;
                let complete = !wire.host_session_id.trim().is_empty()
                    && non_blank(&wire.pane)
                    && non_blank(&wire.execution_node)
                    && (host_kind != HostKind::Herdr || wire.pane.is_some());
                complete.then_some(HostedRuntimeLocator {
                    execution_node: wire.execution_node,
                    host_kind,
                    host_session_id: wire.host_session_id,
                    pane: wire.pane,
                })
            });
        known.map_or(Self::Unknown(raw), Self::Known)
    }
}

impl Serialize for PersistedHostLocator {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(locator) => LocatorWire {
                host_kind: locator.host_kind.as_str().to_string(),
                host_session_id: locator.host_session_id.clone(),
                pane: locator.pane.clone(),
                execution_node: locator.execution_node.clone(),
            }
            .serialize(serializer),
            Self::Unknown(raw) => raw.serialize(serializer),
        }
    }
}

/// Field hook: a present key (even `null`) is a locator, so only an absent key is `None`.
pub(in crate::services::discord) fn deserialize_present<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<PersistedHostLocator>, D::Error> {
    PersistedHostLocator::deserialize(deserializer).map(Some)
}

impl<'de> Deserialize<'de> for PersistedHostLocator {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde_json::Value::deserialize(deserializer).map(Self::from_raw)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::super::save_store::save_inflight_state_in_root;
    use super::super::{InflightTurnState, inflight_state_path, load_inflight_states_from_root};
    use super::*;
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;

    const CHANNEL: u64 = 5340;

    fn row(runtime_kind: Option<RuntimeHandoffKind>) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            CHANNEL,
            Some("adk-codex".to_string()),
            222,
            333,
            444,
            "hello".to_string(),
            None,
            Some("AgentDesk-codex-adk-codex".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        state.runtime_kind = runtime_kind;
        state
    }

    fn load_one(root: &std::path::Path) -> InflightTurnState {
        let mut loaded = load_inflight_states_from_root(root, &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1, "the row must stay visible to the loader");
        loaded.remove(0)
    }

    fn on_disk_locator(root: &std::path::Path) -> Option<Value> {
        let path = inflight_state_path(root, &ProviderKind::Codex, CHANNEL);
        let text = std::fs::read_to_string(path).unwrap();
        serde_json::from_str::<Value>(&text)
            .unwrap()
            .get("host_locator")
            .cloned()
    }

    #[test]
    fn rows_without_a_locator_keep_their_exact_bytes_for_every_runtime_kind() {
        let kinds = [
            RuntimeHandoffKind::LegacyTmuxWrapper,
            RuntimeHandoffKind::ClaudeTui,
            RuntimeHandoffKind::CodexTui,
            RuntimeHandoffKind::ProcessBackend,
            RuntimeHandoffKind::ClaudeEAdapter,
        ];
        let literals = [
            "legacy_tmux_wrapper",
            "claude_tui",
            "codex_tui",
            "process_backend",
            "claude_e_adapter",
        ];
        for (kind, literal) in kinds
            .into_iter()
            .map(Some)
            .zip(literals.map(Some))
            .chain([(None, None)])
        {
            let written = serde_json::to_string_pretty(&row(kind)).unwrap();
            if let Some(literal) = literal {
                assert!(written.contains(&format!("\"runtime_kind\": \"{literal}\"")));
            }
            assert!(
                !written.contains("host_locator"),
                "{kind:?}: a row without a locator must not gain the key"
            );
            let restored: InflightTurnState = serde_json::from_str(&written).unwrap();
            assert_eq!(restored.host_locator, None, "{kind:?}");
            assert_eq!(
                serde_json::to_string_pretty(&restored).unwrap(),
                written,
                "{kind:?}: an old row must re-serialize byte-identically"
            );
        }
    }

    #[test]
    fn known_locator_survives_save_and_load() {
        for locator in [
            HostedRuntimeLocator {
                execution_node: Some("mac-mini".to_string()),
                host_kind: HostKind::Herdr,
                host_session_id: "herdr-session-1".to_string(),
                pane: Some("pane-7".to_string()),
            },
            HostedRuntimeLocator {
                execution_node: None,
                host_kind: HostKind::Tmux,
                host_session_id: "AgentDesk-codex-adk-codex".to_string(),
                pane: None,
            },
        ] {
            let temp = TempDir::new().unwrap();
            let mut state = row(Some(RuntimeHandoffKind::CodexTui));
            state.host_locator = Some(PersistedHostLocator::Known(locator.clone()));
            save_inflight_state_in_root(temp.path(), &state).unwrap();

            let loaded = load_one(temp.path());
            assert_eq!(
                loaded.host_locator,
                Some(PersistedHostLocator::Known(locator))
            );
            assert_eq!(loaded.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
        }
    }

    #[test]
    fn unknown_or_truncated_locator_keeps_the_row_and_its_raw_json_across_resave() {
        let cases = [
            (
                "unknown host",
                json!({"host_kind": "zellij", "host_session_id": "z-1"}),
            ),
            (
                "host case",
                json!({"host_kind": "Herdr", "host_session_id": "h-1", "pane": "p"}),
            ),
            (
                "missing session id",
                json!({"host_kind": "herdr", "pane": "p"}),
            ),
            (
                "missing host kind",
                json!({"host_session_id": "h-1", "pane": "p"}),
            ),
            (
                "session id type",
                json!({"host_kind": "tmux", "host_session_id": 7}),
            ),
            (
                "host kind type",
                json!({"host_kind": 1, "host_session_id": "t-1"}),
            ),
            (
                "pane type",
                json!({"host_kind": "herdr", "host_session_id": "h-1", "pane": 3}),
            ),
            (
                "herdr without pane",
                json!({"host_kind": "herdr", "host_session_id": "h-1"}),
            ),
            (
                "blank session id",
                json!({"host_kind": "tmux", "host_session_id": " "}),
            ),
            (
                "blank pane",
                json!({"host_kind": "herdr", "host_session_id": "h-1", "pane": ""}),
            ),
            (
                "newer key",
                json!({"host_kind": "tmux", "host_session_id": "t-1", "lease": 9}),
            ),
            ("not an object", json!("herdr")),
            ("array", json!(["herdr", "h-1"])),
            ("tmux array", json!(["tmux", "t-1", null, null])),
            ("process array", json!(["process", "p-1", null, null])),
            ("herdr array", json!(["herdr", "h-1", "pane-1", null])),
            ("explicit null", Value::Null),
        ];
        for (label, raw) in cases {
            let temp = TempDir::new().unwrap();
            let path = inflight_state_path(temp.path(), &ProviderKind::Codex, CHANNEL);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut on_disk =
                serde_json::to_value(row(Some(RuntimeHandoffKind::CodexTui))).unwrap();
            on_disk["host_locator"] = raw.clone();
            std::fs::write(&path, serde_json::to_string_pretty(&on_disk).unwrap()).unwrap();

            let loaded = load_one(temp.path());
            assert_eq!(
                loaded.host_locator,
                Some(PersistedHostLocator::Unknown(raw.clone())),
                "{label}: an unreadable locator must stay Unknown, never tmux"
            );
            assert_eq!(
                loaded.runtime_kind,
                Some(RuntimeHandoffKind::CodexTui),
                "{label}"
            );
            assert_eq!(loaded.user_msg_id, 333, "{label}");

            save_inflight_state_in_root(temp.path(), &loaded).unwrap();
            assert_eq!(
                on_disk_locator(temp.path()),
                Some(raw.clone()),
                "{label}: a re-save must write the original locator back"
            );
            assert_eq!(
                load_one(temp.path()).host_locator,
                Some(PersistedHostLocator::Unknown(raw)),
                "{label}: the second restore must still be Unknown"
            );
        }
    }
}
