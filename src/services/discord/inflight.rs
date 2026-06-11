//! Inflight turn state persistence.
//!
//! `response_sent_offset`, `current_msg_id`, and `last_watcher_relayed_offset`
//! participate in the relay state contract documented in
//! `docs/relay-state-contract.md` (#1222 / #1224). Any change that touches
//! relay producers/consumers must keep the invariants there satisfied.

pub(in crate::services::discord) mod budget;

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
// version change, so new→old rollbacks could read rows whose FIFO synthesis
// was elided for ClaudeTui and reject recovery with a misleading "input fifo
// path missing" notice. v8 marks the shape shipping the compat-fixed
// `input_fifo_path` alongside ClaudeTui plus the silent-skip recovery branch;
// old binaries deserialize v8 rows via `#[serde(default)]` (compat window:
// one release each direction).
const INFLIGHT_STATE_VERSION: u32 = 8;
const INFLIGHT_MAX_AGE_SECS: u64 = 300; // 5 minutes
const DRAIN_RESTART_MAX_AGE_SECS: u64 = 1800; // 30 minutes
const HOT_SWAP_HANDOFF_MAX_AGE_SECS: u64 = 900; // 15 minutes
/// #3293: restarts-with-failed-terminal-relay budget. `recovery_relay_attempts`
/// grows at most once per boot (recovery runs once per boot), so this is a
/// "3 consecutive restarts" budget, not a per-process retry cap.
pub(super) const RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET: u32 = 3;

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
    /// #3293: count of restarts whose recovery terminal relay failed
    /// transiently for this row. Additive `#[serde(default)]` field —
    /// no `INFLIGHT_STATE_VERSION` bump per the #2235 compat convention.
    #[serde(default)]
    pub recovery_relay_attempts: u32,
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
    /// #3099 codex re-review (P2): the Discord notify-bot message id that carries
    /// this turn's `⏳` reaction, when the turn was injected with a
    /// `user_msg_id == 0` (no anchored Discord *user* message — e.g. a
    /// `<task-notification>` auto-turn). The `⏳ → ✅` completion cleanup for such
    /// turns previously read whatever was in the single shared prompt-anchor slot
    /// (`prompt_anchor_by_tmux`), so a *later* injection that overwrote that slot
    /// would steal the cleanup: turn A's completion would `⏳→✅` turn B's message
    /// while B was still running, and A's `⏳` would never clear. Pinning the
    /// injected message id on the inflight row makes each turn clean up its OWN
    /// message regardless of how the shared slot has since been overwritten.
    /// `None` for turns with a real `user_msg_id` or legacy rows.
    #[serde(default)]
    pub injected_prompt_message_id: Option<u64>,
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
    // #3034: wire-contract surface pinned by the unit tests below; not yet read
    // by a live audit/metrics callsite.
    #[allow(dead_code)]
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
            recovery_relay_attempts: 0,
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
            injected_prompt_message_id: None,
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
    /// #3041 P1-3 (codex P1-3 issue 2): the turn's `turn_start_offset` — the JSONL
    /// byte offset at which this turn began. Used to disambiguate two consecutive
    /// `user_msg_id == 0` TUI-direct turns whose `started_at` collides because
    /// `now_string` only has 1-second resolution. Monotonic per turn, so it makes
    /// the frame-carried turn identity unique even within a single second.
    pub turn_start_offset: Option<u64>,
}

impl InflightTurnIdentity {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            user_msg_id: state.user_msg_id,
            started_at: state.started_at.clone(),
            tmux_session_name: state.tmux_session_name.clone(),
            turn_start_offset: state.turn_start_offset,
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

    // #3154 — OBSERVE-ONLY on the bridge/watcher save path. A legit fresh-turn
    // reset (different user_msg_id or turn_start_offset) resets
    // response_sent_offset to 0 on purpose (see InflightTurnState::new), so the
    // check is gated by SAME turn identity; only a backward move within the same
    // turn is a violation. We do not skip the write here (that would drop a
    // legit fresh turn); this mirrors the last_offset_monotonic precedent below.
    let same_turn_identity = existing.user_msg_id == state.user_msg_id
        && existing.turn_start_offset == state.turn_start_offset;
    let monotonic_offset =
        !same_turn_identity || state.response_sent_offset >= existing.response_sent_offset;
    record_inflight_invariant(
        monotonic_offset,
        state,
        "response_sent_offset_monotonic",
        code_location,
        "inflight response_sent_offset must not move backwards for the same turn identity",
        serde_json::json!({
            "previous": existing.response_sent_offset,
            "next": state.response_sent_offset,
            "same_turn_identity": same_turn_identity,
            "path": path.display().to_string(),
        }),
    );
    debug_assert!(
        monotonic_offset,
        "inflight response_sent_offset must not move backwards for the same turn identity"
    );

    // I6 (last_offset_monotonic) — OBSERVE-ONLY on the bridge/watcher save
    // path. A legit fresh-turn reset (different user_msg_id or
    // turn_start_offset) lowers last_offset on purpose, so the check is gated
    // by SAME turn identity; only a backward move within the same turn is a
    // violation. We do not skip the write here (that would drop a legit fresh
    // turn); the enforcing variant lives in the standby/refresh path.
    let last_offset_monotonic = !same_turn_identity || state.last_offset >= existing.last_offset;
    record_inflight_invariant(
        last_offset_monotonic,
        state,
        "last_offset_monotonic",
        code_location,
        "inflight last_offset must not move backwards for the same turn identity",
        serde_json::json!({
            "previous": existing.last_offset,
            "next": state.last_offset,
            "same_turn_identity": same_turn_identity,
            "path": path.display().to_string(),
        }),
    );
    debug_assert!(
        last_offset_monotonic,
        "inflight last_offset must not move backwards for the same turn identity"
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

/// #3107 codex re-review (P1): atomic compare-and-set save. Writes `state`
/// ONLY when no inflight row currently exists for `(provider, channel_id)`,
/// returning `true` iff it wrote.
///
/// The watcher self-heal re-acquire (`reacquire_watcher_inflight_for_active_stream`)
/// previously did a non-atomic `load_inflight_state(...).is_some()` preflight
/// followed by an unconditional `save_inflight_state`. Between the check and
/// the save the Discord intake path could create a REAL inflight for a brand
/// new user turn on the same `(provider, channel_id)`; the synthetic
/// `user_msg_id = 0` re-acquire save would then clobber it and the legitimate
/// turn would be lost.
///
/// This helper closes that window by performing the existence check AND the
/// write while holding the same `lock_inflight_state_path` sidecar flock that
/// `save_inflight_state_in_root` / `clear_inflight_state*` already serialize
/// on. A concurrent intake `save_inflight_state` either ran before us (we see
/// its row → no-op, intake wins) or after us (it overwrites our synthetic row
/// with the real turn → intake still wins). The synthetic row is therefore
/// only ever written when there is genuinely no inflight at the moment of the
/// atomic write.
pub(super) fn save_inflight_state_if_absent(state: &InflightTurnState) -> Result<bool, String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_state_if_absent_in_root(&root, state)
}

fn save_inflight_state_if_absent_in_root(
    root: &Path,
    state: &InflightTurnState,
) -> Result<bool, String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Hold the sidecar flock across the existence check AND the write so a
    // concurrent intake `save_inflight_state_in_root` (which takes the same
    // lock) cannot land a real inflight in the gap. `path.exists()` under the
    // lock is the compare; `atomic_write` is the set.
    let _lock = lock_inflight_state_path(&path)?;
    if path.exists() {
        return Ok(false);
    }
    validate_inflight_state_for_save(
        root,
        &path,
        state,
        "src/services/discord/inflight.rs:save_inflight_state_if_absent_in_root",
    );
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    Ok(true)
}

/// Outcome of [`save_inflight_state_if_matches_identity`] — the #3041 P1-2 R3
/// identity-guarded re-save used on a delivery-lease `Skip` epilogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum GuardedSaveOutcome {
    /// On-disk row still matched the turn identity; the row was rewritten.
    Saved,
    /// No inflight row existed (the lease HOLDER already cleared it on its
    /// success path). We do NOT resurrect it — the turn is already delivered.
    Missing,
    /// A row existed but its identity did NOT match (a newer turn replaced it,
    /// or a planned-restart / rebind-origin marker now owns the row). We do
    /// NOT clobber it.
    IdentityMismatch,
    /// Filesystem / serialization error during the write.
    IoError,
}

/// #3041 P1-2 (codex P1-2 R3): identity-guarded re-save for the bridge's
/// delivery-lease `Skip` epilogue. On a Skip the live HOLDER (the watcher)
/// owns this turn's inflight lifecycle and CLEARS the row on its own success.
/// The bridge's epilogue must therefore NOT blindly `save_inflight_state`: if
/// the holder's clear (a `remove_file` under the same sidecar lock) won the
/// race and the bridge's blind re-save ran second, it would resurrect a STALE
/// inflight row for an already-delivered turn — recovery then sees it as
/// delivered and returns WITHOUT clearing, leaking the row indefinitely.
///
/// This helper closes that window the same way `clear_inflight_state_if_matches`
/// (#2427 D-wire) does: read the on-disk row under the lock and only write when
/// it is STILL present AND its `(user_msg_id, started_at, tmux_session_name)`
/// identity (plus `turn_start_offset`, when known) matches the turn the bridge
/// is preserving. If the row is gone (holder delivered → `Missing`) or has been
/// replaced by a newer turn / restart-or-rebind marker (`IdentityMismatch`), we
/// no-op instead of resurrecting. When the holder FAILED and did NOT clear, the
/// row is still present + matching, so we refresh it (`Saved`) and retry
/// survives. Windows-safe: the `lock_inflight_state_path` sidecar flock + the
/// `atomic_write` rename are the same primitives the rest of the module uses.
pub(in crate::services::discord) fn save_inflight_state_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

/// Root-explicit inner form of [`save_inflight_state_if_matches_identity`] for
/// unit tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(super) fn save_inflight_state_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    // Hold the sidecar flock across the read AND the write so a concurrent
    // holder `clear_inflight_state` (which takes the same lock) cannot land its
    // remove between our identity check and our write.
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    // Holder already cleared the row on its success path → do NOT resurrect.
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        // Malformed row: treat like a mismatch and do not clobber — the loader
        // eviction path GCs malformed payloads on the next read.
        return GuardedSaveOutcome::IdentityMismatch;
    };
    // A newer turn (different identity) or a planned-restart / rebind-origin
    // marker now owns the row — never overwrite it with this preserved turn.
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    validate_inflight_state_for_save(
        root,
        &path,
        state,
        "src/services/discord/inflight.rs:save_inflight_state_if_matches_identity_in_root",
    );
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

// ---------------------------------------------------------------------------
// #3077: typed status-panel ownership writes.
//
// `status_message_id` is the de-facto "this turn owns status panel M" pointer,
// and several independent actors (turn-bridge completion fallback, tmux watcher
// TUI-direct publish/orphan-cleanup, placeholder sweeper) used to mutate it via
// a non-atomic `load_inflight_state(...)` → `state.status_message_id = …` →
// `save_inflight_state(...)` triple. Because the read and the write were not
// serialized against each other, a newer turn that rebound the panel between a
// stale actor's load and its blind `= None` could have its panel silently
// orphaned (the #3099/#3100/#3105/#3107 panel-lifecycle race family).
//
// These helpers centralize that read-modify-write behind intentful operations
// that hold the same `lock_inflight_state_path` sidecar flock across the whole
// compare-and-set, exactly like `save_inflight_state_if_absent`. Callers no
// longer touch the field directly; they declare *what* they own and *under
// which precondition*, and the store enforces it atomically.

/// Per-turn precondition for `bind_status_panel`. Lets each caller carry its
/// own ownership invariant into the lock-held read-modify-write so the guard
/// check and the write cannot be split by a concurrent writer (TOCTOU).
#[derive(Debug, Clone, Default)]
pub(in crate::services::discord) struct StatusPanelBindGuard {
    /// Bind only when the on-disk row still belongs to this `user_msg_id`.
    /// `None` means "do not guard on user_msg_id" (used by callers that have
    /// already established identity another way). Mirrors the turn-bridge
    /// status-panel-v2 completion fallback guard.
    pub require_user_msg_id: Option<u64>,
    /// Bind only when the on-disk row still matches this full turn identity
    /// (user_msg_id + started_at + tmux_session_name). Mirrors the tmux
    /// watcher TUI-direct publish guard that defeats turn handoff during the
    /// Discord send await.
    pub require_identity: Option<InflightTurnIdentity>,
    /// When true, do not overwrite a real (non-synthetic) panel id already on
    /// the row — an overlapping actor already published the canonical panel
    /// and our send is a duplicate (#3003 reclaim path). Synthetic-headless
    /// ids do not count as "already set".
    pub skip_if_panel_already_set: bool,
}

/// Outcome of a `bind_status_panel` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum StatusPanelBindOutcome {
    /// The row was found, passed the guard, and now carries `msg_id`.
    Bound,
    /// The row already carried `msg_id`; nothing was written.
    AlreadyBound,
    /// The row exists but a DIFFERENT real panel id is already set and
    /// `skip_if_panel_already_set` was requested — left untouched. Carries the
    /// row's currently-owned (real) panel id as observed under the same flock,
    /// so the caller can adopt the row's actual panel without a second
    /// (racy) re-read of the inflight row.
    SkippedPanelAlreadySet(u64),
    /// No inflight row exists for `(provider, channel_id)`.
    Missing,
    /// The on-disk row did not satisfy `require_user_msg_id` /
    /// `require_identity` — left untouched (a different turn now owns the row).
    GuardMismatch,
    /// Filesystem / serialization failure while persisting the bind.
    IoError,
}

/// Intentful "this turn now owns status panel `msg_id`" write. Performs the
/// guard check and the field set atomically under the inflight sidecar flock,
/// so it is consistent with `save_inflight_state` / `save_inflight_state_if_absent`.
pub(in crate::services::discord) fn bind_status_panel(
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelBindGuard,
) -> StatusPanelBindOutcome {
    let Some(root) = inflight_runtime_root() else {
        return StatusPanelBindOutcome::IoError;
    };
    bind_status_panel_in_root(&root, provider, channel_id, msg_id, guard)
}

fn bind_status_panel_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelBindGuard,
) -> StatusPanelBindOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return StatusPanelBindOutcome::IoError;
    }
    // Hold the sidecar flock across load → guard → set so a concurrent
    // writer cannot land between the guard check and the write.
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return StatusPanelBindOutcome::IoError;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return StatusPanelBindOutcome::Missing;
    };
    if let Some(expected) = guard.require_user_msg_id
        && state.user_msg_id != expected
    {
        return StatusPanelBindOutcome::GuardMismatch;
    }
    if let Some(expected) = guard.require_identity.as_ref()
        && !expected.matches_state(&state)
    {
        return StatusPanelBindOutcome::GuardMismatch;
    }
    // Same-id re-bind is idempotent and must classify as `AlreadyBound`
    // REGARDLESS of `skip_if_panel_already_set` — an idempotent re-bind of the
    // panel this row already owns is a no-op, not a "duplicate skip". Checking
    // the skip flag first (#3077 codex P2 #1) misclassified a same-id re-bind as
    // `SkippedPanelAlreadySet`, which the TUI-direct caller then routed to a
    // DELETE of the row's own already-bound panel. Order: same-id → AlreadyBound;
    // else a DIFFERENT real id already set + skip flag → SkippedPanelAlreadySet.
    if state.status_message_id == Some(msg_id) {
        return StatusPanelBindOutcome::AlreadyBound;
    }
    if guard.skip_if_panel_already_set && status_panel_id_is_real(state.status_message_id) {
        // Safe: `status_panel_id_is_real` only returns true for `Some(real)`,
        // and we already handled `Some(msg_id)` above, so this is a DIFFERENT
        // real panel id. Carry it so the caller adopts the row's owned panel.
        return StatusPanelBindOutcome::SkippedPanelAlreadySet(
            state.status_message_id.unwrap_or_default(),
        );
    }
    state.status_message_id = Some(msg_id);
    match persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:bind_status_panel_in_root",
    ) {
        Ok(()) => StatusPanelBindOutcome::Bound,
        Err(_) => StatusPanelBindOutcome::IoError,
    }
}

/// Per-turn precondition for `clear_status_panel_if_current`. The msg-id
/// compare-and-clear is unconditional; these add the caller's extra ownership
/// guards (so a sweeper/cleanup that loaded a stale snapshot does not clear a
/// row a newer turn already advanced).
#[derive(Debug, Clone, Default)]
pub(in crate::services::discord) struct StatusPanelClearGuard {
    /// Clear only when the on-disk row still belongs to this `user_msg_id`.
    pub require_user_msg_id: Option<u64>,
    /// Clear only when the on-disk row still carries this `current_msg_id`
    /// (placeholder sweeper convergence guard).
    pub require_current_msg_id: Option<u64>,
    /// Clear only when the on-disk row's tmux session still matches (watcher
    /// orphan-cleanup guard).
    pub require_tmux_session_name: Option<String>,
}

/// Compare-and-clear: clears `status_message_id` ONLY when it currently equals
/// `msg_id` (and every guard precondition holds). Returns `true` iff it
/// cleared. This is the #3077 hardening — a blind `= None` becomes a
/// compare-and-clear so a panel a *newer* turn rebound is never wiped by a
/// stale actor that loaded an older snapshot.
pub(in crate::services::discord) fn clear_status_panel_if_current(
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelClearGuard,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    clear_status_panel_if_current_in_root(&root, provider, channel_id, msg_id, guard)
}

fn clear_status_panel_if_current_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    guard: &StatusPanelClearGuard,
) -> bool {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return false;
    };
    if state.status_message_id != Some(msg_id) {
        return false;
    }
    if let Some(expected) = guard.require_user_msg_id
        && state.user_msg_id != expected
    {
        return false;
    }
    if let Some(expected) = guard.require_current_msg_id
        && state.current_msg_id != expected
    {
        return false;
    }
    if let Some(expected) = guard.require_tmux_session_name.as_deref()
        && state.tmux_session_name.as_deref() != Some(expected)
    {
        return false;
    }
    state.status_message_id = None;
    persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:clear_status_panel_if_current_in_root",
    )
    .is_ok()
}

/// #3351: compare-and-clear for the persisted relay-placeholder id
/// (`current_msg_id`), mirroring `clear_status_panel_if_current` (#3077): only
/// the exact `msg_id` is cleared, a placeholderless Discord turn
/// (`current_msg_id == user_msg_id`) is never touched, and the optional tmux
/// guard restricts the clear to the caller's own session row. `current_msg_id
/// == 0` is a legitimate "no anchored placeholder" state, so clearing to 0 is
/// safe.
pub(in crate::services::discord) fn clear_current_msg_if_matches(
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    require_tmux_session_name: Option<&str>,
) -> bool {
    let Some(root) = inflight_runtime_root() else {
        return false;
    };
    clear_current_msg_if_matches_in_root(
        &root,
        provider,
        channel_id,
        msg_id,
        require_tmux_session_name,
    )
}

fn clear_current_msg_if_matches_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    msg_id: u64,
    require_tmux_session_name: Option<&str>,
) -> bool {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return false;
    };
    let Some(mut state) = load_inflight_state_unlocked(&path) else {
        return false;
    };
    if msg_id == 0 || state.current_msg_id != msg_id {
        // A newer turn already advanced `current_msg_id` (or there is nothing
        // to compare) — never wipe another turn's anchor.
        return false;
    }
    if state.user_msg_id != 0 && state.current_msg_id == state.user_msg_id {
        // Placeholderless Discord turn: `current_msg_id` mirrors the user's own
        // message id (adopt-guard mirror) — not a relay placeholder.
        return false;
    }
    if let Some(expected) = require_tmux_session_name
        && state.tmux_session_name.as_deref() != Some(expected)
    {
        return false;
    }
    state.current_msg_id = 0;
    persist_under_lock(
        root,
        &path,
        &state,
        "src/services/discord/inflight.rs:clear_current_msg_if_matches_in_root",
    )
    .is_ok()
}

/// `true` when `id` is a real Discord panel id (present and not a synthetic
/// headless placeholder). Mirrors `turn_bridge::normalize_status_panel_message_id`
/// without pulling in the serenity `MessageId` newtype.
fn status_panel_id_is_real(id: Option<u64>) -> bool {
    match id {
        Some(value) => !super::is_synthetic_headless_message_id_raw(value),
        None => false,
    }
}

/// Reads + deserializes the inflight row at `path` while the caller holds the
/// sidecar flock. Returns `None` on a missing/malformed file (same lenient
/// posture as `load_inflight_state`).
fn load_inflight_state_unlocked(path: &Path) -> Option<InflightTurnState> {
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Shared lock-held persist tail: validate, stamp `updated_at`, atomic-write.
/// Caller must already hold `lock_inflight_state_path`.
fn persist_under_lock(
    root: &Path,
    path: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> Result<(), String> {
    validate_inflight_state_for_save(root, path, state, caller);
    let mut updated = state.clone();
    updated.updated_at = now_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(path, &json)
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

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn refresh_inflight_last_offset_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    output_path: &str,
    expected_current_msg_id: Option<u64>,
    last_offset: u64,
    caller_owner: RelayOwnerKind,
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
        caller_owner,
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

/// #3161 (codex P1): zero-id epilogue/guard cleanup that is STILL identity-safe.
///
/// A zero-id turn (recovery / external-input / cluster-relay synthesized;
/// zero-normalized at [`optional_message_id`]) cannot be authenticated against a
/// non-zero `expected_user_msg_id`, so [`clear_inflight_state_if_matches`]
/// deliberately refuses (`expected_user_msg_id == 0` → `UserMsgMismatch`) to
/// avoid blind-deleting a row it cannot prove ownership of. But a zero-id turn
/// still legitimately owns *its own* row (whose on-disk `user_msg_id` is also 0)
/// and must clean it up — recovery cleanup depends on this.
///
/// This helper closes that gap: it clears ONLY when the on-disk row's
/// `user_msg_id` is itself 0 (a genuine zero-id-owned row). If a NEWER real
/// (non-zero) identity turn has since written its row, the on-disk
/// `user_msg_id != 0` and we return `UserMsgMismatch` — preserving the newer
/// owner so its status panel can still complete. Planned-restart markers and
/// rebind origins are preserved exactly like the non-zero guarded clear.
pub(crate) fn clear_inflight_state_if_matches_zero_owned(
    provider: &ProviderKind,
    channel_id: u64,
) -> GuardedClearOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedClearOutcome::Missing;
    };
    clear_inflight_state_if_matches_zero_owned_in_root(&root, provider, channel_id)
}

/// Root-explicit variant of [`clear_inflight_state_if_matches_zero_owned`] for
/// unit tests.
pub(super) fn clear_inflight_state_if_matches_zero_owned_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
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
    // The only thing a zero-id turn may clear is a zero-id-owned row. A newer
    // non-zero owner has `user_msg_id != 0` → preserve it.
    if state.user_msg_id != 0 {
        return GuardedClearOutcome::UserMsgMismatch;
    }
    match fs::remove_file(&path) {
        Ok(()) => GuardedClearOutcome::Cleared,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => GuardedClearOutcome::Missing,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight zero-owned guarded-clear remove_file failed; treating as IoError so sweeper retries"
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

// #3034: root-parameterized variant exercised directly by the unit tests
// (the prod wrapper was removed; tests drive a tempdir root). Test-only seam.
#[allow(dead_code)]
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

#[allow(clippy::too_many_arguments)]
fn refresh_inflight_last_offset_if_matches_identity_in_root(
    root: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    output_path: &str,
    expected_current_msg_id: Option<u64>,
    last_offset: u64,
    caller_owner: RelayOwnerKind,
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

    // I6 (last_offset_owner_gated): the persisted watermark is advanced only
    // by the current relay owner. A non-owner caller (standby/idle) follows
    // the authoritative offset read-only and must yield to a live owner. The
    // identity guards above already proved this is the SAME turn, so a live
    // owner that differs from the caller is an authority conflict, not a
    // fresh-turn reset.
    let persisted_owner = state.effective_relay_owner_kind();
    let owner_is_live = !matches!(persisted_owner, RelayOwnerKind::None);
    if owner_is_live && persisted_owner != caller_owner {
        record_inflight_invariant(
            false,
            &state,
            "last_offset_owner_gated",
            "src/services/discord/inflight.rs:refresh_inflight_last_offset_if_matches_identity_in_root",
            "inflight last_offset must only be advanced by the current relay owner",
            serde_json::json!({
                "persisted_owner": persisted_owner.as_str(),
                "caller_owner": caller_owner.as_str(),
                "previous": state.last_offset,
                "next": last_offset,
                "path": path.display().to_string(),
            }),
        );
        return false;
    }

    // I6 (last_offset_monotonic): same identity, so a backward watermark write
    // would clobber the authoritative offset and replay a stale transcript
    // tail. Reject and record. A fresh-turn reset is already excluded by the
    // identity guards above.
    if last_offset < state.last_offset {
        record_inflight_invariant(
            false,
            &state,
            "last_offset_monotonic",
            "src/services/discord/inflight.rs:refresh_inflight_last_offset_if_matches_identity_in_root",
            "inflight last_offset must not move backwards for the same turn identity",
            serde_json::json!({
                "previous": state.last_offset,
                "next": last_offset,
                "path": path.display().to_string(),
            }),
        );
        return false;
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
        GuardedClearOutcome, GuardedSaveOutcome, INFLIGHT_STALENESS_THRESHOLD_SECS,
        InflightRestartMode, InflightTurnIdentity, InflightTurnState, RelayOwnerKind,
        StatusPanelBindGuard, StatusPanelBindOutcome, StatusPanelClearGuard,
        bind_status_panel_in_root, clear_current_msg_if_matches_in_root,
        clear_inflight_state_if_matches_identity_after_delivery_in_root,
        clear_inflight_state_if_matches_identity_in_root, clear_inflight_state_if_matches_in_root,
        clear_inflight_state_if_matches_tmux_response_in_root,
        clear_inflight_state_if_matches_zero_owned_in_root, clear_status_panel_if_current_in_root,
        inflight_state_allows_idle_tmux_repair_state, inflight_state_is_stale, inflight_state_path,
        load_inflight_states_from_root, lock_inflight_state_path, normalize_response_sent_offset,
        refresh_inflight_last_offset_if_matches_identity_in_root,
        save_inflight_state_if_matches_identity_in_root, save_inflight_state_in_root,
        validate_inflight_state_for_save,
    };
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::provider::ProviderKind;
    use chrono::TimeZone;
    use std::path::Path;
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

    // ---- #3077: typed status-panel ownership write tests ----

    /// Seeds a single inflight row in `root` and returns it. `user_msg_id` /
    /// `current_msg_id` / `status_message_id` are caller-controlled so the
    /// guard semantics can be exercised.
    fn seed_status_panel_state(
        root: &Path,
        channel_id: u64,
        user_msg_id: u64,
        current_msg_id: u64,
        tmux_session_name: Option<&str>,
        status_message_id: Option<u64>,
    ) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-claude".to_string()),
            user_msg_id,
            user_msg_id,
            current_msg_id,
            "hello".to_string(),
            Some("session-1".to_string()),
            tmux_session_name.map(str::to_string),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        // `new()` takes (.., request_owner_user_id, user_msg_id, current_msg_id, ..);
        // pin the guard-relevant fields explicitly so the test intent is exact.
        state.user_msg_id = user_msg_id;
        state.current_msg_id = current_msg_id;
        state.status_message_id = status_message_id;
        save_inflight_state_in_root(root, &state).expect("seed inflight state");
        state
    }

    fn loaded_status_message_id(root: &Path, channel_id: u64) -> Option<u64> {
        load_inflight_states_from_root(root, &ProviderKind::Claude)
            .into_iter()
            .find(|s| s.channel_id == channel_id)
            .and_then(|s| s.status_message_id)
    }

    #[test]
    fn bind_status_panel_sets_id_when_unguarded() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(temp.path(), 7001, 10, 11, Some("AgentDesk-claude-a"), None);

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7001,
            5555,
            &StatusPanelBindGuard::default(),
        );

        assert_eq!(outcome, StatusPanelBindOutcome::Bound);
        assert_eq!(loaded_status_message_id(temp.path(), 7001), Some(5555));
    }

    #[test]
    fn bind_status_panel_is_idempotent_when_already_bound() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7002,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7002,
            5555,
            &StatusPanelBindGuard::default(),
        );

        assert_eq!(outcome, StatusPanelBindOutcome::AlreadyBound);
        assert_eq!(loaded_status_message_id(temp.path(), 7002), Some(5555));
    }

    #[test]
    fn bind_status_panel_respects_user_msg_id_guard() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(temp.path(), 7003, 10, 11, Some("AgentDesk-claude-a"), None);

        // Guard expects a different user_msg_id (a newer turn now owns the row).
        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7003,
            5555,
            &StatusPanelBindGuard {
                require_user_msg_id: Some(99),
                ..Default::default()
            },
        );

        assert_eq!(outcome, StatusPanelBindOutcome::GuardMismatch);
        assert_eq!(loaded_status_message_id(temp.path(), 7003), None);
    }

    #[test]
    fn bind_status_panel_skips_when_real_panel_already_set() {
        let temp = TempDir::new().unwrap();
        // A real (non-synthetic) panel id already on the row.
        seed_status_panel_state(
            temp.path(),
            7004,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(4242),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7004,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        // Carries the row's owned (DIFFERENT) panel id so the caller can adopt it.
        assert_eq!(
            outcome,
            StatusPanelBindOutcome::SkippedPanelAlreadySet(4242)
        );
        // Canonical panel preserved — not overwritten by the duplicate.
        assert_eq!(loaded_status_message_id(temp.path(), 7004), Some(4242));
    }

    #[test]
    fn bind_status_panel_same_id_is_already_bound_even_with_skip_flag() {
        // #3077 (codex P2 #1): an idempotent re-bind of the SAME panel id the row
        // already owns must classify as `AlreadyBound`, NOT
        // `SkippedPanelAlreadySet`, even when `skip_if_panel_already_set` is set.
        // Misclassifying it routed the TUI-direct caller to DELETE its own panel.
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7007,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7007,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        assert_eq!(outcome, StatusPanelBindOutcome::AlreadyBound);
        assert_eq!(loaded_status_message_id(temp.path(), 7007), Some(5555));
    }

    #[test]
    fn bind_status_panel_different_id_skips_and_reports_owned_id() {
        // A DIFFERENT real panel id already set + skip flag → SkippedPanelAlreadySet
        // carrying the row's owned id (so the caller adopts the real panel).
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7008,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(4242),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7008,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        assert_eq!(
            outcome,
            StatusPanelBindOutcome::SkippedPanelAlreadySet(4242)
        );
        assert_eq!(loaded_status_message_id(temp.path(), 7008), Some(4242));
    }

    #[test]
    fn bind_status_panel_overwrites_synthetic_even_with_skip_flag() {
        let temp = TempDir::new().unwrap();
        // A synthetic-headless id does NOT count as "already set".
        seed_status_panel_state(
            temp.path(),
            7005,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(crate::services::discord::SYNTHETIC_HEADLESS_MESSAGE_ID_FLOOR + 1),
        );

        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7005,
            5555,
            &StatusPanelBindGuard {
                skip_if_panel_already_set: true,
                ..Default::default()
            },
        );

        assert_eq!(outcome, StatusPanelBindOutcome::Bound);
        assert_eq!(loaded_status_message_id(temp.path(), 7005), Some(5555));
    }

    #[test]
    fn bind_status_panel_missing_row_reports_missing() {
        let temp = TempDir::new().unwrap();
        let outcome = bind_status_panel_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7006,
            5555,
            &StatusPanelBindGuard::default(),
        );
        assert_eq!(outcome, StatusPanelBindOutcome::Missing);
    }

    #[test]
    fn clear_status_panel_if_current_clears_on_match() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7101,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7101,
            5555,
            &StatusPanelClearGuard::default(),
        );

        assert!(cleared);
        assert_eq!(loaded_status_message_id(temp.path(), 7101), None);
    }

    #[test]
    fn clear_status_panel_if_current_preserves_newer_turns_panel_on_mismatch() {
        let temp = TempDir::new().unwrap();
        // A newer turn already rebound the panel to 9999; a stale actor still
        // believes it owns 5555 and asks to clear it. The compare-and-clear
        // must NOT wipe the newer turn's panel.
        seed_status_panel_state(
            temp.path(),
            7102,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(9999),
        );

        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7102,
            5555,
            &StatusPanelClearGuard::default(),
        );

        assert!(!cleared);
        assert_eq!(loaded_status_message_id(temp.path(), 7102), Some(9999));
    }

    #[test]
    fn clear_status_panel_if_current_respects_extra_guards() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7103,
            10,
            11,
            Some("AgentDesk-claude-a"),
            Some(5555),
        );

        // msg-id matches, but user_msg_id/current_msg_id/tmux guards point at a
        // different turn → must NOT clear.
        let user_mismatch = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_user_msg_id: Some(99),
                ..Default::default()
            },
        );
        assert!(!user_mismatch);

        let current_mismatch = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_current_msg_id: Some(99),
                ..Default::default()
            },
        );
        assert!(!current_mismatch);

        let tmux_mismatch = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_tmux_session_name: Some("AgentDesk-claude-other".to_string()),
                ..Default::default()
            },
        );
        assert!(!tmux_mismatch);

        assert_eq!(loaded_status_message_id(temp.path(), 7103), Some(5555));

        // All guards satisfied → clears.
        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7103,
            5555,
            &StatusPanelClearGuard {
                require_user_msg_id: Some(10),
                require_current_msg_id: Some(11),
                require_tmux_session_name: Some("AgentDesk-claude-a".to_string()),
            },
        );
        assert!(cleared);
        assert_eq!(loaded_status_message_id(temp.path(), 7103), None);
    }

    // ---- #3351: relay-placeholder (`current_msg_id`) compare-and-clear ----

    fn loaded_current_msg_id(root: &Path, channel_id: u64) -> Option<u64> {
        load_inflight_states_from_root(root, &ProviderKind::Claude)
            .into_iter()
            .find(|s| s.channel_id == channel_id)
            .map(|s| s.current_msg_id)
    }

    #[test]
    fn clear_current_msg_if_matches_clears_on_match() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7201,
            10,
            5555,
            Some("AgentDesk-claude-a"),
            None,
        );

        let cleared = clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7201,
            5555,
            Some("AgentDesk-claude-a"),
        );

        assert!(cleared);
        assert_eq!(loaded_current_msg_id(temp.path(), 7201), Some(0));
    }

    #[test]
    fn clear_current_msg_if_matches_preserves_newer_turn_on_mismatch() {
        let temp = TempDir::new().unwrap();
        // A newer turn already advanced `current_msg_id` to 9999; a stale actor
        // asking to clear 5555 must not touch it. A zero msg_id never matches.
        seed_status_panel_state(
            temp.path(),
            7202,
            10,
            9999,
            Some("AgentDesk-claude-a"),
            None,
        );

        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7202,
            5555,
            None,
        ));
        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7202,
            0,
            None,
        ));
        assert_eq!(loaded_current_msg_id(temp.path(), 7202), Some(9999));
    }

    #[test]
    fn clear_current_msg_if_matches_respects_tmux_guard() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(
            temp.path(),
            7203,
            10,
            5555,
            Some("AgentDesk-claude-a"),
            None,
        );

        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7203,
            5555,
            Some("AgentDesk-claude-other"),
        ));
        assert_eq!(loaded_current_msg_id(temp.path(), 7203), Some(5555));
    }

    #[test]
    fn clear_current_msg_if_matches_preserves_placeholderless_turn() {
        let temp = TempDir::new().unwrap();
        // Placeholderless Discord turn: `current_msg_id` mirrors the user's own
        // message id — never clear it (adopt-guard mirror).
        seed_status_panel_state(
            temp.path(),
            7204,
            5555,
            5555,
            Some("AgentDesk-claude-a"),
            None,
        );

        assert!(!clear_current_msg_if_matches_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7204,
            5555,
            Some("AgentDesk-claude-a"),
        ));
        assert_eq!(loaded_current_msg_id(temp.path(), 7204), Some(5555));
    }

    #[test]
    fn clear_status_panel_if_current_noops_on_missing_row() {
        let temp = TempDir::new().unwrap();
        let cleared = clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7104,
            5555,
            &StatusPanelClearGuard::default(),
        );
        assert!(!cleared);
    }

    #[test]
    fn bind_then_clear_if_current_round_trips_atomically() {
        let temp = TempDir::new().unwrap();
        seed_status_panel_state(temp.path(), 7200, 10, 11, Some("AgentDesk-claude-a"), None);

        // bind then clear-if-current with the same id returns the row to None,
        // mirroring the watcher publish → orphan-cleanup lifecycle through the
        // single locked store path.
        assert_eq!(
            bind_status_panel_in_root(
                temp.path(),
                &ProviderKind::Claude,
                7200,
                5555,
                &StatusPanelBindGuard::default(),
            ),
            StatusPanelBindOutcome::Bound
        );
        assert_eq!(loaded_status_message_id(temp.path(), 7200), Some(5555));

        assert!(clear_status_panel_if_current_in_root(
            temp.path(),
            &ProviderKind::Claude,
            7200,
            5555,
            &StatusPanelClearGuard::default(),
        ));
        assert_eq!(loaded_status_message_id(temp.path(), 7200), None);
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

    /// #3041 P1-3 (Part a, B1): the identity-guarded save must NOT let a stale write
    /// clobber a NEWER turn that has taken over the inflight row (e.g. a fast
    /// follow-up turn on the same channel between the watcher's compute and its
    /// write). A mismatched identity yields `IdentityMismatch` and the newer turn's
    /// row is preserved. (The frame-carried B1 commit fence removed the racy
    /// delegated-terminal-end inflight persist; this keeps the generic guard covered
    /// via a still-live field.)
    #[test]
    fn identity_guarded_save_rejects_stale_write_against_newer_turn() {
        let temp = TempDir::new().unwrap();
        // The original turn (user_msg_id = 100).
        let mut original = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        original.user_msg_id = 100;
        let original_identity = InflightTurnIdentity::from_state(&original);

        // A NEWER turn (distinct user_msg_id) now owns the row on disk.
        let mut newer = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 200);
        newer.user_msg_id = 200;
        save_inflight_state_in_root(temp.path(), &newer).unwrap();

        // Stale write under the OLD identity → must be rejected, leaving the newer
        // turn intact.
        let mut stale_persist = original.clone();
        stale_persist.last_watcher_relayed_offset = Some(256);
        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &stale_persist,
            &original_identity,
            original.turn_start_offset,
        );
        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);

        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].user_msg_id, 200, "newer turn must be preserved");
        assert_eq!(
            rows[0].last_watcher_relayed_offset, None,
            "the newer turn must NOT inherit the old turn's stale write"
        );
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
            RelayOwnerKind::StandbyRelay,
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
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].user_msg_id, 101);
        assert_eq!(loaded[0].started_at, "2026-05-17 10:00:05");
        assert_eq!(loaded[0].last_offset, 20);
    }

    #[test]
    fn identity_heartbeat_refresh_rejects_backward_offset_same_identity() {
        // #3017 I6 (last_offset_monotonic): a backward watermark write for the
        // SAME turn identity is rejected so a stale transcript tail cannot be
        // replayed; the on-disk offset is left untouched.
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.last_offset = 200;
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
            150,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 200);
    }

    #[test]
    fn identity_heartbeat_refresh_advances_forward_offset_same_identity() {
        // #3017 I6: a forward watermark write for the same identity advances.
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.last_offset = 200;
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
            250,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 250);
    }

    #[test]
    fn identity_heartbeat_refresh_standby_yields_to_watcher_owner() {
        // #3017 I6 (last_offset_owner_gated): a StandbyRelay caller must not
        // advance the watermark while the persisted owner is the live Watcher.
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        state.last_offset = 100;
        state.set_relay_owner_kind(RelayOwnerKind::Watcher);
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
            250,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].last_offset, 100);
    }

    #[test]
    fn identity_heartbeat_refresh_allows_fresh_turn_reset() {
        // #3017 I6 fresh-turn exemption: a NEW turn identity legitimately
        // resets the watermark to a smaller offset; the identity guards reject
        // the refresh BEFORE the monotonic clamp ever runs, so the standby
        // caller simply does not clobber the new turn.
        let temp = TempDir::new().unwrap();
        let mut old_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        old_turn.current_msg_id = 0;
        old_turn.user_msg_id = 100;
        old_turn.started_at = "2026-05-17 10:00:00".to_string();
        old_turn.last_offset = 500;
        old_turn.turn_start_offset = Some(0);
        let old_identity = InflightTurnIdentity::from_state(&old_turn);
        let output_path = old_turn.output_path.clone().expect("test output path");
        save_inflight_state_in_root(temp.path(), &old_turn).unwrap();

        // A fresh turn: new user_msg_id AND a new turn_start_offset that
        // legitimately resets the watermark to a smaller value.
        let mut fresh_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh_turn.current_msg_id = 0;
        fresh_turn.user_msg_id = 101;
        fresh_turn.started_at = "2026-05-17 10:00:05".to_string();
        fresh_turn.output_path = Some(output_path.clone());
        fresh_turn.last_offset = 10;
        fresh_turn.turn_start_offset = Some(10);
        save_inflight_state_in_root(temp.path(), &fresh_turn).unwrap();

        // The standby caller still believes it is the OLD turn; the identity
        // guards reject it, leaving the fresh turn's small offset intact.
        let refreshed = refresh_inflight_last_offset_if_matches_identity_in_root(
            temp.path(),
            &ProviderKind::Claude,
            321,
            &old_identity,
            old_turn.turn_start_offset,
            &output_path,
            None,
            123,
            RelayOwnerKind::StandbyRelay,
        );

        assert!(!refreshed);
        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].user_msg_id, 101);
        assert_eq!(loaded[0].last_offset, 10);
    }

    #[test]
    fn validate_save_records_backward_last_offset_violation_same_identity() {
        // #3017 I6 OBSERVE-ONLY on the save path: a backward last_offset for
        // the same turn identity records a `last_offset_monotonic` violation
        // (and trips the debug_assert) but does NOT skip the write — a legit
        // fresh-turn reset must still be able to persist.
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.last_offset = 300;
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // Same identity (user_msg_id + turn_start_offset) but a backward
        // last_offset → records a violation. Run with the debug_assert
        // disabled by catching the panic so we can assert observability fired.
        let mut backward = existing.clone();
        backward.last_offset = 100;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_inflight_state_for_save(
                temp.path(),
                &path,
                &backward,
                "src/services/discord/inflight.rs:test",
            );
        }));
        // In debug builds the debug_assert fires; in release it returns.
        // Either way the invariant record was emitted before the assert.
        assert!(result.is_err() || cfg!(not(debug_assertions)));
    }

    #[test]
    fn validate_save_allows_backward_last_offset_for_fresh_turn() {
        // #3017 I6: a DIFFERENT turn identity lowering last_offset is exempt —
        // the save path must not flag a legit fresh-turn reset.
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.last_offset = 300;
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        let mut fresh = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh.user_msg_id = 101;
        fresh.last_offset = 10;

        // No panic — different identity is exempt from the monotonic clamp.
        validate_inflight_state_for_save(
            temp.path(),
            &path,
            &fresh,
            "src/services/discord/inflight.rs:test",
        );
    }

    #[test]
    fn validate_save_records_backward_response_sent_offset_violation_same_identity() {
        // #3154 OBSERVE-ONLY on the save path: a backward response_sent_offset
        // for the SAME turn identity records a `response_sent_offset_monotonic`
        // violation (and trips the debug_assert) but does NOT skip the write —
        // mirrors the last_offset_monotonic precedent.
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.full_response = "hello world".to_string();
        existing.response_sent_offset = 8;
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // Same identity (user_msg_id + turn_start_offset) but a backward
        // response_sent_offset → records a violation. The debug_assert fires in
        // debug builds; catch the panic so we can assert observability fired.
        let mut backward = existing.clone();
        backward.response_sent_offset = 3;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_inflight_state_for_save(
                temp.path(),
                &path,
                &backward,
                "src/services/discord/inflight.rs:test",
            );
        }));
        // In debug builds the debug_assert fires; in release it returns.
        // Either way the invariant record was emitted before the assert.
        assert!(result.is_err() || cfg!(not(debug_assertions)));
    }

    #[test]
    fn validate_save_allows_response_sent_offset_reset_for_fresh_turn() {
        // #3154: a DIFFERENT turn identity resetting response_sent_offset to 0
        // (as InflightTurnState::new does) is exempt — the save path must not
        // flag a legit new-turn reset as a backward regression.
        let temp = TempDir::new().unwrap();
        let mut existing = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 100);
        existing.full_response = "prior turn output".to_string();
        existing.response_sent_offset = 12; // prior turn had N > 0
        save_inflight_state_in_root(temp.path(), &existing).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // A fresh turn: new user_msg_id AND a new turn_start_offset, with
        // response_sent_offset reset to 0 (the InflightTurnState::new default).
        let mut fresh = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 101);
        fresh.user_msg_id = 101;
        fresh.turn_start_offset = Some(99);
        assert_eq!(fresh.response_sent_offset, 0);

        // No panic — different identity is exempt from the monotonic clamp.
        validate_inflight_state_for_save(
            temp.path(),
            &path,
            &fresh,
            "src/services/discord/inflight.rs:test",
        );
    }

    #[test]
    fn validate_save_allows_synthetic_overwrite_after_user_turn_3154() {
        // #3154 replay: a prior USER turn persisted response_sent_offset > 0,
        // then a wakeup/background (TUI-direct synthetic) turn resets inflight
        // via InflightTurnState::new (new identity, response_sent_offset 0).
        // This is a LEGITIMATE new-turn transition and must NOT be flagged as a
        // response_sent_offset_monotonic regression.
        let temp = TempDir::new().unwrap();

        // Prior USER turn with response_sent_offset > 0.
        let mut user_turn = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 555);
        user_turn.user_msg_id = 555;
        user_turn.turn_start_offset = Some(0);
        user_turn.full_response = "user turn response body".to_string();
        user_turn.response_sent_offset = 15;
        save_inflight_state_in_root(temp.path(), &user_turn).unwrap();

        let provider = ProviderKind::Claude;
        let path = inflight_state_path(temp.path(), &provider, 321);

        // Synthetic turn freshly constructed via InflightTurnState::new — a new
        // identity (different user_msg_id / turn_start_offset) and
        // response_sent_offset 0.
        let synthetic = InflightTurnState::new(
            ProviderKind::Claude,
            321,
            Some("adk".to_string()),
            42,
            0, // synthetic user_msg_id
            0,
            "synthetic wakeup".to_string(),
            None,
            Some("AgentDesk-claude-adk".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            900, // new last_offset → new turn_start_offset
        );
        assert_eq!(synthetic.response_sent_offset, 0);
        assert_ne!(synthetic.turn_start_offset, user_turn.turn_start_offset);

        // No panic — the fresh synthetic identity is exempt from the monotonic
        // clamp, so no response_sent_offset_monotonic violation is recorded.
        validate_inflight_state_for_save(
            temp.path(),
            &path,
            &synthetic,
            "src/services/discord/inflight.rs:test",
        );
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

    /// #3161 (codex P1): the zero-owned guarded clear removes a genuine
    /// zero-id-owned row (recovery / external-input turn whose on-disk
    /// `user_msg_id` is 0). Recovery cleanup must keep working.
    #[test]
    fn clear_inflight_state_if_matches_zero_owned_clears_zero_id_row() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 9, 0);
        state.user_msg_id = 0;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_zero_owned_in_root(
            temp.path(),
            &ProviderKind::Claude,
            9,
        );
        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty());
    }

    /// #3161 (codex P1): the zero-owned guarded clear must NOT delete a NEWER
    /// real (non-zero) owner's row. A zero-id turn finalizing after a non-zero
    /// owner wrote its row yields `UserMsgMismatch` and the row survives.
    #[test]
    fn clear_inflight_state_if_matches_zero_owned_preserves_nonzero_owner() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 9, 4242);
        state.user_msg_id = 4242;
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        let outcome = clear_inflight_state_if_matches_zero_owned_in_root(
            temp.path(),
            &ProviderKind::Claude,
            9,
        );
        assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
        let still_there = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(still_there.len(), 1);
        assert_eq!(still_there[0].user_msg_id, 4242);
    }

    /// No on-disk row → `Missing`. Idempotency safety net.
    #[test]
    fn clear_inflight_state_if_matches_missing_is_noop() {
        let temp = TempDir::new().unwrap();
        let outcome =
            clear_inflight_state_if_matches_in_root(temp.path(), &ProviderKind::Claude, 42, 999);
        assert_eq!(outcome, GuardedClearOutcome::Missing);
    }

    // ---------------------------------------------------------------------
    // #3041 P1-2 (codex P1-2 R3): identity-guarded epilogue re-save. On a
    // delivery-lease `Skip` the watcher (holder) owns the inflight lifecycle
    // and clears the row on its OWN success. The bridge's epilogue must NOT
    // resurrect a holder-cleared row; it must refresh a still-present matching
    // row so retry survives when the holder FAILED.
    // ---------------------------------------------------------------------

    /// Skip → holder SUCCEEDED and cleared inflight (no row on disk). The bridge
    /// epilogue's identity-guarded save must NOT resurrect it (`Missing`) — no
    /// stale leak.
    #[test]
    fn skip_save_does_not_resurrect_holder_cleared_inflight() {
        let temp = TempDir::new().unwrap();
        // The holder already removed the row on its success path → nothing on disk.
        let state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        let expected = InflightTurnIdentity::from_state(&state);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &state,
            &expected,
            state.turn_start_offset,
        );

        assert_eq!(
            outcome,
            GuardedSaveOutcome::Missing,
            "holder-cleared inflight must NOT be resurrected by the bridge skip epilogue"
        );
        assert!(
            load_inflight_states_from_root(temp.path(), &ProviderKind::Claude).is_empty(),
            "no row may be recreated for an already-delivered turn"
        );
    }

    /// Skip → holder FAILED (NotDelivered) and did NOT clear; the turn-start row
    /// is still on disk with matching identity. The bridge epilogue's
    /// identity-guarded save refreshes it (`Saved`) so retry can re-deliver —
    /// no black-hole.
    #[test]
    fn skip_save_preserves_inflight_when_holder_did_not_clear() {
        let temp = TempDir::new().unwrap();
        let mut state = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        save_inflight_state_in_root(temp.path(), &state).unwrap();

        // The bridge accumulated more of the answer during the turn; it preserves
        // this updated copy for retry. Identity (user_msg_id/started_at/tmux) is
        // unchanged, so the guarded save must land.
        state.full_response = "partially delivered answer, retry me".to_string();
        let expected = InflightTurnIdentity::from_state(&state);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &state,
            &expected,
            state.turn_start_offset,
        );

        assert_eq!(
            outcome,
            GuardedSaveOutcome::Saved,
            "a still-present matching row must be refreshed so retry survives a holder failure"
        );
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].full_response,
            "partially delivered answer, retry me"
        );
    }

    /// Skip → a NEWER turn (different `user_msg_id`) already wrote its inflight
    /// before the preserving bridge's epilogue ran. The guarded save must NOT
    /// clobber the fresh turn (`IdentityMismatch`).
    #[test]
    fn skip_save_does_not_clobber_newer_turn() {
        let temp = TempDir::new().unwrap();
        // Newer turn currently owns the row on disk. (NB: the guard-test helper's
        // 3rd arg feeds `current_msg_id`; set the real `user_msg_id` explicitly so
        // the two turns differ on the identity field the guard actually checks.)
        let mut newer = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 0);
        newer.user_msg_id = 999;
        save_inflight_state_in_root(temp.path(), &newer).unwrap();

        // The preserving bridge is still holding the PREVIOUS turn (user_msg_id
        // 777). Its identity no longer matches the on-disk newer turn.
        let mut preserved = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 0);
        preserved.user_msg_id = 777;
        let expected = InflightTurnIdentity::from_state(&preserved);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &preserved,
            &expected,
            preserved.turn_start_offset,
        );

        assert_eq!(
            outcome,
            GuardedSaveOutcome::IdentityMismatch,
            "a preserved older turn must NOT overwrite a newer turn's inflight"
        );
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].user_msg_id, 999,
            "the newer turn's inflight must remain intact"
        );
    }

    /// Skip → the on-disk row's `turn_start_offset` no longer matches (a newer
    /// turn reusing the same `user_msg_id`/session at a different offset). The
    /// guarded save must refuse (`IdentityMismatch`).
    #[test]
    fn skip_save_checks_turn_start_offset() {
        let temp = TempDir::new().unwrap();
        let mut on_disk = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        on_disk.turn_start_offset = Some(500);
        save_inflight_state_in_root(temp.path(), &on_disk).unwrap();

        // Same identity (user_msg_id/started_at/tmux) as on_disk so ONLY the
        // turn_start_offset differs — isolating the offset guard.
        let mut preserved = on_disk.clone();
        preserved.turn_start_offset = Some(0);
        let expected = InflightTurnIdentity::from_state(&preserved);

        // The preserving bridge expects offset 0 but disk shows 500.
        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &preserved,
            &expected,
            Some(0),
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
    }

    /// Skip → the on-disk row became a planned-restart marker. The guarded save
    /// must not clobber it (`IdentityMismatch`); restart recovery owns it.
    #[test]
    fn skip_save_does_not_clobber_planned_restart_marker() {
        let temp = TempDir::new().unwrap();
        let mut marker = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        marker.set_restart_mode(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &marker).unwrap();

        let preserved = build_inflight_for_guard_tests(ProviderKind::Claude, 321, 777);
        let expected = InflightTurnIdentity::from_state(&preserved);

        let outcome = save_inflight_state_if_matches_identity_in_root(
            temp.path(),
            &preserved,
            &expected,
            preserved.turn_start_offset,
        );

        assert_eq!(outcome, GuardedSaveOutcome::IdentityMismatch);
        let rows = load_inflight_states_from_root(temp.path(), &ProviderKind::Claude);
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].restart_mode.is_some(),
            "the planned-restart marker must be preserved for recovery"
        );
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
    /// in parallel (the override is global state). `pub(super)` so the
    /// #3293 `recovery_relay_attempts_tests` module serializes on it too.
    pub(super) fn stale_override_test_mutex() -> &'static std::sync::Mutex<()> {
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

#[cfg(test)]
mod recovery_relay_attempts_tests {
    //! #3293: the `recovery_relay_attempts` restart-budget counter.
    //!
    //! The field is an additive `#[serde(default)]` column (NO
    //! `INFLIGHT_STATE_VERSION` bump, #2235 convention): legacy rows must
    //! deserialize with `0`, the value must round-trip, survive the
    //! boot-time `mark_all_inflight_states_restart_mode` re-marking pass
    //! (the infinite-WARN-loop carrier), and never weaken the pane-alive
    //! stale-removal guard.
    use super::{
        InflightTurnState, RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET, load_inflight_states_from_root,
        save_inflight_state_in_root,
    };
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::ProviderKind;
    use tempfile::TempDir;

    fn make_state(channel_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            42,
            43,
            "hello".to_string(),
            Some("session-3293".to_string()),
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    #[test]
    fn budget_is_three_restarts() {
        assert_eq!(RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET, 3);
    }

    #[test]
    fn legacy_row_without_field_deserializes_to_zero() {
        // A pre-#3293 on-disk row has no `recovery_relay_attempts` key; the
        // additive field must default to 0 instead of failing the parse
        // (which would GC the row as malformed).
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
            "updated_at": "2026-05-17 10:00:00"
        }))
        .expect("legacy row without recovery_relay_attempts must deserialize");
        assert_eq!(state.recovery_relay_attempts, 0);
    }

    #[test]
    fn counter_round_trips_through_serde() {
        let mut state = make_state(3293);
        state.recovery_relay_attempts = 2;
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed: InflightTurnState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.recovery_relay_attempts, 2);
    }

    #[test]
    fn counter_survives_disk_round_trip_in_isolated_root() {
        let temp = TempDir::new().unwrap();
        let mut state = make_state(932_931);
        state.recovery_relay_attempts = 2;
        save_inflight_state_in_root(temp.path(), &state).expect("save");

        let loaded = load_inflight_states_from_root(temp.path(), &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].recovery_relay_attempts, 2);
    }

    /// The boot-time `mark_all_inflight_states_restart_mode` pass rewrites
    /// every row (the carrier of the pre-#3293 infinite WARN loop). The
    /// counter must survive that rewrite or the budget could never trip.
    #[test]
    fn counter_survives_restart_mode_remarking() {
        // `mark_all_inflight_states_restart_mode` resolves the root from the
        // process-global `AGENTDESK_ROOT_DIR`; serialize against every other
        // env-touching test and restore the previous value on exit.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous_root = std::env::var("AGENTDESK_ROOT_DIR").ok();
        let temp = TempDir::new().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        struct EnvRestore(Option<String>);
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _restore = EnvRestore(previous_root);

        // `mark_all_…` scans `$AGENTDESK_ROOT_DIR/runtime/discord_inflight`;
        // seed the row under that exact root.
        let inflight_root = super::inflight_runtime_root().expect("env root must resolve");
        let mut state = make_state(932_932);
        state.recovery_relay_attempts = 2;
        save_inflight_state_in_root(&inflight_root, &state).expect("save");

        let updated = super::mark_all_inflight_states_restart_mode(
            &ProviderKind::Codex,
            InflightRestartMode::DrainRestart,
        );
        assert_eq!(updated, 1, "the seeded row must be re-marked");

        let loaded = load_inflight_states_from_root(&inflight_root, &ProviderKind::Codex);
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        assert_eq!(
            loaded[0].recovery_relay_attempts, 2,
            "re-marking must not reset the restart-relay budget counter"
        );
    }

    /// #3293 invariant: the counter must not interact with the pane-alive
    /// stale-removal guard — a row carrying a saturated counter whose tmux
    /// pane is still alive is preserved by `stale_removal_reason`.
    #[test]
    fn saturated_counter_does_not_weaken_pane_alive_stale_guard() {
        let _guard = super::stall_recovery_tests::stale_override_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        super::set_test_tmux_alive_override(Some(&["AgentDesk-codex-adk-cdx-932933"]));

        let mut state = make_state(932_933);
        state.tmux_session_name = Some("AgentDesk-codex-adk-cdx-932933".to_string());
        state.recovery_relay_attempts = 99;

        let result = super::stale_removal_reason(&state, super::INFLIGHT_MAX_AGE_SECS + 1, 7);
        super::set_test_tmux_alive_override(None);
        assert!(
            result.is_none(),
            "alive tmux pane must preserve the row regardless of the relay counter; got {result:?}"
        );
    }
}
