use super::{Deserialize, InflightTurnState, Serialize};

/// Origin of a turn whose state is captured in [`InflightTurnState`]. Pure
/// audit metadata for #2285 / #2161 — callers must not branch RELAY routing on
/// this value; the session-bound relay (epic #2285 E1–E5) treats every matched
/// session uniformly.
///
/// EXCEPTION (#3969, behavioral dependency — do not silently regress): the
/// watcher's completion-footer suppression for #3089 footer chrome DOES key on
/// `turn_source == Managed`. The #3089 footer is kept only for Discord-origin
/// (`Managed`) turns; every non-`Managed` mirror origin (e.g. `/loop`
/// self-paced / monitor / external-input TUI mirrors) suppresses the footer. So
/// the `Managed` discriminant is now load-bearing for that footer decision —
/// preserve this carve-out when changing how `turn_source` is assigned.
///
/// EXCEPTION (#4455): Codex manual rebind uses the origin only to choose the
/// conservative timestamp ordering for prompt evidence. Managed/monitor rows
/// are born before prompt injection; external-input/adopted rows are observed
/// after the rollout prompt exists. Missing evidence never changes ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum TurnSource {
    /// AgentDesk-launched tmux session via the normal Discord intake path.
    /// This is the historical default for every legacy row.
    #[default]
    Managed,
    /// Triggered by a Monitor pattern auto-turn synthesised on top of an
    /// existing managed session (`TaskNotificationKind::MonitorAutoTurn`).
    MonitorTriggered,
    /// User typed directly into the tmux pane (SSH / local tty) while the
    /// pane was bound to a Discord channel. Detected by the watcher when
    /// rollout activity advances without a Discord-origin inflight in
    /// place.
    ExternalInput,
    /// AgentDesk discovered a session created externally (e.g. operator ran
    /// `tmux new -s <expected>` and started a provider) and adopted it via
    /// `SessionDiscovery` + `SessionRegistry` (epic #2285 E2). Distinct
    /// from `ExternalInput` (which keeps an existing Discord-bound session
    /// running) — `ExternalAdopted` is the *first* time AgentDesk sees the
    /// session.
    ExternalAdopted,
}

/// Active relay owner persisted with an in-flight turn.
///
/// `None` preserves the historical bridge-owned/default shape. `Watcher` is
/// equivalent to legacy `watcher_owns_live_relay = true`. `StandbyRelay`
/// captures the cluster-standby JSONL relay: it does not own a tmux watcher
/// slot, but it does own live Discord delivery while it is running. `Unknown`
/// is a conservative forward-compat fallback for future live-owner variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(in crate::services::discord) enum RelayOwnerKind {
    #[default]
    None,
    Watcher,
    StandbyRelay,
    SessionBoundRelay,
    Unknown,
}

impl RelayOwnerKind {
    pub(in crate::services::discord) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Watcher => "watcher",
            Self::StandbyRelay => "standby_relay",
            Self::SessionBoundRelay => "session_bound_relay",
            Self::Unknown => "unknown",
        }
    }
}

impl TurnSource {
    /// Stable wire representation for completion audit logs and metrics labels.
    pub(in crate::services::discord) fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::MonitorTriggered => "monitor_triggered",
            Self::ExternalInput => "external_input",
            Self::ExternalAdopted => "external_adopted",
        }
    }
}
