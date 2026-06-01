//! Inflight turn state persistence.
//!
//! `response_sent_offset`, `current_msg_id`, and
//! `last_watcher_relayed_offset` participate in the relay state contract
//! documented in `docs/relay-state-contract.md` (#1222 / #1224).
//! Any change that touches relay producers/consumers must keep the
//! invariants enumerated there satisfied.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::TimeZone;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::InflightRestartMode;
use super::runtime_store::{atomic_write, discord_inflight_root};
use crate::dispatch::Source;
use crate::services::agent_protocol::{RuntimeHandoffKind, TaskNotificationKind};
use crate::services::provider::ProviderKind;

// #2235 (follow-up to #2213): bump v7→v8. v7 added `runtime_kind` without a
// version change, so rolling back from new→old binaries could read rows whose
// FIFO synthesis was elided for ClaudeTui and reject recovery with a misleading
// "input fifo path missing" notice. v8 marks the on-disk shape that ships the
// compat-fixed `input_fifo_path` alongside ClaudeTui plus the silent-skip
// recovery branch; old binaries continue to deserialize v8 rows via
// `#[serde(default)]` and treat the new `runtime_kind` as legacy, so the
// compat window is one release in each direction.
const INFLIGHT_STATE_VERSION: u32 = 8;
const INFLIGHT_MAX_AGE_SECS: u64 = 300; // 5 minutes
const DRAIN_RESTART_MAX_AGE_SECS: u64 = 1800; // 30 minutes
const HOT_SWAP_HANDOFF_MAX_AGE_SECS: u64 = 900; // 15 minutes

/// #1446 stall-deadlock recovery: an inflight state is treated as "stale"
/// (i.e. the dispatch that wrote it almost certainly already terminated
/// without cleanup) when its persisted `updated_at` has not advanced for
/// this many seconds. THREAD-GUARD uses this exact threshold; the
/// stall-watchdog uses `2x` to stay strictly more conservative than any
/// caller that has already observed the state directly.
///
/// `updated_at` is rewritten on every `save_inflight_state` call but is
/// **not** a true heartbeat — a healthy foreground model/tool call can
/// legitimately go silent for multiple minutes (long Bash, slow LLM
/// stream, large Read).
///
/// History: this constant used to be aligned with
/// `placeholder_sweeper::ABANDON_THRESHOLD_SECS` (then 300s) so the
/// "definitely stale" gate fired exactly when the sweeper had already
/// replaced the placeholder with its terminal "abandoned" form. After
/// #2427 (#2436 / #2437 / #2438) the explicit-signal wires (pane death,
/// heartbeat-gap inflight sweeper, generation-mismatch bulk invalidate,
/// TurnCompleted idempotent guard) make the sweeper a pure safety net
/// — its abandon timer was relaxed to 1800s (30 min). The 300s figure
/// here is retained because it gates **new** user-message dispatch
/// (THREAD-GUARD) and the stall-watchdog (#1446): both want to recover
/// quickly once an explicit signal failed to fire, and the explicit
/// wires above are expected to clear the cleanup hit within seconds.
/// False-positive cleanup of a live turn is still much worse than
/// slightly delayed recovery (issue #1446).
pub(super) const INFLIGHT_STALENESS_THRESHOLD_SECS: u64 = 300;

/// Build an optional `serenity::MessageId` from a possibly-zero raw inflight id.
///
/// `current_msg_id == 0` is a LEGITIMATE state: a TUI-direct / recovery turn
/// (`runtime_kind = claude_tui`, `status_message_id = None`) that never anchored
/// a Discord placeholder message. `serenity::MessageId::new(0)` PANICS
/// ("Attempted to call MessageId::new with invalid (0) value"), so every
/// recovery/relay path that derives a placeholder id from a possibly-zero
/// inflight field must funnel through this helper and treat `None` as
/// "no anchored placeholder" — skipping the placeholder-specific step while
/// still performing watcher/session recovery — rather than panicking.
pub(in crate::services::discord) fn optional_message_id(
    raw: u64,
) -> Option<poise::serenity_prelude::MessageId> {
    if raw == 0 {
        None
    } else {
        Some(poise::serenity_prelude::MessageId::new(raw))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct InflightTurnState {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub channel_name: Option<String>,
    #[serde(default)]
    pub logical_channel_id: Option<u64>,
    #[serde(default)]
    pub thread_id: Option<u64>,
    #[serde(default)]
    pub thread_title: Option<String>,
    pub request_owner_user_id: u64,
    pub user_msg_id: u64,
    /// Discord message id for the live status panel when status-panel-v2 is
    /// enabled. `current_msg_id` remains the assistant response message.
    #[serde(default)]
    pub status_message_id: Option<u64>,
    pub current_msg_id: u64,
    pub current_msg_len: usize,
    pub user_text: String,
    /// Origin of the user turn. Voice turns keep normal text mirroring but also
    /// drive spoken result playback and voice progress subscribers.
    #[serde(default)]
    pub source: Source,
    pub session_id: Option<String>,
    pub tmux_session_name: Option<String>,
    pub output_path: Option<String>,
    pub input_fifo_path: Option<String>,
    /// #2235: deserializing through `deserialize_runtime_kind_tolerant` so a
    /// future variant written by a newer binary collapses to `None` instead
    /// of failing the whole row's parse (which would otherwise lose the
    /// inflight to `inflight_malformed_json_graceful_skip`). Combined with
    /// the silent-skip recovery branch this gives one release of forward
    /// compat for new runtime kinds.
    #[serde(default, deserialize_with = "deserialize_runtime_kind_tolerant")]
    pub runtime_kind: Option<RuntimeHandoffKind>,
    /// #2235: transient sidecar populated by `load_inflight_states_from_root`
    /// when the on-disk JSON had a `runtime_kind` field whose value was a
    /// non-empty string this binary did not recognize (i.e. a future variant).
    /// Distinct from `runtime_kind = None` for "field absent" (legacy v7
    /// rows). Recovery uses this to silent-skip present-but-unknown rows
    /// regardless of `version`, while still recovering legacy absent-field
    /// rows via the normal heuristics. `#[serde(skip)]` keeps the flag
    /// out of the on-disk shape — it is purely an in-memory annotation.
    #[serde(skip)]
    pub runtime_kind_unknown_on_disk: bool,
    #[serde(default)]
    pub worktree_path: Option<String>,
    #[serde(default)]
    pub worktree_branch: Option<String>,
    #[serde(default)]
    pub base_commit: Option<String>,
    pub last_offset: u64,
    /// Stable start offset for the current turn's output JSONL slice.
    #[serde(default)]
    pub turn_start_offset: Option<u64>,
    pub full_response: String,
    pub response_sent_offset: usize,
    /// True once the terminal assistant response has been committed to the
    /// outbound Discord delivery path. Completion/status UI may still be
    /// suppressed by a TUI quiescence timeout, but recovery must not treat
    /// this row as an active provider turn after this point.
    #[serde(default)]
    pub terminal_delivery_committed: bool,
    #[serde(default)]
    pub current_tool_line: Option<String>,
    #[serde(default)]
    pub last_tool_name: Option<String>,
    #[serde(default)]
    pub last_tool_summary: Option<String>,
    #[serde(default)]
    pub prev_tool_status: Option<String>,
    #[serde(
        default,
        serialize_with = "serialize_task_notification_kind",
        deserialize_with = "deserialize_task_notification_kind"
    )]
    pub task_notification_kind: Option<TaskNotificationKind>,
    pub started_at: String,
    pub updated_at: String,
    /// Restart generation at which this turn was born.
    #[serde(default)]
    pub born_generation: u64,
    /// Whether any tool_use was seen during this turn (persisted for restart recovery).
    #[serde(default)]
    pub any_tool_used: bool,
    /// Whether text was streamed after the last tool_use (persisted for restart recovery).
    #[serde(default)]
    pub has_post_tool_text: bool,
    /// ADK session key (hostname:session-name) for long-turn diagnostics.
    #[serde(default)]
    pub session_key: Option<String>,
    /// Preferred Discord bot key for terminal headless delivery.
    #[serde(default)]
    pub delivery_bot: Option<String>,
    /// Suppress assistant-text relay to the Discord channel for the turn.
    /// Set from `metadata.silent` on a headless trigger (e.g. family-profile-probe);
    /// lifecycle/error/cancel notifications still post.
    #[serde(default)]
    pub silent_turn: bool,
    /// Active dispatch ID for long-turn diagnostics.
    #[serde(default)]
    pub dispatch_id: Option<String>,
    /// Last tmux output offset from which a watcher relayed a response.
    /// Persisted so that replacement watcher instances can skip already-delivered output.
    #[serde(default)]
    pub last_watcher_relayed_offset: Option<u64>,
    /// `.generation` marker file mtime (nanos since epoch) snapshotted at
    /// the same moment as `last_watcher_relayed_offset`. Persisted so that
    /// a replacement watcher (post dcserver restart) can tell whether a
    /// shorter-than-restored-offset jsonl is the same wrapper after a
    /// `truncate_jsonl_head_safe` rotation (mtime unchanged → pin to
    /// EOF) or a fresh wrapper after cancel→respawn (mtime changed →
    /// reset to 0). See `tmux::watermark_after_output_regression`
    /// (#1270). `None` for offsets persisted before this field existed.
    #[serde(default)]
    pub last_watcher_relayed_generation_mtime_ns: Option<i64>,
    /// Lifecycle-aware restart/handoff mode for recovery semantics.
    #[serde(default)]
    pub restart_mode: Option<InflightRestartMode>,
    /// Generation that owns the planned restart/handoff lifecycle.
    #[serde(default)]
    pub restart_generation: Option<u64>,
    /// #897 counter-model re-review — `true` when this inflight was
    /// synthesised by `POST /api/inflight/rebind` to adopt a live tmux
    /// session that had no real user-authored turn driving it (zero-valued
    /// `user_msg_id` / `current_msg_id` / `request_owner_user_id`).
    ///
    /// Callers that route or persist based on "is there a live foreground
    /// turn" must treat a rebind-origin inflight as **absent**. This state
    /// exists only to surface a live adopted tmux session through inflight /
    /// monitoring APIs; it does not represent a user-authored Discord turn.
    /// Reactions / transcript writes that key off `user_msg_id` should also
    /// skip work when this flag is set, because the placeholder IDs do not
    /// identify a real Discord message.
    #[serde(default)]
    pub rebind_origin: bool,
    /// #1255 codex round-2 P2: `true` while a long-running tool placeholder
    /// (`Monitor` / background `Bash`/`Task`/`Agent`) owns `current_msg_id`.
    /// `placeholder_sweeper` skips inflights whose `full_response` is non-empty
    /// to avoid clobbering partially delivered text — but the placeholder
    /// branch may have been opened *after* assistant prose, so the sweeper
    /// would otherwise miss live cards that crash mid-flight. Set/cleared by
    /// the turn loop alongside `long_running_placeholder_active`.
    #[serde(default)]
    pub long_running_placeholder_active: bool,
    /// #1222 single-relay-owner: true when a live tmux watcher, not
    /// `turn_bridge`, owns assistant content delivery for this in-flight turn.
    /// Lifecycle/banner paths may still update metadata, but bridge must not
    /// stream or terminal-replace assistant text while this is true.
    #[serde(default)]
    pub watcher_owns_live_relay: bool,
    /// #2376: typed replacement for `watcher_owns_live_relay`.
    ///
    /// The legacy boolean can only distinguish "watcher" from "not watcher".
    /// A standby JSONL relay is not a watcher, but it is still a live relay
    /// owner that restored watchers must yield to. Keep the boolean for on-disk
    /// compatibility while new writers populate this typed field.
    #[serde(default, deserialize_with = "deserialize_relay_owner_kind_tolerant")]
    pub relay_owner_kind: RelayOwnerKind,
    /// #2285 audit trail — origin of the turn that produced this inflight.
    /// Recorded for diagnostics; the session-bound relay does NOT branch on
    /// this value (epic #2285 acceptance criterion E: relay is decided by
    /// `SessionMatcher` membership, not by turn source). Defaults to
    /// `Managed` for legacy rows that pre-date this field.
    #[serde(default)]
    pub turn_source: TurnSource,
}

/// Origin of a turn whose state is captured in [`InflightTurnState`]. Pure
/// audit metadata for #2285 / #2161 — callers must not branch relay or
/// completion semantics on this value; the session-bound relay (epic #2285
/// E1–E5) treats every matched session uniformly.
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
    /// Stable wire representation for audit logs / metrics labels.
    pub(in crate::services::discord) fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "managed",
            Self::MonitorTriggered => "monitor_triggered",
            Self::ExternalInput => "external_input",
            Self::ExternalAdopted => "external_adopted",
        }
    }
}

#[cfg(test)]
mod turn_source_tests {
    use super::{InflightTurnState, RelayOwnerKind, TurnSource};
    use crate::services::provider::ProviderKind;

    #[test]
    fn default_is_managed_for_legacy_rows() {
        // #2285 audit field is backward compatible — legacy v8 inflight rows
        // that pre-date the field must round-trip through serde with
        // `TurnSource::Managed` filled in via `#[serde(default)]`.
        assert_eq!(TurnSource::default(), TurnSource::Managed);
    }

    #[test]
    fn wire_strings_are_stable_audit_labels() {
        // The four labels are committed to observability dashboards / metrics
        // — renaming them silently is a downstream-breaking change.
        assert_eq!(TurnSource::Managed.as_str(), "managed");
        assert_eq!(TurnSource::MonitorTriggered.as_str(), "monitor_triggered");
        assert_eq!(TurnSource::ExternalInput.as_str(), "external_input");
        assert_eq!(TurnSource::ExternalAdopted.as_str(), "external_adopted");
    }

    #[test]
    fn serde_round_trip_uses_snake_case() {
        // Confirms the `rename_all = "snake_case"` attribute survives any
        // future refactor that re-imports the enum elsewhere.
        let json = serde_json::to_string(&TurnSource::ExternalAdopted).unwrap();
        assert_eq!(json, "\"external_adopted\"");
        let parsed: TurnSource = serde_json::from_str("\"monitor_triggered\"").unwrap();
        assert_eq!(parsed, TurnSource::MonitorTriggered);
    }

    #[test]
    fn missing_field_defaults_to_managed_when_deserialised() {
        // The full state struct is gated behind `legacy-sqlite-tests`, so we
        // exercise the `#[serde(default)]` contract with a small wrapper
        // that captures the exact attribute combination used on the field.
        #[derive(serde::Deserialize, Debug)]
        struct Probe {
            #[serde(default)]
            turn_source: TurnSource,
        }
        let parsed: Probe = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.turn_source, TurnSource::Managed);
    }

    #[test]
    fn relay_owner_kind_defaults_to_none_for_legacy_rows() {
        #[derive(serde::Deserialize, Debug)]
        struct Probe {
            #[serde(default)]
            relay_owner_kind: RelayOwnerKind,
        }

        let parsed: Probe = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.relay_owner_kind, RelayOwnerKind::None);
    }

    #[test]
    fn relay_owner_kind_uses_legacy_bool_when_typed_field_absent() {
        let state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 8,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 8,
            "current_msg_id": 9,
            "current_msg_len": 0,
            "user_text": "hello",
            "source": "text",
            "session_id": null,
            "tmux_session_name": "AgentDesk-codex-adk-cdx",
            "output_path": "/tmp/out.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-05-17 10:00:00",
            "updated_at": "2026-05-17 10:00:00",
            "watcher_owns_live_relay": true
        }))
        .expect("legacy bool-only row should deserialize");

        assert_eq!(state.relay_owner_kind, RelayOwnerKind::None);
        assert_eq!(state.effective_relay_owner_kind(), RelayOwnerKind::Watcher);
    }

    #[test]
    fn relay_owner_kind_unknown_value_deserializes_as_unknown() {
        let state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 8,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 8,
            "current_msg_id": 9,
            "current_msg_len": 0,
            "user_text": "hello",
            "source": "text",
            "session_id": null,
            "tmux_session_name": "AgentDesk-codex-adk-cdx",
            "output_path": "/tmp/out.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-05-17 10:00:00",
            "updated_at": "2026-05-17 10:00:00",
            "watcher_owns_live_relay": false,
            "relay_owner_kind": "future_owner"
        }))
        .expect("future relay owner must not make the whole row malformed");

        assert_eq!(state.relay_owner_kind, RelayOwnerKind::Unknown);
        assert_eq!(state.effective_relay_owner_kind(), RelayOwnerKind::Unknown);
    }

    #[test]
    fn relay_owner_kind_session_bound_relay_round_trips() {
        let state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 8,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 8,
            "current_msg_id": 9,
            "current_msg_len": 0,
            "user_text": "hello",
            "source": "text",
            "session_id": null,
            "tmux_session_name": "AgentDesk-codex-adk-cdx",
            "output_path": "/tmp/out.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-05-17 10:00:00",
            "updated_at": "2026-05-17 10:00:00",
            "watcher_owns_live_relay": false,
            "relay_owner_kind": "session_bound_relay"
        }))
        .expect("session-bound relay owner should deserialize");

        assert_eq!(state.relay_owner_kind, RelayOwnerKind::SessionBoundRelay);
        assert_eq!(
            state.effective_relay_owner_kind(),
            RelayOwnerKind::SessionBoundRelay
        );
        assert_eq!(
            RelayOwnerKind::SessionBoundRelay.as_str(),
            "session_bound_relay"
        );
    }

    #[test]
    fn relay_owner_kind_typed_field_wins_over_legacy_bool() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            7,
            8,
            9,
            "hello".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        state.watcher_owns_live_relay = true;
        state.relay_owner_kind = RelayOwnerKind::StandbyRelay;

        assert_eq!(
            state.effective_relay_owner_kind(),
            RelayOwnerKind::StandbyRelay
        );
    }
}

impl InflightTurnState {
    pub fn new(
        provider: ProviderKind,
        channel_id: u64,
        channel_name: Option<String>,
        request_owner_user_id: u64,
        user_msg_id: u64,
        current_msg_id: u64,
        user_text: String,
        session_id: Option<String>,
        tmux_session_name: Option<String>,
        output_path: Option<String>,
        input_fifo_path: Option<String>,
        last_offset: u64,
    ) -> Self {
        let now = now_string();
        let runtime_kind = input_fifo_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .map(|_| RuntimeHandoffKind::LegacyTmuxWrapper);
        Self {
            version: INFLIGHT_STATE_VERSION,
            provider: provider.as_str().to_string(),
            channel_id,
            channel_name,
            logical_channel_id: Some(channel_id),
            thread_id: None,
            thread_title: None,
            request_owner_user_id,
            user_msg_id,
            status_message_id: None,
            current_msg_id,
            current_msg_len: 0,
            user_text,
            source: Source::Text,
            session_id,
            tmux_session_name,
            output_path,
            input_fifo_path,
            runtime_kind,
            runtime_kind_unknown_on_disk: false,
            worktree_path: None,
            worktree_branch: None,
            base_commit: None,
            last_offset,
            turn_start_offset: Some(last_offset),
            full_response: String::new(),
            response_sent_offset: 0,
            terminal_delivery_committed: false,
            current_tool_line: None,
            last_tool_name: None,
            last_tool_summary: None,
            prev_tool_status: None,
            task_notification_kind: None,
            started_at: now.clone(),
            updated_at: now,
            born_generation: super::runtime_store::load_generation(),
            any_tool_used: false,
            has_post_tool_text: false,
            session_key: None,
            delivery_bot: None,
            silent_turn: false,
            dispatch_id: None,
            last_watcher_relayed_offset: None,
            last_watcher_relayed_generation_mtime_ns: None,
            restart_mode: None,
            restart_generation: None,
            rebind_origin: false,
            long_running_placeholder_active: false,
            watcher_owns_live_relay: false,
            relay_owner_kind: RelayOwnerKind::None,
            turn_source: TurnSource::Managed,
        }
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }

    pub(in crate::services::discord) fn effective_relay_owner_kind(&self) -> RelayOwnerKind {
        match self.relay_owner_kind {
            RelayOwnerKind::None if self.watcher_owns_live_relay => RelayOwnerKind::Watcher,
            kind => kind,
        }
    }

    pub(in crate::services::discord) fn set_relay_owner_kind(&mut self, kind: RelayOwnerKind) {
        self.relay_owner_kind = kind;
        self.watcher_owns_live_relay = matches!(kind, RelayOwnerKind::Watcher);
    }

    pub(in crate::services::discord) fn terminal_delivery_completed(&self) -> bool {
        self.terminal_delivery_committed
    }

    pub fn set_restart_mode(&mut self, restart_mode: InflightRestartMode) {
        self.restart_mode = Some(restart_mode);
        self.restart_generation = Some(super::runtime_store::load_generation());
    }

    pub fn clear_restart_mode(&mut self) {
        self.restart_mode = None;
        self.restart_generation = None;
    }

    pub(in crate::services::discord) fn runtime_kind_for_recovery(&self) -> RuntimeHandoffKind {
        if let Some(kind) = self.runtime_kind {
            return kind;
        }
        if self
            .input_fifo_path
            .as_deref()
            .is_some_and(|path| !path.is_empty())
        {
            return RuntimeHandoffKind::LegacyTmuxWrapper;
        }
        if self.provider == ProviderKind::Claude.as_str()
            && self
                .tmux_session_name
                .as_deref()
                .is_some_and(|name| !name.is_empty())
            && self
                .output_path
                .as_deref()
                .is_some_and(|path| !path.is_empty())
        {
            return RuntimeHandoffKind::ClaudeTui;
        }
        RuntimeHandoffKind::ProcessBackend
    }

    pub fn set_worktree_context(
        &mut self,
        worktree_path: Option<String>,
        worktree_branch: Option<String>,
        base_commit: Option<String>,
    ) {
        self.worktree_path = worktree_path;
        self.worktree_branch = worktree_branch;
        self.base_commit = base_commit;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct InflightTurnIdentity {
    pub user_msg_id: u64,
    pub started_at: String,
    pub tmux_session_name: Option<String>,
}

impl InflightTurnIdentity {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            user_msg_id: state.user_msg_id,
            started_at: state.started_at.clone(),
            tmux_session_name: state.tmux_session_name.clone(),
        }
    }

    fn matches_state(&self, state: &InflightTurnState) -> bool {
        self.user_msg_id == state.user_msg_id
            && self.started_at == state.started_at
            && self.tmux_session_name == state.tmux_session_name
    }
}

/// #2235: tolerant deserializer for `runtime_kind`. A newer binary may write
/// a `RuntimeHandoffKind` variant this binary does not know about; serde's
/// default `deny_unknown_variants` posture would propagate a parse error and
/// `load_inflight_states_from_root` would delete the entire row as malformed
/// (`inflight_malformed_json_graceful_skip`). Instead we map unknown strings
/// to `None`. The recovery engine consults this `None` together with the
/// row-shape heuristic to decide whether to silent-skip recovery (issue
/// #2235 DoD #3) instead of guessing a runtime and surfacing a misleading
/// "input fifo path missing" notice.
fn deserialize_runtime_kind_tolerant<'de, D>(
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
fn deserialize_relay_owner_kind_tolerant<'de, D>(
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

fn serialize_task_notification_kind<S>(
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

fn deserialize_task_notification_kind<'de, D>(
    deserializer: D,
) -> Result<Option<TaskNotificationKind>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    Ok(value.as_deref().and_then(TaskNotificationKind::from_str))
}

pub(super) fn inflight_runtime_root() -> Option<PathBuf> {
    discord_inflight_root()
}

/// #2235: expose the local `INFLIGHT_STATE_VERSION` so the recovery engine
/// can decide whether an on-disk row was authored by a newer binary (i.e.
/// `state.version > inflight_state_version()`). Read-only accessor — the
/// constant itself stays private so we control the single bump site.
pub(super) fn inflight_state_version() -> u32 {
    INFLIGHT_STATE_VERSION
}

/// Load all inflight states for a provider WITHOUT the eviction side-effect
/// that `load_inflight_states_from_root` performs. Returns each state paired
/// with its file-mtime age in seconds. Used by `placeholder_sweeper` so the
/// sweeper can read-then-act-then-evict in one pass instead of racing the
/// regular load path's auto-deletion on stale entries.
pub(super) fn load_inflight_states_for_sweep(
    provider: &ProviderKind,
) -> Vec<(InflightTurnState, u64)> {
    let Some(root) = inflight_runtime_root() else {
        return Vec::new();
    };
    let dir = inflight_provider_dir(&root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            continue;
        };
        if state.provider_kind().as_ref() != Some(provider) {
            continue;
        }
        let age_secs = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        out.push((state, age_secs));
    }
    out
}

/// Delete the inflight state file for a (provider, channel_id) pair if it
/// still exists. Used by `placeholder_sweeper` to evict abandoned states
/// after a final placeholder edit. Idempotent.
pub(super) fn delete_inflight_state_file(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    fs::remove_file(path).is_ok()
}

fn inflight_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

fn inflight_state_path(root: &Path, provider: &ProviderKind, channel_id: u64) -> PathBuf {
    inflight_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

struct InflightStateFileLock {
    _file: fs::File,
}

impl Drop for InflightStateFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Best effort unlock; closing the fd would release it anyway.
            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

fn inflight_state_lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

fn lock_inflight_state_path(path: &Path) -> Result<InflightStateFileLock, String> {
    let lock_path = inflight_state_lock_path(path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&lock_path)
        .map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    Ok(InflightStateFileLock { _file: file })
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Parse the persisted `started_at` (`now_string` localtime form) back into
/// a Unix timestamp. Returns `None` for unparseable values so callers can
/// fall back to a wall-clock derived approximation.
pub(super) fn parse_started_at_unix(started_at: &str) -> Option<i64> {
    let naive = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .single()
        .map(|local| local.timestamp())
}

/// Parse a persisted `updated_at` field (same `now_string` localtime form
/// as `started_at`) back into a Unix timestamp. Wrapper kept distinct from
/// `parse_started_at_unix` purely for call-site readability — both fields
/// share the same encoding but represent different lifecycle moments.
pub(super) fn parse_updated_at_unix(updated_at: &str) -> Option<i64> {
    parse_started_at_unix(updated_at)
}

/// #1446 stall-deadlock recovery: returns `true` when the persisted
/// `updated_at` of an inflight state is older than
/// `threshold_secs` seconds relative to `now_unix_secs`.
///
/// Returns `false` if `updated_at` is unparseable — staleness should never
/// be inferred from missing data. This keeps the helper safe to call from
/// the THREAD-GUARD and stall-watchdog paths even when a partially
/// migrated state file is on disk.
pub(super) fn inflight_state_is_stale(
    state: &InflightTurnState,
    now_unix_secs: i64,
    threshold_secs: u64,
) -> bool {
    let Some(updated_at_unix) = parse_updated_at_unix(&state.updated_at) else {
        return false;
    };
    let age_secs = now_unix_secs.saturating_sub(updated_at_unix);
    age_secs >= 0 && (age_secs as u64) >= threshold_secs
}

fn turn_id_for_state(state: &InflightTurnState) -> Option<String> {
    (state.user_msg_id != 0).then(|| format!("discord:{}:{}", state.channel_id, state.user_msg_id))
}

fn record_inflight_invariant(
    condition: bool,
    state: &InflightTurnState,
    invariant: &'static str,
    code_location: &'static str,
    message: &'static str,
    details: serde_json::Value,
) -> bool {
    let turn_id = turn_id_for_state(state);
    crate::services::observability::record_invariant_check(
        condition,
        crate::services::observability::InvariantViolation {
            provider: Some(state.provider.as_str()),
            channel_id: Some(state.channel_id),
            dispatch_id: state.dispatch_id.as_deref(),
            session_key: state.session_key.as_deref(),
            turn_id: turn_id.as_deref(),
            invariant,
            code_location,
            message,
            details,
        },
    )
}

fn validate_inflight_state_for_save(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    code_location: &'static str,
) {
    let offset_in_bounds = state.response_sent_offset <= state.full_response.len()
        && state
            .full_response
            .is_char_boundary(state.response_sent_offset);
    record_inflight_invariant(
        offset_in_bounds,
        state,
        "response_sent_offset_in_bounds",
        code_location,
        "inflight response_sent_offset must stay within full_response",
        serde_json::json!({
            "response_sent_offset": state.response_sent_offset,
            "full_response_len": state.full_response.len(),
            "path": path.display().to_string(),
        }),
    );
    debug_assert!(
        offset_in_bounds,
        "inflight response_sent_offset must stay within full_response"
    );

    let Ok(existing_content) = fs::read_to_string(path) else {
        return;
    };
    let Ok(existing) = serde_json::from_str::<InflightTurnState>(&existing_content) else {
        return;
    };

    let monotonic_offset = state.response_sent_offset >= existing.response_sent_offset;
    record_inflight_invariant(
        monotonic_offset,
        state,
        "response_sent_offset_monotonic",
        code_location,
        "inflight response_sent_offset must not move backwards",
        serde_json::json!({
            "previous": existing.response_sent_offset,
            "next": state.response_sent_offset,
            "path": path.display().to_string(),
        }),
    );
    debug_assert!(
        monotonic_offset,
        "inflight response_sent_offset must not move backwards"
    );

    let same_tmux_owner = existing.tmux_session_name.is_none()
        || state.tmux_session_name.is_none()
        || existing.tmux_session_name == state.tmux_session_name;
    record_inflight_invariant(
        same_tmux_owner,
        state,
        "inflight_tmux_one_to_one",
        code_location,
        "inflight state for a channel must not drift between tmux sessions",
        serde_json::json!({
            "previous_tmux_session_name": existing.tmux_session_name.as_deref(),
            "next_tmux_session_name": state.tmux_session_name.as_deref(),
            "root": root.display().to_string(),
            "path": path.display().to_string(),
        }),
    );
}

pub(super) fn save_inflight_state(state: &InflightTurnState) -> Result<(), String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_in_root(&root, state)
}

/// #897 counter-model review P2 #1 — atomic "create, don't overwrite"
/// variant of `save_inflight_state`. Used by `POST /api/inflight/rebind` so a
/// concurrent legitimate turn that wins the mailbox race between the rebind
/// handler's existence check and its write cannot have its canonical
/// inflight file silently overwritten by the synthetic rebind state
/// (`user_msg_id=0`, placeholder ids zeroed). Returns `InflightAlreadyExists`
/// when the target path is already occupied — the handler translates that
/// into HTTP 409 and the operator retries (or leaves it to the live turn).
#[derive(Debug)]
pub(super) enum CreateNewInflightError {
    /// A state file already exists at the target path — another path wrote
    /// it between the caller's preflight check and this call.
    AlreadyExists,
    /// Filesystem or serialization failure.
    Internal(String),
}

impl std::fmt::Display for CreateNewInflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyExists => write!(f, "inflight state already exists"),
            Self::Internal(msg) => write!(f, "{msg}"),
        }
    }
}

pub(super) fn save_inflight_state_create_new(
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    let Some(root) = inflight_runtime_root() else {
        return Err(CreateNewInflightError::Internal(
            "Home directory not found".to_string(),
        ));
    };
    save_inflight_state_create_new_in_root(&root, state)
}

/// Test-visible inner form of `save_inflight_state_create_new`. Takes an
/// explicit root so unit tests can exercise the O_CREAT|O_EXCL semantics
/// without tripping over `AGENTDESK_ROOT_DIR` env-var races.
fn save_inflight_state_create_new_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<(), CreateNewInflightError> {
    let Some(provider) = state.provider_kind() else {
        return Err(CreateNewInflightError::Internal(format!(
            "Unknown provider '{}'",
            state.provider
        )));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
    }
    let _lock = lock_inflight_state_path(&path).map_err(CreateNewInflightError::Internal)?;
    validate_inflight_state_for_save(
        root,
        &path,
        state,
        "src/services/discord/inflight.rs:save_inflight_state_create_new_in_root",
    );
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated)
        .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;

    // `OpenOptions::create_new(true)` is the canonical atomic check-and-
    // create primitive on POSIX (O_CREAT | O_EXCL). No reliance on a
    // preceding `load_inflight_state` — the kernel itself serializes this.
    use std::io::Write;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(json.as_bytes())
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            file.sync_all()
                .map_err(|e| CreateNewInflightError::Internal(e.to_string()))?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(CreateNewInflightError::AlreadyExists)
        }
        Err(e) => Err(CreateNewInflightError::Internal(e.to_string())),
    }
}

fn save_inflight_state_in_root(root: &Path, state: &InflightTurnState) -> Result<(), String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_inflight_state_path(&path)?;
    validate_inflight_state_for_save(
        root,
        &path,
        state,
        "src/services/discord/inflight.rs:save_inflight_state_in_root",
    );
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

pub(crate) fn clear_inflight_state(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    fs::remove_file(path).is_ok()
}

/// Outcome of an explicit-signal cleanup attempt that is guarded against
/// racing the next turn's inflight write (#2427 Pitfall #1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardedClearOutcome {
    /// File matched the expected `user_msg_id` and was removed.
    Cleared,
    /// File existed but a different `user_msg_id` was on disk — the next
    /// turn already wrote its inflight, so we leave it alone.
    UserMsgMismatch,
    /// File on disk is a planned-restart marker (`restart_mode` set). The
    /// caller is an explicit cleanup signal that fired for the previous
    /// generation, so the marker must be preserved for recovery.
    PlannedRestartSkipped,
    /// File on disk is a rebind origin (`rebind_origin = true`). Its
    /// lifetime is owned by `/api/inflight/rebind`, not the watcher /
    /// turn-bridge, so the cleanup signal does not apply.
    RebindOriginSkipped,
    /// No inflight file existed (already cleared by a peer / never written).
    Missing,
    /// Filesystem error during the final `remove_file` step. Distinguished
    /// from `Missing` so callers can surface the cleanup failure (warn/error
    /// log + do NOT cancel the watcher, since the inflight is still on
    /// disk and the next sweeper tick will retry). Codex review HIGH on
    /// PR #2460: previously these errors were silently bucketed as Missing,
    /// hiding broken cleanup from the operator while the 1800s safety-net
    /// did the real work.
    IoError,
}

/// Idempotent inflight cleanup driven by an *explicit* turn-completion
/// signal (`TurnCompleted` emit, pane death detection, etc.). This is the
/// #2427 D / A wire — by the time we run, the regular hook on the
/// completion path may have already cleared the file (Cleared turns into
/// Missing). We only act when the inflight on disk still describes the
/// turn we believe just finished.
///
/// Guards:
/// * `expected_user_msg_id` — required to defeat the Pitfall #1 race where
///   a stale `TurnCompleted` arrives after the next turn has already
///   written its inflight. `0` is treated as "no guard available" and we
///   refuse to delete to stay on the conservative side.
/// * `restart_mode = Some(_)` — preserved (planned drain/hot-swap turns
///   must survive across the dcserver restart they were saved for).
/// * `rebind_origin = true` — preserved (Pitfall #5).
pub(crate) fn clear_inflight_state_if_matches(
    provider: &ProviderKind,
    channel_id: u64,
    expected_user_msg_id: u64,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_in_root(&root, provider, channel_id, expected_user_msg_id)
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_identity_in_root(&root, provider, channel_id, expected)
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_identity_after_delivery(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    full_response: &str,
    response_sent_offset: usize,
    last_offset: u64,
) -> (GuardedClearOutcome, bool) {
    let Some(root) = inflight_runtime_root() else {
        return (GuardedClearOutcome::Missing, false);
    };
    clear_inflight_state_if_matches_identity_after_delivery_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
        full_response,
        response_sent_offset,
        last_offset,
    )
}

pub(in crate::services::discord) fn clear_inflight_state_if_matches_tmux_response(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    response: &str,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_tmux_response_in_root(
        &root,
        provider,
        channel_id,
        tmux_session_name,
        response,
    )
}

pub(in crate::services::discord) fn refresh_inflight_last_offset_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    output_path: &str,
    expected_current_msg_id: Option<u64>,
    last_offset: u64,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    refresh_inflight_last_offset_if_matches_identity_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_turn_start_offset,
        output_path,
        expected_current_msg_id,
        last_offset,
    )
}

/// Root-explicit variant for unit tests. Production callers should use
/// [`clear_inflight_state_if_matches`].
pub(super) fn clear_inflight_state_if_matches_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected_user_msg_id: u64,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        // Malformed file: treat like Missing — the loader-side eviction
        // will GC the malformed payload on the next read.
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if expected_user_msg_id == 0 || state.user_msg_id != expected_user_msg_id {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    // #2450: save and guarded-clear share the same sidecar lock, so the
    // read/validate/unlink sequence below cannot race a concurrent
    // atomic-write rename for a fresh turn.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(pre) = fs::metadata(&path) else {
            return GuardedClearOutcome::Missing;
        };
        let Ok(post) = fs::metadata(&path) else {
            return GuardedClearOutcome::Missing;
        };
        if pre.dev() != post.dev() || pre.ino() != post.ino() {
            return GuardedClearOutcome::UserMsgMismatch;
        }
        // Final re-read + re-validate before unlink keeps the older
        // corruption/mismatch protections intact while the sidecar lock
        // closes the save-vs-clear race.
        let Ok(reread) = fs::read_to_string(&path) else {
            return GuardedClearOutcome::Missing;
        };
        let Ok(restate) = serde_json::from_str::<InflightTurnState>(&reread) else {
            return GuardedClearOutcome::Missing;
        };
        if restate.user_msg_id != expected_user_msg_id
            || restate.restart_mode.is_some()
            || restate.rebind_origin
        {
            return GuardedClearOutcome::UserMsgMismatch;
        }
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected_user_msg_id,
                error = %error,
                "inflight guarded-clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

fn clear_inflight_state_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedClearOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&state) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

fn normalize_response_sent_offset(full_response: &str, response_sent_offset: usize) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn clear_inflight_state_if_matches_identity_after_delivery_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    full_response: &str,
    response_sent_offset: usize,
    last_offset: u64,
) -> (GuardedClearOutcome, bool) {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return (GuardedClearOutcome::IoError, false);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return (GuardedClearOutcome::Missing, false);
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return (GuardedClearOutcome::Missing, false);
    };
    if state.restart_mode.is_some() {
        return (GuardedClearOutcome::PlannedRestartSkipped, false);
    }
    if state.rebind_origin {
        return (GuardedClearOutcome::RebindOriginSkipped, false);
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&state) {
        return (GuardedClearOutcome::UserMsgMismatch, false);
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if state.turn_start_offset != Some(expected_offset) {
            return (GuardedClearOutcome::UserMsgMismatch, false);
        }
    }

    let mut delivered_state = state;
    delivered_state.full_response = full_response.to_string();
    delivered_state.response_sent_offset =
        normalize_response_sent_offset(full_response, response_sent_offset);
    delivered_state.last_offset = last_offset;
    delivered_state.updated_at = now_string();

    let mirrored_delivery = match serde_json::to_string_pretty(&delivered_state)
        .map_err(|error| error.to_string())
        .and_then(|json| atomic_write(&path, &json))
    {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight delivery mirror failed before identity-guarded clear"
            );
            false
        }
    };

    let outcome = match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded delivery clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    };
    (outcome, mirrored_delivery)
}

fn clear_inflight_state_if_matches_tmux_response_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    response: &str,
) -> GuardedClearOutcome {
    let tmux_session_name = tmux_session_name.trim();
    let response = response.trim();
    if tmux_session_name.is_empty() || response.is_empty() {
        return GuardedClearOutcome::UserMsgMismatch;
    }

    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedClearOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedClearOutcome::Missing;
    };
    let Ok(state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedClearOutcome::Missing;
    };
    if state.restart_mode.is_some() {
        return GuardedClearOutcome::PlannedRestartSkipped;
    }
    if state.rebind_origin {
        return GuardedClearOutcome::RebindOriginSkipped;
    }
    if state.tmux_session_name.as_deref().map(str::trim) != Some(tmux_session_name) {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    if state.full_response.trim() != response {
        return GuardedClearOutcome::UserMsgMismatch;
    }

    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                tmux_session_name,
                error = %error,
                "inflight tmux-response guarded clear remove_file failed; treating as IoError so sweeper retries"
            );
            GuardedClearOutcome::IoError
        }
    }
}

fn refresh_inflight_last_offset_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    output_path: &str,
    expected_current_msg_id: Option<u64>,
    last_offset: u64,
) -> bool {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(mut state) = serde_json::from_str::<InflightTurnState>(&data) else {
        return false;
    };
    if state.restart_mode.is_some() || state.rebind_origin {
        return false;
    }
    if state.output_path.as_deref() != Some(output_path) {
        return false;
    }
    if let Some(expected_msg_id) = expected_current_msg_id {
        if state.current_msg_id != expected_msg_id {
            return false;
        }
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&state) {
        return false;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if state.turn_start_offset != Some(expected_offset) {
            return false;
        }
    }

    state.last_offset = last_offset;
    state.updated_at = now_string();
    serde_json::to_string_pretty(&state)
        .map_err(|error| error.to_string())
        .and_then(|json| atomic_write(&path, &json))
        .is_ok()
}

fn inflight_state_allows_idle_tmux_repair_state(state: &InflightTurnState) -> bool {
    state.full_response.trim().is_empty()
        && state.response_sent_offset == 0
        && state.last_watcher_relayed_offset.is_none()
        && state.dispatch_id.as_deref().is_none_or(str::is_empty)
        && state.current_tool_line.is_none()
        && state.last_tool_name.is_none()
        && !state.long_running_placeholder_active
}

pub(crate) fn inflight_state_allows_idle_tmux_repair(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<bool> {
    load_inflight_state(provider, channel_id)
        .map(|state| inflight_state_allows_idle_tmux_repair_state(&state))
}

pub(super) fn inflight_state_file_exists(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    inflight_state_path(&root, provider, channel_id).exists()
}

pub(super) fn clear_inflight_by_tmux_name(provider: &ProviderKind, tmux_name: &str) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };

    let provider_dir = inflight_provider_dir(&root, provider);
    let Ok(entries) = fs::read_dir(&provider_dir) else {
        return false;
    };

    let mut cleared = false;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(_lock) = lock_inflight_state_path(&path) else {
            continue;
        };
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<InflightTurnState>(&content) else {
            continue;
        };
        if state.tmux_session_name.as_deref() != Some(tmux_name) {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            cleared = true;
        }
    }

    cleared
}

pub(super) fn mark_all_inflight_states_restart_mode(
    provider: &ProviderKind,
    restart_mode: InflightRestartMode,
) -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    let states = load_inflight_states_from_root(&root, provider);
    let mut updated = 0usize;
    for mut state in states {
        state.set_restart_mode(restart_mode);
        if save_inflight_state_in_root(&root, &state).is_ok() {
            updated += 1;
        }
    }
    updated
}

/// #2437 (#2427 C wire) boot-time bulk invalidate. Removes inflight
/// state files whose `restart_generation` does not match
/// `current_generation` AND that are NOT planned-restart rows. The
/// planned-restart gate in `stale_removal_reason` (this file, the
/// `state.restart_mode.is_some()` branch) already handles its own
/// generation-mismatch eviction with `DRAIN_RESTART_MAX_AGE_SECS` /
/// `HOT_SWAP_HANDOFF_MAX_AGE_SECS` retention — do not double-evict
/// those here or recovery will lose handoff rows from the prior
/// generation.
///
/// Skips:
///   * `state.restart_mode.is_some()` — planned restart / hot-swap.
///   * `state.rebind_origin` — rebind API owns these, not generation.
///   * `state.restart_generation == Some(current_generation)` — this
///     generation's own rows.
///
/// Returns the number of state files removed. Intended to be called
/// **once per provider** at dcserver boot, BEFORE
/// `restore_inflight_turns`, so recovery does not revive a row from a
/// generation whose tmux session no longer exists.
pub(crate) fn invalidate_stale_generation(
    provider: &ProviderKind,
    current_generation: u64,
) -> usize {
    let Some(root) = inflight_runtime_root() else {
        return 0;
    };
    let removed = invalidate_stale_generation_in_root(&root, provider, current_generation);
    removed.len()
}

/// Test-friendly variant. Returns the list of evicted `(channel_id,
/// row_generation)` tuples so unit tests can pin both the count and
/// the row identities without re-loading the directory.
fn invalidate_stale_generation_in_root(
    root: &Path,
    provider: &ProviderKind,
    current_generation: u64,
) -> Vec<(u64, Option<u64>)> {
    let states = load_inflight_states_from_root(root, provider);
    let mut removed = Vec::new();
    for state in states {
        if state.restart_mode.is_some() {
            continue;
        }
        if state.rebind_origin {
            continue;
        }
        // Codex review HIGH on PR #2460: normal rows are constructed with
        // `restart_generation: None` (see `InflightTurnState::new`). The
        // previous `Some(current_generation)` guard alone would evict every
        // healthy current-generation row at boot. Preserve unstamped rows
        // too so only rows explicitly stamped from a PRIOR generation are
        // evicted. (Stale unstamped rows are still bounded by the
        // intake-time staleness threshold path; this function is the
        // boot-time hammer, not the long-lived cleaner.)
        match state.restart_generation {
            None => continue,
            Some(row_generation) if row_generation == current_generation => continue,
            Some(_) => {}
        }
        let path = inflight_state_path(root, provider, state.channel_id);
        let Ok(_lock) = lock_inflight_state_path(&path) else {
            continue;
        };
        let Some(state) = read_inflight_state_content(&path) else {
            continue;
        };
        if state.provider_kind().as_ref() != Some(provider) {
            continue;
        }
        if state.restart_mode.is_some() {
            continue;
        }
        if state.rebind_origin {
            continue;
        }
        match state.restart_generation {
            None => continue,
            Some(row_generation) if row_generation == current_generation => continue,
            Some(_) => {}
        }
        if fs::remove_file(&path).is_ok() {
            // Only emit observability when called via the env wrapper —
            // raw `_in_root` calls are unit tests and we want to keep
            // them deterministic.
            crate::services::observability::emit_inflight_lifecycle_event(
                provider.as_str(),
                state.channel_id,
                state.dispatch_id.as_deref(),
                None,
                None,
                "evict_stale_generation",
                serde_json::json!({
                    "reason": "generation_mismatch_boot",
                    "row_generation": state.restart_generation,
                    "current_generation": current_generation,
                    "user_msg_id": state.user_msg_id,
                }),
            );
            removed.push((state.channel_id, state.restart_generation));
        }
    }
    removed
}

/// Load a single inflight state by provider + channel_id (returns None if missing).
pub(super) fn load_inflight_state(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<InflightTurnState> {
    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub(super) fn load_inflight_states(provider: &ProviderKind) -> Vec<InflightTurnState> {
    let Some(root) = inflight_runtime_root() else {
        return Vec::new();
    };
    load_inflight_states_from_root(&root, provider)
}

pub(crate) fn latest_request_owner_user_id_for_channel(channel_id: u64) -> Option<u64> {
    let providers = [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
        ProviderKind::Qwen,
    ];

    providers
        .iter()
        .flat_map(load_inflight_states)
        .filter(|state| state.channel_id == channel_id)
        .max_by(|left, right| left.updated_at.cmp(&right.updated_at))
        .map(|state| state.request_owner_user_id)
}

fn planned_restart_retention_secs(restart_mode: InflightRestartMode) -> u64 {
    match restart_mode {
        InflightRestartMode::DrainRestart => DRAIN_RESTART_MAX_AGE_SECS,
        InflightRestartMode::HotSwapHandoff => HOT_SWAP_HANDOFF_MAX_AGE_SECS,
    }
}

/// Thread-local test seam for `tmux_pane_alive_for_stale_check`. Production
/// always calls `tmux_diagnostics::tmux_session_has_live_pane`; tests inject a
/// known-alive name set via `set_test_tmux_alive_override` so the override
/// behaviour can be exercised without spawning real tmux.
#[cfg(test)]
static TEST_TMUX_ALIVE_OVERRIDE: std::sync::OnceLock<
    std::sync::Mutex<Option<std::collections::HashSet<String>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn set_test_tmux_alive_override(names: Option<&[&str]>) {
    let lock = TEST_TMUX_ALIVE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = lock.lock().expect("tmux alive override lock poisoned");
    *guard = names.map(|slice| slice.iter().map(|s| (*s).to_string()).collect());
}

fn tmux_pane_alive_for_stale_check(name: &str) -> bool {
    #[cfg(test)]
    {
        if let Some(lock) = TEST_TMUX_ALIVE_OVERRIDE.get()
            && let Ok(guard) = lock.lock()
            && let Some(set) = guard.as_ref()
        {
            return set.contains(name);
        }
    }
    crate::services::tmux_diagnostics::tmux_session_has_live_pane(name)
}

fn stale_removal_reason(
    state: &InflightTurnState,
    age_secs: u64,
    current_generation: u64,
) -> Option<String> {
    match state.restart_mode {
        Some(restart_mode) => {
            if state.restart_generation != Some(current_generation) {
                return Some(format!(
                    "removing {} inflight state from old generation {:?} (current generation {})",
                    restart_mode.label(),
                    state.restart_generation,
                    current_generation
                ));
            }
            let max_age = planned_restart_retention_secs(restart_mode);
            if age_secs > max_age {
                // Defense-in-depth: when DrainRestart inflight ages past the
                // 30-min retention window, refuse to wipe if the inflight's
                // tmux pane is still alive. Wiping the row strands the live
                // CLI's eventual response — see the 2026-05-26 incident where
                // repeated quick-exits left a codex turn pane alive but its
                // inflight anchor was removed at the 10th boot. Only one
                // probe per stale row, gated by all the cheaper checks above.
                if matches!(restart_mode, InflightRestartMode::DrainRestart)
                    && let Some(name) = state.tmux_session_name.as_deref()
                    && tmux_pane_alive_for_stale_check(name)
                {
                    tracing::warn!(
                        "  ⚠ inflight stale-age ({age_secs}s > {max_age}s) overridden — tmux pane '{name}' still alive (channel {})",
                        state.channel_id
                    );
                    return None;
                }
                return Some(format!(
                    "removing stale {} inflight state file ({age_secs}s old > {max_age}s)",
                    restart_mode.label()
                ));
            }
            None
        }
        None => {
            if age_secs > INFLIGHT_MAX_AGE_SECS {
                if let Some(name) = state.tmux_session_name.as_deref()
                    && tmux_pane_alive_for_stale_check(name)
                {
                    tracing::warn!(
                        "  ⚠ inflight stale-age ({age_secs}s > {INFLIGHT_MAX_AGE_SECS}s) overridden — tmux pane '{name}' still alive (channel {})",
                        state.channel_id
                    );
                    return None;
                }
                Some(format!(
                    "removing stale inflight state file ({age_secs}s old > {INFLIGHT_MAX_AGE_SECS}s)"
                ))
            } else {
                None
            }
        }
    }
}

fn parse_inflight_state_content(content: &str) -> serde_json::Result<InflightTurnState> {
    let mut state = serde_json::from_str::<InflightTurnState>(content)?;
    // #2235: the tolerant `runtime_kind` deserializer collapses both
    // "field absent" (legacy v7 rows) and "present-but-unknown variant"
    // (rows written by a future binary) to `runtime_kind = None`.
    // Recovery treats these two cases differently — absent legacy rows
    // recover via heuristics; present-unknown rows silent-skip. Re-parse
    // the JSON as a value to disambiguate and record the verdict on the
    // transient `runtime_kind_unknown_on_disk` flag.
    if state.runtime_kind.is_none()
        && let Ok(raw_value) = serde_json::from_str::<serde_json::Value>(content)
        && let Some(raw_runtime) = raw_value.get("runtime_kind")
        && let Some(raw_str) = raw_runtime.as_str()
        && !raw_str.is_empty()
        && !matches!(
            raw_str,
            "legacy_tmux_wrapper" | "claude_tui" | "codex_tui" | "process_backend"
        )
    {
        state.runtime_kind_unknown_on_disk = true;
    }
    Ok(state)
}

fn read_inflight_state_content(path: &Path) -> Option<InflightTurnState> {
    let content = fs::read_to_string(path).ok()?;
    parse_inflight_state_content(&content).ok()
}

fn stale_removal_reason_for_path(
    path: &Path,
    state: &InflightTurnState,
    current_generation: u64,
) -> Option<String> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let age = modified.elapsed().ok()?;
    stale_removal_reason(state, age.as_secs(), current_generation)
}

fn load_inflight_states_from_root(root: &Path, provider: &ProviderKind) -> Vec<InflightTurnState> {
    let dir = inflight_provider_dir(root, provider);
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut states = Vec::new();
    let mut tmux_owners: HashMap<String, u64> = HashMap::new();
    let current_generation = super::runtime_store::load_generation();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to read inflight state file: {}",
                path.display()
            );
            continue;
        };
        let mut state = match parse_inflight_state_content(&content) {
            Ok(state) => state,
            Err(_) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚠ removing malformed inflight state file: {}",
                    path.display()
                );
                let Ok(_lock) = lock_inflight_state_path(&path) else {
                    continue;
                };
                match read_inflight_state_content(&path) {
                    Some(locked_state) => locked_state,
                    None => {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                }
            }
        };
        if state.provider_kind().as_ref() != Some(provider) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ removing inflight state with provider mismatch: {}",
                path.display()
            );
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                continue;
            };
            let Some(locked_state) = read_inflight_state_content(&path) else {
                let _ = fs::remove_file(&path);
                continue;
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                let _ = fs::remove_file(&path);
                continue;
            }
            state = locked_state;
        }
        if stale_removal_reason_for_path(&path, &state, current_generation).is_some() {
            let Ok(_lock) = lock_inflight_state_path(&path) else {
                continue;
            };
            let Some(locked_state) = read_inflight_state_content(&path) else {
                let _ = fs::remove_file(&path);
                continue;
            };
            if locked_state.provider_kind().as_ref() != Some(provider) {
                let _ = fs::remove_file(&path);
                continue;
            }
            if let Some(reason) =
                stale_removal_reason_for_path(&path, &locked_state, current_generation)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] ⚠ {}: {}", reason, path.display());
                let _ = fs::remove_file(&path);
                continue;
            }
            state = locked_state;
        }
        if let Some(tmux_session_name) = state
            .tmux_session_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            if let Some(previous_channel_id) =
                tmux_owners.insert(tmux_session_name.to_string(), state.channel_id)
            {
                record_inflight_invariant(
                    false,
                    &state,
                    "inflight_tmux_one_to_one",
                    "src/services/discord/inflight.rs:load_inflight_states_from_root",
                    "one tmux session must not be owned by multiple inflight channel files",
                    serde_json::json!({
                        "tmux_session_name": tmux_session_name,
                        "previous_channel_id": previous_channel_id,
                        "current_channel_id": state.channel_id,
                        "path": path.display().to_string(),
                    }),
                );
            }
        }
        states.push(state);
    }
    states
}

/// #2448: explicit completion signal published from the turn_bridge
/// CompletionGuard so downstream listeners (currently the standby JSONL
/// relay) can exit promptly instead of polling against a wall-clock
/// timeout. Variants are intentionally narrow; add cases as new
/// listeners need them.
#[derive(Debug, Clone)]
pub(in crate::services::discord) enum InflightSignal {
    /// The turn_bridge task for `channel_id` reached its terminal drop —
    /// any per-turn relay tasks bound to this channel may now exit.
    Completed { channel_id: u64 },
}

/// #1446 Layer 1 — `inflight_state_is_stale` is a pure helper with no
/// filesystem or runtime dependencies, so we keep its test always-on
/// (`#[cfg(test)]`) rather than gating it on the `legacy-sqlite-tests`
/// feature like the rest of this file. The legacy-gated tests below
/// require a live SQLite test harness and cannot run in plain `cargo
/// test --bin agentdesk` invocations.
#[cfg(test)]
mod stall_recovery_tests {
    use super::{
        GuardedClearOutcome, INFLIGHT_STALENESS_THRESHOLD_SECS, InflightRestartMode,
        InflightTurnIdentity, InflightTurnState,
        clear_inflight_state_if_matches_identity_after_delivery_in_root,
        clear_inflight_state_if_matches_identity_in_root, clear_inflight_state_if_matches_in_root,
        clear_inflight_state_if_matches_tmux_response_in_root,
        inflight_state_allows_idle_tmux_repair_state, inflight_state_is_stale, inflight_state_path,
        load_inflight_states_from_root, lock_inflight_state_path, normalize_response_sent_offset,
        refresh_inflight_last_offset_if_matches_identity_in_root, save_inflight_state_in_root,
    };
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;
    use chrono::TimeZone;
    use tempfile::TempDir;

    /// `inflight_state_is_stale` must flip to true once `updated_at` is
    /// older than the configured threshold and stay false for fresh state.
    #[test]
    fn inflight_state_is_stale_returns_true_after_threshold() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            7,
            8,
            9,
            "hello".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            None,
            None,
            0,
        );

        // Anchor `now` and derive `updated_at` from it deterministically so
        // the test is independent of wall clock.
        let now_unix = chrono::Utc::now().timestamp();
        let fresh_unix = now_unix - 5;
        let stale_unix = now_unix - (INFLIGHT_STALENESS_THRESHOLD_SECS as i64) - 1;

        let to_local = |unix: i64| {
            chrono::Local
                .timestamp_opt(unix, 0)
                .single()
                .expect("valid local time")
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        };

        state.updated_at = to_local(fresh_unix);
        assert!(
            !inflight_state_is_stale(&state, now_unix, INFLIGHT_STALENESS_THRESHOLD_SECS),
            "fresh state must NOT be reported as stale"
        );

        state.updated_at = to_local(stale_unix);
        assert!(
            inflight_state_is_stale(&state, now_unix, INFLIGHT_STALENESS_THRESHOLD_SECS),
            "state older than threshold must be reported as stale"
        );

        // Unparseable timestamp must default to "not stale" — never infer
        // staleness from missing data.
        state.updated_at = "garbage-not-a-date".to_string();
        assert!(
            !inflight_state_is_stale(&state, now_unix, INFLIGHT_STALENESS_THRESHOLD_SECS),
            "unparseable updated_at must NOT be treated as stale"
        );
    }

    #[test]
    fn idle_tmux_repair_only_allows_empty_unclaimed_inflight() {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            888,
            Some("adk-cc".to_string()),
            1,
            2,
            3,
            "user prompt".to_string(),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        assert!(inflight_state_allows_idle_tmux_repair_state(&state));

        state.current_msg_len = "⠋ Processing...".len();
        assert!(inflight_state_allows_idle_tmux_repair_state(&state));

        state.full_response = "partial".to_string();
        assert!(!inflight_state_allows_idle_tmux_repair_state(&state));
        state.full_response.clear();

        state.last_watcher_relayed_offset = Some(10);
        assert!(!inflight_state_allows_idle_tmux_repair_state(&state));
        state.last_watcher_relayed_offset = None;

        state.dispatch_id = Some("dispatch-1".to_string());
        assert!(!inflight_state_allows_idle_tmux_repair_state(&state));
    }

    #[test]
    fn status_message_id_round_trips_for_status_panel_resume() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.status_message_id = Some(123_456);

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status_message_id, Some(123_456));
        assert_eq!(loaded[0].current_msg_id, 99);
    }

    #[test]
    fn runtime_kind_round_trips_and_direct_tui_has_no_fifo_requirement() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            77,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            12,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].runtime_kind, Some(RuntimeHandoffKind::ClaudeTui));
        assert_eq!(
            loaded[0].runtime_kind_for_recovery(),
            RuntimeHandoffKind::ClaudeTui
        );
        assert!(loaded[0].input_fifo_path.is_none());
        assert!(!loaded[0].runtime_kind_for_recovery().requires_input_fifo());
    }

    /// #2235 v8 compat shape: a ClaudeTui inflight row that carries both a
    /// stamped `runtime_kind` and a populated `input_fifo_path` must
    /// round-trip cleanly under `INFLIGHT_STATE_VERSION` = 8 so an old
    /// (pre-#2213) binary rolling back over the file can still satisfy its
    /// FIFO-required recovery branch.
    #[test]
    fn inflight_v8_claude_tui_round_trips_with_fifo_for_rollback_compat() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            55,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            Some("/tmp/claude-fifo.input".to_string()),
            12,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].version, super::INFLIGHT_STATE_VERSION);
        assert_eq!(loaded[0].version, 8);
        assert_eq!(loaded[0].runtime_kind, Some(RuntimeHandoffKind::ClaudeTui));
        assert_eq!(
            loaded[0].input_fifo_path.as_deref(),
            Some("/tmp/claude-fifo.input")
        );
        assert_eq!(
            loaded[0].runtime_kind_for_recovery(),
            RuntimeHandoffKind::ClaudeTui
        );
    }

    /// #2235: rows written by a newer binary may serialize an unknown
    /// `runtime_kind` string. `deserialize_runtime_kind_tolerant` must
    /// collapse the unknown value to `None` so the whole inflight row isn't
    /// tossed as malformed JSON. The recovery engine layers the
    /// version-aware silent-skip on top of this.
    #[test]
    fn inflight_unknown_runtime_kind_string_deserializes_as_none() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(ProviderKind::Claude.as_str());
        std::fs::create_dir_all(&dir).unwrap();

        // Seed a JSON row whose `runtime_kind` is a variant string this
        // binary does not know about (`"future_runtime"`). Without the
        // tolerant deserializer this row would be deleted as malformed by
        // `load_inflight_states_from_root`.
        let valid_state = InflightTurnState::new(
            ProviderKind::Claude,
            444,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        let mut value = serde_json::to_value(&valid_state).unwrap();
        value["runtime_kind"] = serde_json::Value::String("future_runtime".to_string());
        // Also bump the on-disk version to simulate a row authored by a
        // newer binary, so the recovery-engine silent-skip guard would
        // trigger downstream of this deserialization step.
        value["version"] =
            serde_json::Value::Number(serde_json::Number::from(super::INFLIGHT_STATE_VERSION + 1));
        let path = dir.join("444.json");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1, "tolerant deser must keep the row");
        assert_eq!(loaded[0].channel_id, 444);
        assert!(
            loaded[0].runtime_kind.is_none(),
            "unknown variant must collapse to None"
        );
        assert!(
            loaded[0].version > super::INFLIGHT_STATE_VERSION,
            "version stays forward-marked for the recovery silent-skip guard"
        );
        assert!(
            loaded[0].runtime_kind_unknown_on_disk,
            "present-but-unknown runtime_kind must be distinguishable from legacy absent-field None"
        );
    }

    /// #2235: legacy v7 rows have NO `runtime_kind` field on disk at all.
    /// These must deserialize with `runtime_kind = None` AND
    /// `runtime_kind_unknown_on_disk = false`, so the recovery silent-skip
    /// guard does not regress legacy recovery flows that depend on the
    /// `runtime_kind_for_recovery` heuristic.
    #[test]
    fn inflight_legacy_v7_row_with_absent_runtime_kind_recovers_via_heuristic() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join(ProviderKind::Claude.as_str());
        std::fs::create_dir_all(&dir).unwrap();

        let valid_state = InflightTurnState::new(
            ProviderKind::Claude,
            555,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        let mut value = serde_json::to_value(&valid_state).unwrap();
        // Strip the runtime_kind field entirely to mimic an on-disk legacy
        // v7 row from before #2213.
        value.as_object_mut().unwrap().remove("runtime_kind");
        value["version"] = serde_json::Value::Number(serde_json::Number::from(7u32));
        let path = dir.join("555.json");
        std::fs::write(&path, serde_json::to_string(&value).unwrap()).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].runtime_kind.is_none());
        assert!(
            !loaded[0].runtime_kind_unknown_on_disk,
            "absent-field legacy v7 rows must not look like a forward-unknown row"
        );
        assert_eq!(loaded[0].version, 7);
    }

    /// #2235: when an on-disk row has `runtime_kind = None` (legacy pre-v8
    /// row or a future variant this binary doesn't know about) the
    /// `runtime_kind_for_recovery` heuristic must still pick a deterministic
    /// kind. The recovery engine layered on top of this then uses
    /// `state.runtime_kind.is_none()` to switch the missing-FIFO branch to a
    /// silent debug-skip — exercised here at the data-model layer.
    #[test]
    fn inflight_unknown_runtime_kind_falls_back_without_panic() {
        let temp = TempDir::new().unwrap();
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            66,
            Some("adk-claude".to_string()),
            7,
            8,
            99,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            None,
            0,
        );
        // Simulate the pre-v8 / unknown-runtime case: no stamped runtime_kind
        // and no FIFO path. `runtime_kind_for_recovery` should fall back to
        // ClaudeTui because tmux/output are present, allowing recovery to
        // skip silently rather than synthesizing a missing-FIFO notice.
        state.runtime_kind = None;
        state.input_fifo_path = None;

        save_inflight_state_in_root(temp.path(), &state).expect("save inflight state");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].runtime_kind.is_none());
        assert_eq!(
            loaded[0].runtime_kind_for_recovery(),
            RuntimeHandoffKind::ClaudeTui
        );
    }

    #[test]
    fn inflight_malformed_json_graceful_skip() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let dir = root.join(ProviderKind::Claude.as_str());
        std::fs::create_dir_all(&dir).unwrap();

        let valid_state = InflightTurnState::new(
            ProviderKind::Claude,
            111,
            Some("adk-claude".to_string()),
            222,
            333,
            444,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk-claude".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        let valid_path = dir.join("111.json");
        std::fs::write(&valid_path, serde_json::to_string(&valid_state).unwrap()).unwrap();

        let malformed_path = dir.join("999.json");
        std::fs::write(&malformed_path, "{ malformed json ]").unwrap();

        let loaded = load_inflight_states_from_root(root, &ProviderKind::Claude);

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].channel_id, 111);
        assert!(valid_path.exists());
        assert!(!malformed_path.exists());
    }

    fn build_inflight_for_guard_tests(
        provider: ProviderKind,
        channel_id: u64,
        user_msg_id: u64,
    ) -> InflightTurnState {
        InflightTurnState::new(
            provider,
            channel_id,
            Some("adk".to_string()),
            42,
            100,
            user_msg_id,
            "user prompt".to_string(),
            None,
            Some("AgentDesk-claude-adk".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    /// #2427 D/A wire — happy path. When the on-disk inflight has a
    /// matching `user_msg_id` and is neither a planned-restart marker
    /// nor a rebind origin, the explicit signal removes it.
    #[test]
    fn clear_inflight_state_if_matches_removes_matching_turn() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        let user_msg_id = state.user_msg_id;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            user_msg_id,
        );
        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    /// #2427 Pitfall #1 — stale TurnCompleted carrying the previous
    /// turn's `user_msg_id` must NOT delete the next turn's inflight.
    #[test]
    fn clear_inflight_state_if_matches_protects_next_turn_against_stale_signal() {
        let temp = TempDir::new().unwrap();
        let next_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        save_inflight_state_in_root(temp.path(), &next_turn).unwrap();

        // Stale completion for previous turn user_msg_id = 50 arrives now.
        let outcome =
            clear_inflight_state_if_matches_in_root(temp.path(), &ProviderKind::Claude, 321, 50);
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);

        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].user_msg_id, 100);
    }

    #[test]
    fn tmux_response_guard_clears_matching_delivered_idle_relay() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.full_response = "done from idle relay".to_string();
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_tmux_response_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            "AgentDesk-claude-adk",
            "done from idle relay",
        );

        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    #[test]
    fn tmux_response_guard_preserves_new_turn_with_different_response() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        state.user_msg_id = 101;
        state.full_response = String::new();
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_tmux_response_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            "AgentDesk-claude-adk",
            "previous idle relay response",
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].user_msg_id, 101);
    }

    #[test]
    fn identity_guard_preserves_same_named_respawn() {
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();
        let old_identity = InflightTurnIdentity::from_state(&old_turn);

        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.user_text = "fresh prompt".to_string();
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        let outcome = clear_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
        );
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);

        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].started_at, "2026-05-17 10:00:05");
        assert_eq!(
            still_there[0].tmux_session_name, old_turn.tmux_session_name,
            "test must cover same-named respawn"
        );
    }

    #[test]
    fn identity_delivery_clear_removes_matching_turn() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        let identity = InflightTurnIdentity::from_state(&state);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let (outcome, mirrored) = clear_inflight_state_if_matches_identity_after_delivery_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            state.turn_start_offset,
            "hello\nworld",
            "hello\nworld".len(),
            99,
        );

        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(mirrored);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    #[test]
    fn identity_delivery_clear_does_not_overwrite_fresh_turn() {
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();
        let old_identity = InflightTurnIdentity::from_state(&old_turn);

        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.user_text = "fresh prompt".to_string();
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        let (outcome, mirrored) = clear_inflight_state_if_matches_identity_after_delivery_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
            old_turn.turn_start_offset,
            "stale delivered response",
            "stale delivered response".len(),
            99,
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert!(!mirrored);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].started_at, fresh_turn.started_at);
        assert_eq!(still_there[0].user_text, "fresh prompt");
        assert!(still_there[0].full_response.is_empty());
        assert_eq!(still_there[0].response_sent_offset, 0);
    }

    #[test]
    fn identity_delivery_clear_checks_turn_start_offset() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        let identity = InflightTurnIdentity::from_state(&state);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let (outcome, mirrored) = clear_inflight_state_if_matches_identity_after_delivery_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            Some(999),
            "stale delivered response",
            "stale delivered response".len(),
            99,
        );

        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert!(!mirrored);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert!(still_there[0].full_response.is_empty());
        assert_eq!(still_there[0].response_sent_offset, 0);
    }

    #[test]
    fn identity_heartbeat_refresh_updates_matching_turn_under_lock() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        let identity = InflightTurnIdentity::from_state(&state);
        let output_path = state.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &identity,
            state.turn_start_offset,
            &output_path,
            Some(state.current_msg_id),
            123,
        );

        assert!(refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 123);
    }

    #[test]
    fn identity_heartbeat_refresh_does_not_overwrite_fresh_turn() {
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.current_msg_id = 0;
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        let old_identity = InflightTurnIdentity::from_state(&old_turn);
        let output_path = old_turn.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();

        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.current_msg_id = 0;
        fresh_turn.user_msg_id = 101;
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.output_path = Some(output_path.clone());
        fresh_turn.last_offset = 20;
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
            old_turn.turn_start_offset,
            &output_path,
            None,
            123,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].user_msg_id, 101);
        assert_eq!(loaded[0].started_at, "2026-05-17 10:00:05");
        assert_eq!(loaded[0].last_offset, 20);
    }

    #[test]
    fn delivery_response_sent_offset_stays_on_utf8_boundary() {
        let response = "안녕";
        let first_char_middle = 1;

        assert_eq!(
            normalize_response_sent_offset(response, first_char_middle),
            0
        );
        assert_eq!(
            normalize_response_sent_offset(response, response.len() + 100),
            response.len()
        );
    }

    #[test]
    fn guarded_clear_and_save_race_preserves_fresh_state() {
        let temp = TempDir::new().unwrap();
        let root = std::sync::Arc::new(temp.path().to_path_buf());

        for iteration in 0..20 {
            let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Codex, 777, 100);
            old_turn.started_at = format!("2026-05-17 10:00:{iteration:02}");
            save_inflight_state_in_root(root.as_ref(), &old_turn).unwrap();
            let old_identity = InflightTurnIdentity::from_state(&old_turn);

            let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Codex, 777, 101);
            fresh_turn.started_at = format!("2026-05-17 10:01:{iteration:02}");
            fresh_turn.user_text = "fresh prompt".to_string();

            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let clear_root = root.clone();
            let clear_barrier = barrier.clone();
            let clear_handle = std::thread::spawn(move || {
                clear_barrier.wait();
                clear_inflight_state_if_matches_identity_in_root(
                    clear_root.as_ref(),
                    &ProviderKind::Codex,
                    777,
                    &old_identity,
                )
            });

            barrier.wait();
            save_inflight_state_in_root(root.as_ref(), &fresh_turn).unwrap();
            let _ = clear_handle.join().expect("clear thread should not panic");

            let loaded = load_inflight_states_from_root(root.as_ref(), &ProviderKind::Codex);
            assert_eq!(loaded.len(), 1);
            assert_eq!(loaded[0].started_at, fresh_turn.started_at);
            assert_eq!(loaded[0].user_text, "fresh prompt");
        }
    }

    /// #2427 — planned-restart markers must survive the explicit-signal
    /// hook because their lifetime is owned by the next dcserver boot's
    /// recovery. We bypass `load_inflight_states_from_root` here (which
    /// has its own retention-eviction side-effect) and assert directly
    /// on the file system that the row is intact after the guarded
    /// clear refused to touch it.
    #[test]
    fn clear_inflight_state_if_matches_preserves_planned_restart() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Codex, 555, 333);
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);
        let user_msg_id = state.user_msg_id;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_in_root(
            temp.path(),
            &ProviderKind::Codex,
            555,
            user_msg_id,
        );
        assert_eq!(outcome, GuardedClearOutcome::PlannedRestartSkipped);

        let provider_dir = temp.path().join(ProviderKind::Codex.as_str());
        let path = provider_dir.join("555.json");
        assert!(
            path.exists(),
            "planned-restart marker file should survive guarded clear"
        );
    }

    /// #2427 Pitfall #5 — rebind_origin rows are owned by the
    /// `/api/inflight/rebind` API. The explicit signal must NOT touch
    /// them even when user_msg_id matches.
    #[test]
    fn clear_inflight_state_if_matches_preserves_rebind_origin() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Gemini, 901, 444);
        state.rebind_origin = true;
        let user_msg_id = state.user_msg_id;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_in_root(
            temp.path(),
            &ProviderKind::Gemini,
            901,
            user_msg_id,
        );
        assert_eq!(outcome, GuardedClearOutcome::RebindOriginSkipped);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Gemini).len(),
            1
        );
    }

    /// `expected_user_msg_id = 0` is the "no guard available" sentinel —
    /// refuse to clear so the helper never accidentally deletes a row
    /// it cannot authenticate against.
    #[test]
    fn clear_inflight_state_if_matches_refuses_zero_guard() {
        let temp = TempDir::new().unwrap();
        let state = build_inflight_for_guard_tests(ProviderKind::Qwen, 8, 12_345);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome =
            clear_inflight_state_if_matches_in_root(temp.path(), &ProviderKind::Qwen, 8, 0);
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Qwen).len(),
            1
        );
    }

    /// No on-disk row → `Missing`. Idempotency safety net.
    #[test]
    fn clear_inflight_state_if_matches_missing_is_noop() {
        let temp = TempDir::new().unwrap();
        let outcome =
            clear_inflight_state_if_matches_in_root(temp.path(), &ProviderKind::Claude, 42, 999);
        assert_eq!(outcome, GuardedClearOutcome::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn load_inflight_states_revalidates_malformed_row_under_lock() {
        let temp = TempDir::new().unwrap();
        let path = inflight_state_path(temp.path(), &ProviderKind::Codex, 18_001);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ definitely not valid json").unwrap();
        let lock = lock_inflight_state_path(&path).unwrap();
        let root = temp.path().to_path_buf();

        let loader =
            std::thread::spawn(move || load_inflight_states_from_root(&root, &ProviderKind::Codex));

        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut fresh = build_inflight_for_guard_tests(ProviderKind::Codex, 18_001, 88_001);
        fresh.user_msg_id = 88_001;
        std::fs::write(&path, serde_json::to_string_pretty(&fresh).unwrap()).unwrap();
        drop(lock);

        let states = loader.join().unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].user_msg_id, 88_001);
        assert_eq!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Codex).len(),
            1
        );
    }

    /// Process-wide mutex so the two halves of the alive/dead override
    /// regression do not race against each other when cargo test runs them
    /// in parallel (the override is global state).
    fn stale_override_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// 2026-05-26 adk-cdx incident regression: a DrainRestart inflight whose
    /// file mtime aged past 1800s but whose tmux pane is still alive must
    /// NOT be removed. Wiping it strands the live CLI's eventual response.
    #[test]
    fn stale_drain_restart_preserved_when_tmux_pane_alive() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&["AgentDesk-codex-adk-cdx-stale-alive-77"]));

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            77,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-77".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-alive-77".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);

        let result = super::stale_removal_reason(&state, 2000, 7);
        super::set_test_tmux_alive_override(None);
        assert!(
            result.is_none(),
            "alive tmux pane must override stale-age removal; got {:?}",
            result
        );
    }

    /// Mirror of the above: when the same aged DrainRestart row has NO live
    /// tmux pane, the existing stale-removal still fires.
    #[test]
    fn stale_drain_restart_removed_when_tmux_pane_dead() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&[])); // empty override = nothing alive

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            78,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-78".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-dead-78".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);

        let result = super::stale_removal_reason(&state, 2000, 7);
        super::set_test_tmux_alive_override(None);
        let reason = result.expect("dead-pane DrainRestart past 1800s must be removed");
        assert!(
            reason.contains("removing stale drain_restart"),
            "unexpected removal reason: {reason}"
        );
    }

    /// 2026-05-28 adk-cdx relay gap regression: normal, non-restart inflight
    /// rows must also be preserved while their tmux pane is alive. Otherwise a
    /// long-running Codex turn can finish after the 300s cleanup and have its
    /// terminal response suppressed because the inflight anchor vanished.
    #[test]
    fn stale_normal_inflight_preserved_when_tmux_pane_alive() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&["AgentDesk-codex-adk-cdx-stale-alive-79"]));

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            79,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-79".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-alive-79".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        let result = super::stale_removal_reason(&state, super::INFLIGHT_MAX_AGE_SECS + 1, 7);
        super::set_test_tmux_alive_override(None);
        assert!(
            result.is_none(),
            "alive tmux pane must preserve normal inflight rows; got {:?}",
            result
        );
    }

    #[test]
    fn stale_normal_inflight_removed_when_tmux_pane_dead() {
        let _guard = stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&[]));

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            80,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-80".to_string()),
            Some("AgentDesk-codex-adk-cdx-stale-dead-80".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        let result = super::stale_removal_reason(&state, super::INFLIGHT_MAX_AGE_SECS + 1, 7);
        super::set_test_tmux_alive_override(None);
        let reason = result.expect("dead-pane normal inflight past 300s must be removed");
        assert!(
            reason.contains("removing stale inflight state file"),
            "unexpected removal reason: {reason}"
        );
    }
}

#[cfg(test)]
mod wave_a_cleanup_tests {
    //! #2437 (#2427 C wire) — unit tests for the boot-time generation
    //! bulk invalidate. The B wire shares `clear_inflight_state_if_matches`
    //! with #2427's D / A wires and is already covered by the
    //! `clear_inflight_state_if_matches_*` tests in the parent mod.
    use super::{
        InflightTurnState, inflight_state_path, invalidate_stale_generation_in_root,
        load_inflight_states_from_root, lock_inflight_state_path, save_inflight_state_in_root,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    fn make_state(channel_id: u64, user_msg_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            user_msg_id,
            user_msg_id + 1000,
            "hello".to_string(),
            None,
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    #[test]
    fn invalidate_stale_generation_evicts_non_planned_old_generations() {
        // C wire: a row whose `restart_generation` does not match the
        // boot-time `current_generation` AND that is not a planned
        // restart must be evicted before recovery runs.
        let temp = TempDir::new().unwrap();

        let mut row_old = make_state(501, 11);
        row_old.restart_generation = Some(3);
        save_inflight_state_in_root(temp.path(), &row_old).expect("save");

        let mut row_current = make_state(502, 22);
        row_current.restart_generation = Some(5);
        save_inflight_state_in_root(temp.path(), &row_current).expect("save");

        // Pre-condition: both rows on disk.
        let before = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(before.len(), 2);

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 5);
        assert_eq!(removed.len(), 1, "only the old-gen row should be removed");
        assert_eq!(removed[0], (501, Some(3)));

        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].channel_id, 502);
    }

    #[test]
    fn invalidate_stale_generation_preserves_planned_restart_rows() {
        // DrainRestart / HotSwapHandoff rows have their own
        // generation-mismatch handling in `stale_removal_reason` (auto-
        // evicts at load time with extended retention) — the C wire
        // must defer to that path and NOT double-evict.
        //
        // We stamp `restart_generation = Some(0)` to match the unit-
        // test environment's `load_generation()` reading (no generation
        // file → 0), so the load path itself does not auto-evict the
        // row. Then we ask `invalidate_stale_generation_in_root` to
        // run with a different "current_generation" — the helper must
        // still skip the row because `restart_mode.is_some()`, NOT
        // because the generations happen to match.
        // `load_generation()` reads the PROCESS-WIDE `AGENTDESK_ROOT_DIR`, so
        // serialize on the shared env lock and point the root at our own temp
        // dir for the whole test. Otherwise a concurrent root-mutating test can
        // flip the env between this read and the load-path read below, making
        // `current_runtime_gen` inconsistent and tripping the assertions.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = TempDir::new().unwrap();
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _env_reset = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        // With the root isolated to `temp` (no generation file → 0), the load
        // path's `stale_removal_reason` planned-restart branch hits its
        // generation-match arm and does not auto-evict.
        let current_runtime_gen = super::super::runtime_store::load_generation();

        let mut planned = make_state(601, 33);
        planned.set_restart_mode(InflightRestartMode::DrainRestart);
        planned.restart_generation = Some(current_runtime_gen);
        save_inflight_state_in_root(temp.path(), &planned).expect("save");

        // Pre-condition: row survives the load path.
        let before = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(
            before.len(),
            1,
            "load must not auto-evict same-gen planned restart"
        );

        // Now ask the C wire helper to use a "current_generation"
        // value that DEFINITELY mismatches the row's stamp. The helper
        // must still skip the row because `restart_mode.is_some()`.
        let mismatched_gen = current_runtime_gen.wrapping_add(9_999);
        let removed =
            invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, mismatched_gen);
        assert!(
            removed.is_empty(),
            "planned-restart rows must NOT be evicted by C wire bulk invalidate \
             even when their restart_generation mismatches the current generation"
        );

        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert!(after[0].restart_mode.is_some());
    }

    #[test]
    fn invalidate_stale_generation_preserves_rebind_origin_rows() {
        let temp = TempDir::new().unwrap();

        let mut rebind = make_state(701, 44);
        rebind.rebind_origin = true;
        rebind.restart_generation = Some(1);
        save_inflight_state_in_root(temp.path(), &rebind).expect("save");

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 9);
        assert!(removed.is_empty());
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert!(after[0].rebind_origin);
    }

    #[test]
    fn invalidate_stale_generation_preserves_current_generation_rows() {
        let temp = TempDir::new().unwrap();

        let mut fresh = make_state(801, 55);
        fresh.restart_generation = Some(7);
        save_inflight_state_in_root(temp.path(), &fresh).expect("save");

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 7);
        assert!(
            removed.is_empty(),
            "rows whose restart_generation matches current_generation must NOT be evicted"
        );

        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn invalidate_stale_generation_preserves_unstamped_rows() {
        // Codex review HIGH on PR #2460: normal `InflightTurnState::new`
        // sets `restart_generation = None`. Evicting unstamped rows here
        // would clear every healthy current-generation row at boot.
        // Unstamped rows are preserved; the intake-time staleness threshold
        // path is what bounds genuinely abandoned legacy rows.
        let temp = TempDir::new().unwrap();

        let unstamped = make_state(901, 66);
        assert!(unstamped.restart_generation.is_none());
        save_inflight_state_in_root(temp.path(), &unstamped).expect("save");

        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 4);
        assert!(removed.is_empty());
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
    }

    #[test]
    fn invalidate_stale_generation_empty_dir_is_no_op() {
        let temp = TempDir::new().unwrap();
        let removed = invalidate_stale_generation_in_root(temp.path(), &ProviderKind::Codex, 1);
        assert!(removed.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn invalidate_stale_generation_revalidates_row_under_lock() {
        let temp = TempDir::new().unwrap();

        let mut stale = make_state(951, 77);
        stale.restart_generation = Some(1);
        save_inflight_state_in_root(temp.path(), &stale).expect("save stale");

        let path = inflight_state_path(temp.path(), &ProviderKind::Codex, stale.channel_id);
        let lock = lock_inflight_state_path(&path).unwrap();
        let root = temp.path().to_path_buf();
        let invalidator = std::thread::spawn(move || {
            invalidate_stale_generation_in_root(&root, &ProviderKind::Codex, 2)
        });

        std::thread::sleep(std::time::Duration::from_millis(100));
        let mut fresh = make_state(951, 78);
        fresh.restart_generation = Some(2);
        std::fs::write(&path, serde_json::to_string_pretty(&fresh).unwrap()).unwrap();
        drop(lock);

        let removed = invalidator.join().unwrap();
        assert!(
            removed.is_empty(),
            "fresh same-generation row written before the delete lock was acquired must survive"
        );
        let after = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].restart_generation, Some(2));
        assert_eq!(after[0].user_msg_id, 78);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        CreateNewInflightError, InflightTurnState, latest_request_owner_user_id_for_channel,
        load_inflight_states, load_inflight_states_from_root,
        mark_all_inflight_states_restart_mode, save_inflight_state_create_new_in_root,
        save_inflight_state_in_root, stale_removal_reason,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_inflight_state() {
        let temp = TempDir::new().unwrap();

        let state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].channel_id, 123);
        assert_eq!(loaded[0].current_msg_id, 999);
        assert_eq!(loaded[0].last_offset, 42);
        assert_eq!(loaded[0].turn_start_offset, Some(42));
    }

    #[test]
    fn test_save_and_load_inflight_state_preserves_worktree_metadata() {
        let temp = TempDir::new().unwrap();

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        state.set_worktree_context(
            Some("/tmp/worktree".to_string()),
            Some("agentdesk/codex/adk-cdx".to_string()),
            Some("abc123".to_string()),
        );
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].worktree_path.as_deref(), Some("/tmp/worktree"));
        assert_eq!(
            loaded[0].worktree_branch.as_deref(),
            Some("agentdesk/codex/adk-cdx")
        );
        assert_eq!(loaded[0].base_commit.as_deref(), Some("abc123"));
    }

    #[test]
    fn planned_restart_state_uses_generation_aware_retention() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        state.restart_mode = Some(InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);

        assert!(
            stale_removal_reason(&state, 600, 7).is_none(),
            "current-generation planned restart should survive the normal 300s cleanup window"
        );
        assert!(
            stale_removal_reason(&state, 10, 8)
                .expect("old generation planned restart should be removed")
                .contains("old generation")
        );
    }

    #[test]
    fn latest_request_owner_user_id_prefers_most_recent_state_across_providers() {
        let temp = TempDir::new().unwrap();
        let inflight_root = temp.path().join("runtime").join("discord_inflight");

        let mut claude_state = InflightTurnState::new(
            ProviderKind::Claude,
            123,
            Some("adk-cc".to_string()),
            111,
            789,
            999,
            "hello".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        claude_state.updated_at = "2026-04-11 00:00:00".to_string();
        save_inflight_state_in_root(&inflight_root, &claude_state).unwrap();

        let mut codex_state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            222,
            790,
            1000,
            "world".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        codex_state.updated_at = "2026-04-11 00:00:05".to_string();
        save_inflight_state_in_root(&inflight_root, &codex_state).unwrap();

        let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        let owner = latest_request_owner_user_id_for_channel(123);
        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }

        assert_eq!(owner, Some(222));
    }

    #[test]
    fn mark_all_inflight_states_restart_mode_marks_saved_states() {
        let _lock = super::super::runtime_store::lock_test_env();
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("agentdesk-root");
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        struct EnvReset;
        impl Drop for EnvReset {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }
        let _reset = EnvReset;

        let inflight_root = root.join("runtime").join("discord_inflight");
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            123,
            Some("adk-cdx".to_string()),
            456,
            789,
            999,
            "hello".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            42,
        );
        save_inflight_state_in_root(&inflight_root, &state).unwrap();

        assert_eq!(
            mark_all_inflight_states_restart_mode(
                &ProviderKind::Codex,
                InflightRestartMode::DrainRestart,
            ),
            1
        );

        let states = load_inflight_states(&ProviderKind::Codex);
        assert_eq!(states.len(), 1);
        assert_eq!(
            states[0].restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        assert_eq!(
            states[0].restart_generation,
            Some(super::super::runtime_store::load_generation())
        );
    }

    /// #897 P2 #1: `save_inflight_state_create_new_in_root` must succeed on
    /// a vacant path (atomic create) and reject a second call at the same
    /// path with `AlreadyExists` — this is the guarantee that prevents a
    /// `/api/inflight/rebind` call from overwriting a concurrent live
    /// turn's canonical inflight state.
    #[test]
    fn save_inflight_state_create_new_rejects_existing_path() {
        let temp = TempDir::new().unwrap();
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            1_234_567,
            Some("adk-cdx".to_string()),
            0,
            0,
            0,
            "/api/inflight/rebind".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        save_inflight_state_create_new_in_root(temp.path(), &state)
            .expect("first atomic create must succeed on a vacant path");

        match save_inflight_state_create_new_in_root(temp.path(), &state) {
            Err(CreateNewInflightError::AlreadyExists) => {}
            other => panic!(
                "second atomic create must report AlreadyExists, got {:?}",
                other
            ),
        }
    }

    /// #897 P2 #1: a previously-saved `save_inflight_state_in_root` write
    /// must be observed by `save_inflight_state_create_new_in_root` as
    /// `AlreadyExists`. This is the actual race we need to guard against —
    /// a legitimate turn writes its state via `save_inflight_state`, then a
    /// concurrent rebind call must NOT overwrite it.
    #[test]
    fn save_inflight_state_create_new_rejects_path_written_by_normal_save() {
        let temp = TempDir::new().unwrap();
        let live_turn_state = InflightTurnState::new(
            ProviderKind::Codex,
            9_876_543,
            Some("adk-cdx".to_string()),
            123, // live user
            456, // real user_msg_id
            789, // real current_msg_id
            "real user input".to_string(),
            Some("session-live".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        save_inflight_state_in_root(temp.path(), &live_turn_state)
            .expect("legitimate turn write must succeed");

        let rebind_state = InflightTurnState::new(
            ProviderKind::Codex,
            9_876_543,
            Some("adk-cdx".to_string()),
            0,
            0,
            0,
            "/api/inflight/rebind".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        match save_inflight_state_create_new_in_root(temp.path(), &rebind_state) {
            Err(CreateNewInflightError::AlreadyExists) => {}
            other => panic!("rebind must not overwrite live turn state; got {:?}", other),
        }

        // Canonical live-turn data must survive.
        let states = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].request_owner_user_id, 123);
        assert_eq!(states[0].user_msg_id, 456);
        assert_eq!(states[0].user_text, "real user input");
    }

    /// Relay state contract — invariant **I1**
    /// (`docs/relay-state-contract.md`, `response_sent_offset_in_bounds` +
    /// `response_sent_offset_monotonic`).
    ///
    /// A normal forward-moving save must succeed without tripping either
    /// the bounds or the monotonicity check.
    #[test]
    fn relay_state_contract_i1_response_sent_offset_advances_monotonically() {
        let temp = TempDir::new().unwrap();

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            555,
            Some("adk-cdx".to_string()),
            1,
            2,
            3,
            "user prompt".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.full_response = "hello world".to_string();
        state.response_sent_offset = 5;
        save_inflight_state_in_root(temp.path(), &state).expect("first save");

        state.response_sent_offset = 11; // == full_response.len()
        save_inflight_state_in_root(temp.path(), &state).expect("forward save");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].response_sent_offset, 11);
        assert_eq!(loaded[0].full_response, "hello world");
    }

    /// Relay state contract — invariant **I3**
    /// (`docs/relay-state-contract.md`, `watcher_relay_idempotent`).
    ///
    /// `last_watcher_relayed_offset` must round-trip across save/load so
    /// a replacement watcher (post restart, post replace) initialises its
    /// duplicate-relay guard from the persisted high-water mark.
    #[test]
    fn relay_state_contract_i3_last_watcher_relayed_offset_persists() {
        let temp = TempDir::new().unwrap();

        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            777,
            Some("adk-cdx".to_string()),
            1,
            2,
            3,
            "user prompt".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.last_watcher_relayed_offset = Some(4_096);
        save_inflight_state_in_root(temp.path(), &state).expect("save with watcher offset");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_watcher_relayed_offset, Some(4_096));
    }
}
