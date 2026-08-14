use super::{
    Deserialize, Deserializer, RelayOwnerKind, RuntimeHandoffKind, Serializer, TaskNotificationKind,
};

/// #2235: tolerant deserializer for `runtime_kind`. A newer binary may write
/// a `RuntimeHandoffKind` variant this binary does not know about; serde's
/// default `deny_unknown_variants` posture would propagate a parse error and
/// `load_inflight_states_from_root` would delete the entire row as malformed
/// (`inflight_malformed_json_graceful_skip`). Instead we map unknown strings
/// to `None`. The recovery engine consults this `None` together with the
/// row-shape heuristic to decide whether to silent-skip recovery (issue
/// #2235 DoD #3) instead of guessing a runtime and surfacing a misleading
/// "input fifo path missing" notice.
pub(super) fn deserialize_runtime_kind_tolerant<'de, D>(
    deserializer: D,
) -> Result<Option<RuntimeHandoffKind>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(raw.as_deref().and_then(|value| match value {
        "legacy_tmux_wrapper" => Some(RuntimeHandoffKind::LegacyTmuxWrapper),
        "claude_tui" => Some(RuntimeHandoffKind::ClaudeTui),
        "codex_tui" => Some(RuntimeHandoffKind::CodexTui),
        "process_backend" => Some(RuntimeHandoffKind::ProcessBackend),
        "claude_e_adapter" => Some(RuntimeHandoffKind::ClaudeEAdapter),
        _ => None,
    }))
}

/// #2376: tolerant deserializer for `relay_owner_kind`. Older binaries must
/// not delete an otherwise valid inflight row just because a newer binary
/// wrote a relay-owner variant they do not understand.
pub(super) fn deserialize_relay_owner_kind_tolerant<'de, D>(
    deserializer: D,
) -> Result<RelayOwnerKind, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    Ok(match raw.as_deref() {
        Some("watcher") => RelayOwnerKind::Watcher,
        Some("standby_relay") => RelayOwnerKind::StandbyRelay,
        Some("session_bound_relay") => RelayOwnerKind::SessionBoundRelay,
        Some("none") | None => RelayOwnerKind::None,
        _ => RelayOwnerKind::Unknown,
    })
}

pub(super) fn serialize_task_notification_kind<S>(
    value: &Option<TaskNotificationKind>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(kind) => serializer.serialize_some(kind.as_str()),
        None => serializer.serialize_none(),
    }
}

pub(super) fn deserialize_task_notification_kind<'de, D>(
    deserializer: D,
) -> Result<Option<TaskNotificationKind>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.as_deref().and_then(TaskNotificationKind::from_str))
}
