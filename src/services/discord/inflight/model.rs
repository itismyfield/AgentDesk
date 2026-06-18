//! Inflight turn-state domain model (#3479 extraction).
//!
//! Pure data types for the inflight turn state contract: the
//! [`InflightTurnState`] row, its turn-identity projection
//! [`InflightTurnIdentity`], the [`TurnSource`] / [`RelayOwnerKind`] audit
//! enums, the `optional_message_id` zero-id helper, and the version-tolerant
//! serde adapters. Behaviour-preserving move out of `inflight.rs`; the parent
//! re-exports every public item so existing `inflight::*` paths still resolve.

use super::*;

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
pub(in crate::services::discord) struct InflightTurnState {
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
    /// #3581: bounded-preservation stamps for a `rebind_origin` row.
    ///
    /// A rebind-origin inflight is born by `recovery_engine` when a
    /// STALL-WATCHDOG force-clean → watcher respawn synthesises a
    /// `turn_source = ExternalAdopted`, `user_msg_id = 0`,
    /// `relay_owner_kind = None` row to expose a recovered tmux session.
    /// Historically these rows were preserved unconditionally (the rebind
    /// API "owns" them), but a row that is never adopted and never makes
    /// progress becomes a permanent orphan that
    /// `tui_direct_pending_start::backstop_claim_is_safe` mistakes for a
    /// "live foreign inflight", wedging every subsequent turn-start
    /// (#3581). These stamps let the reaper bound the preservation:
    /// `rebind_origin_created_at_unix` anchors the TTL, `_deadline_secs`
    /// carries the (env-overridable) deadline captured at birth, and
    /// `_birth_generation` lets a boot-time generation mismatch reap the
    /// row immediately. All three are additive `#[serde(default)]` fields —
    /// no `INFLIGHT_STATE_VERSION` bump per the #2235 compat convention;
    /// legacy rows deserialize them as `None` and fall back to file-mtime
    /// age. Only ever populated at the rebind-origin birth site; `None` on
    /// every non-rebind row.
    #[serde(default)]
    pub rebind_origin_created_at_unix: Option<i64>,
    #[serde(default)]
    pub rebind_origin_deadline_secs: Option<u64>,
    #[serde(default)]
    pub rebind_origin_birth_generation: Option<u64>,
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
    /// FIX #6 (Codex P2): originating `Intervention::reply_context` for this
    /// follow-up turn, persisted so a PRE-submit busy-timeout requeue can
    /// rebuild the retry Intervention without losing the quoted-reply context.
    /// `#[serde(default)]` → `None` for legacy rows / non-follow-up paths.
    #[serde(default)]
    pub followup_reply_context: Option<String>,
    /// FIX #6: originating `Intervention::has_reply_boundary`. Defaults to
    /// `false` for legacy rows.
    #[serde(default)]
    pub followup_has_reply_boundary: bool,
    /// FIX #6: originating `Intervention::merge_consecutive`. Defaults to
    /// `false` for legacy rows.
    #[serde(default)]
    pub followup_merge_consecutive: bool,
    /// FIX #6: originating `Intervention::pending_uploads` (attachment refs).
    /// Empty for legacy rows / turns without uploads.
    #[serde(default)]
    pub followup_pending_uploads: Vec<String>,
    /// FIX #6: originating `Intervention::voice_announcement` projection.
    /// `VoiceTranscriptAnnouncement` is already serde-persisted in the durable
    /// intervention queue, so it round-trips directly here. `None` for
    /// non-voice turns / legacy rows.
    #[serde(default)]
    pub followup_voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
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
            born_generation: super::super::runtime_store::load_generation(),
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
            // #3581: only the rebind-origin birth site stamps these.
            rebind_origin_created_at_unix: None,
            rebind_origin_deadline_secs: None,
            rebind_origin_birth_generation: None,
            long_running_placeholder_active: false,
            watcher_owns_live_relay: false,
            relay_owner_kind: RelayOwnerKind::None,
            turn_source: TurnSource::Managed,
            injected_prompt_message_id: None,
            followup_reply_context: None,
            followup_has_reply_boundary: false,
            followup_merge_consecutive: false,
            followup_pending_uploads: Vec::new(),
            followup_voice_announcement: None,
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
        self.restart_generation = Some(super::super::runtime_store::load_generation());
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

    /// FIX #6 (Codex P2): record the originating `Intervention`'s follow-up
    /// requeue context so a PRE-submit busy-timeout requeue
    /// (`mailbox_requeue_inflight_for_followup_retry`) can faithfully rebuild
    /// the retry Intervention (reply context / attachments / voice metadata)
    /// instead of dropping it. Called at the follow-up turn-start construction
    /// site; non-follow-up paths leave the defaults (empty/None), which is
    /// correct.
    pub(in crate::services::discord) fn set_followup_requeue_context(
        &mut self,
        reply_context: Option<String>,
        has_reply_boundary: bool,
        merge_consecutive: bool,
        pending_uploads: Vec<String>,
        voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    ) {
        self.followup_reply_context = reply_context;
        self.followup_has_reply_boundary = has_reply_boundary;
        self.followup_merge_consecutive = merge_consecutive;
        self.followup_pending_uploads = pending_uploads;
        self.followup_voice_announcement = voice_announcement;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct InflightTurnIdentity {
    pub user_msg_id: u64,
    pub started_at: String,
    pub tmux_session_name: Option<String>,
    /// #3041 P1-3 (codex P1-3 issue 2): the turn's `turn_start_offset` — the JSONL
    /// byte offset at which this turn began. Disambiguates two consecutive
    /// `user_msg_id == 0` TUI-direct turns whose `started_at` collides at
    /// `now_string`'s 1-second resolution; monotonic per turn → unique identity.
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

    pub(in crate::services::discord::inflight) fn matches_state(
        &self,
        state: &InflightTurnState,
    ) -> bool {
        self.user_msg_id == state.user_msg_id
            && self.started_at == state.started_at
            && self.tmux_session_name == state.tmux_session_name
            // #3419 R3 (codex MEDIUM): keep the clear key == full-struct-eq decision key (TOCTOU on offset-only-diff rows).
            && self.turn_start_offset == state.turn_start_offset
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
