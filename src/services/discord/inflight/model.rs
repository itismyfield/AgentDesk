//! Inflight turn-state domain model (#3479 extraction).
//!
//! Pure data types for the inflight turn state contract: the
//! [`InflightTurnState`] row, its turn-identity projection
//! [`InflightTurnIdentity`], the [`TurnSource`] / [`RelayOwnerKind`] audit
//! enums, the `opt_message_id` zero-id helper, and the version-tolerant serde
//! adapters. Behaviour-preserving move out of `inflight.rs`; the parent
//! re-exports every public item so existing `inflight::*` paths still resolve.

use std::num::NonZeroU64;

use super::*;
use crate::services::agent_protocol::TaskNotificationKind;

/// Build an optional `serenity::MessageId` from a possibly-zero raw persisted id.
///
/// A zero message id is a legitimate sentinel for an unanchored TUI-direct or
/// recovery turn. Callers must treat `None` as "skip the message-specific step"
/// instead of constructing `MessageId::new(0)`, which panics. Because zero is an
/// expected sentinel here (not an anomaly), the zero case is silent — unlike
/// [`opt_channel_id`], where a zero channel id is never legitimate.
pub(in crate::services::discord) fn opt_message_id(
    raw: u64,
) -> Option<poise::serenity_prelude::MessageId> {
    NonZeroU64::new(raw).map(|raw| poise::serenity_prelude::MessageId::new(raw.get()))
}

/// Build an optional `serenity::ChannelId` from a persisted Discord channel id.
///
/// Stored recovery state must never construct `ChannelId::new(0)`: the invalid
/// state is skipped and left available for a later diagnostic or repair.
pub(in crate::services::discord) fn opt_channel_id(
    raw: u64,
) -> Option<poise::serenity_prelude::ChannelId> {
    let Some(raw) = NonZeroU64::new(raw) else {
        tracing::warn!("skipping Discord channel operation because persisted id is zero");
        return None;
    };
    Some(poise::serenity_prelude::ChannelId::new(raw.get()))
}

pub(in crate::services::discord) use opt_message_id as optional_message_id;

#[cfg(test)]
mod discord_id_tests {
    use super::{opt_channel_id, opt_message_id};
    use poise::serenity_prelude::{ChannelId, MessageId};

    #[test]
    fn optional_id_helpers_return_none_for_zero_without_panicking() {
        assert_eq!(opt_message_id(0), None);
        assert_eq!(opt_message_id(42), Some(MessageId::new(42)));
        assert_eq!(opt_channel_id(0), None);
        assert_eq!(opt_channel_id(43), Some(ChannelId::new(43)));
    }
}

const SYNTHETIC_FINALIZER_TURN_ID_MASK: u64 = i64::MAX as u64;
const SYNTHETIC_FINALIZER_TURN_ID_FLOOR: u64 = 1_000_000_000_000_000_000;

fn mix_finalizer_hash(mut hash: u64, bytes: &[u8]) -> u64 {
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn synthetic_finalizer_turn_id(
    provider: &str,
    channel_id: u64,
    started_at: &str,
    tmux_session_name: Option<&str>,
    output_path: Option<&str>,
    turn_start_offset: Option<u64>,
    last_offset: u64,
    born_generation: u64,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    let offset = turn_start_offset.unwrap_or(last_offset);
    hash = mix_finalizer_hash(hash, provider.as_bytes());
    hash = mix_finalizer_hash(hash, &[0xff]);
    hash = mix_finalizer_hash(hash, &channel_id.to_le_bytes());
    hash = mix_finalizer_hash(hash, &[0xff]);
    hash = mix_finalizer_hash(hash, started_at.as_bytes());
    hash = mix_finalizer_hash(hash, &[0xff]);
    hash = mix_finalizer_hash(hash, tmux_session_name.unwrap_or("").as_bytes());
    hash = mix_finalizer_hash(hash, &[0xff]);
    hash = mix_finalizer_hash(hash, output_path.unwrap_or("").as_bytes());
    hash = mix_finalizer_hash(hash, &[0xff]);
    hash = mix_finalizer_hash(hash, &offset.to_le_bytes());
    hash = mix_finalizer_hash(hash, &[0xff]);
    hash = mix_finalizer_hash(hash, &born_generation.to_le_bytes());
    hash = mix_finalizer_hash(hash, &[0xff]);
    (hash % (SYNTHETIC_FINALIZER_TURN_ID_MASK - SYNTHETIC_FINALIZER_TURN_ID_FLOOR))
        + SYNTHETIC_FINALIZER_TURN_ID_FLOOR
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(in crate::services::discord) struct InflightTurnState {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    pub channel_name: Option<String>,
    /// Offset-authority channel for tmux watcher delivery records.
    ///
    /// This is usually identical to `channel_id`, but a bridge turn can reuse a
    /// watcher owned by another channel. Durable delivery records are keyed by
    /// that owner channel, while `channel_id` remains the Discord delivery
    /// channel. `logical_channel_id` is a thread-parent axis and must not be
    /// used as a substitute.
    #[serde(default)]
    pub watcher_owner_channel_id: Option<u64>,
    #[serde(default)]
    pub logical_channel_id: Option<u64>,
    #[serde(default)]
    pub thread_id: Option<u64>,
    #[serde(default)]
    pub thread_title: Option<String>,
    pub request_owner_user_id: u64,
    pub user_msg_id: u64,
    /// Nonzero identity used by the single-authority finalizer ledger. It is
    /// independent of Discord user-message anchoring: real user id wins, then a
    /// pinned injected prompt id, then a persisted synthetic id for id-0 turns.
    #[serde(default)]
    pub finalizer_turn_id: u64,
    /// Discord message id for the live status panel when status-panel-v2 is
    /// enabled. `current_msg_id` remains the assistant response message.
    #[serde(default)]
    pub status_message_id: Option<u64>,
    /// #3805 P2: per-turn epoch for the live status panel. Later stages bump
    /// this on every two-message re-anchor (rollover / recovery) so a stale
    /// turn can never edit or delete a newer turn's panel (the generation guard
    /// rides alongside the existing `finalizer_turn_id` / snowflake identity
    /// checks). Additive `#[serde(default)]` field — legacy rows deserialize as
    /// `0`, no `INFLIGHT_STATE_VERSION` bump per the #2235 compat convention.
    /// PR-A is pure scaffolding: no read/write site exists yet (later PR-B~
    /// wires the generation bump and guard).
    #[serde(default)]
    pub status_panel_generation: u64,
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
    #[serde(default)]
    pub claude_e_pid: Option<u32>,
    #[serde(default)]
    pub claude_e_process_starttime: Option<u128>,
    #[serde(default)]
    pub claude_e_macos_lstart_hash: Option<u128>,
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
    /// Monotonic per-row write generation. `updated_at` is intentionally a
    /// human-readable second-resolution string for compatibility, so gates that
    /// must detect same-second liveness use this additive counter instead.
    #[serde(default)]
    pub save_generation: u64,
    /// Restart generation at which this turn was born.
    #[serde(default)]
    pub born_generation: u64,
    /// #3293: count of restarts whose recovery terminal relay failed
    /// transiently for this row. Additive `#[serde(default)]` field —
    /// no `INFLIGHT_STATE_VERSION` bump per the #2235 compat convention.
    #[serde(default)]
    pub recovery_relay_attempts: u32,
    /// #3918: durable per-turn idempotency marker for the committed-then-gone
    /// anchor-repost fallback (#3607/#3610). Set to `true` immediately AFTER a
    /// `Delivered` send-new repost and BEFORE the row is cleared, so that if the
    /// subsequent `clear_inflight_state` fails (returns `false`) or the process
    /// crashes after this write, the next boot re-loads this row with the marker
    /// set and `try_recover_anchor_repost` short-circuits to `None` instead of
    /// re-posting the same answer a SECOND time. This is the primary
    /// at-most-once guard for the realistic unbounded loop (a silently failing
    /// clear). Additive `#[serde(default)]` field — legacy rows deserialize as
    /// `false`, no `INFLIGHT_STATE_VERSION` bump per the #2235 compat convention.
    #[serde(default)]
    pub anchor_reposted: bool,
    /// #3918: count of committed-then-gone anchor-repost send-new ATTEMPTS for
    /// this turn, bumped durably BEFORE each send. Hard-bounds the residual
    /// Discord-accept→marker-write crash window (where `anchor_reposted` was not
    /// yet recorded) to at most `RECOVERY_RELAY_RESTART_ATTEMPT_BUDGET` posts so
    /// duplication can never be unbounded. Deliberately DISTINCT from
    /// `recovery_relay_attempts` (the transient-failure retry budget) so the
    /// pre-send count never double-counts against the `PreserveAndCount` bump —
    /// a premature force-clear there would re-introduce the #3607 data loss.
    /// Additive `#[serde(default)]` field; legacy rows deserialize as `0`.
    #[serde(default)]
    pub anchor_repost_attempts: u32,
    /// #3976: durable per-row marker stamped ONLY after a genuinely confirmed
    /// `SessionBoundRelay` TUI-direct terminal delivery (the POST landed AND the
    /// identity gate matched AND the `confirmed_end_offset` watermark advance
    /// fired — see `session_relay_sink::advance_offset_for_confirmed_delegated_terminal`).
    ///
    /// The watermark advanced by that path is the resettable, non-durable
    /// in-memory `confirmed_end_offset`; it writes NOTHING else to the row. So a
    /// DELIVERED-but-unmirrored row is byte-identical to a never-delivered
    /// black-hole row, and on a watermark reset (generation change / output
    /// regression / restart) below the prior turn body the orphan-reclaim path
    /// could not tell them apart — it downgraded the delivered row to ownerless
    /// and recovery re-emitted the already-delivered tail byte-for-byte when a
    /// following non-Managed (/loop) turn started. This durable flag is the
    /// discriminator: `session_bound_relay_external_input_orphan_shape_at`
    /// excludes a row with it set, so a delivered row is NOT reclaimed while a
    /// genuine never-delivered black-hole (flag still `false`) STILL is. Same
    /// idempotency-marker pattern as `anchor_reposted` (#3918). Additive
    /// `#[serde(default)]` field — legacy rows deserialize as `false`, no
    /// `INFLIGHT_STATE_VERSION` bump per the #2235 compat convention.
    #[serde(default)]
    pub session_bound_delivered: bool,
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
    /// Immutable per-row turn discriminator used by identity-guarded clears.
    /// Additive `#[serde(default)]` field: legacy rows deserialize as `None` and
    /// keep the identity-only clear contract.
    #[serde(default)]
    pub turn_nonce: Option<String>,
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
    /// #4002: `true` when this inflight exists ONLY to carry relay ownership for
    /// a `SystemContinuation` (compact-resume) turn. It has a real note message id
    /// in `user_msg_id` (so the idle-tail self-pin / Path A gates work) but is NOT
    /// a user-authored lifecycle turn: the watcher completion Path B (⏳ → ✅
    /// reaction + `session_transcripts` / `turn_analytics` rows keyed on
    /// `user_msg_id`) MUST skip such a row, or every compact resume would brand its
    /// neutral note with a `✅` and write a phantom user-turn analytics/transcript
    /// row (`turn_id=discord:<channel>:note.id`). Relay-ownership adoption and the
    /// bridge-tail stand-down are unaffected — only the post-completion bookkeeping
    /// is suppressed. Additive `#[serde(default)]` field — legacy rows deserialize
    /// as `false` (unchanged behaviour); set only at the SystemContinuation
    /// synthetic birth site.
    #[serde(default)]
    pub relay_ownership_only: bool,
    /// #4370: `true` when this turn was re-adopted from persisted inflight state
    /// by `recovery_engine::reregister_active_turn_from_inflight` — i.e. a REAL
    /// user turn (mailbox owner == `request_owner_user_id`, NOT the synthetic
    /// relay owner) whose mailbox slot was reseeded from disk instead of being
    /// born through the normal turn loop or a synthetic compact-resume note. The
    /// re-adopt site is reached from restart restore AND from the mid-execution
    /// watcher/manual-rebind reattach entries, so the name says "re-adopted from
    /// inflight", not "restart-only".
    ///
    /// DELIBERATELY DISTINCT FROM `relay_ownership_only`: that marker means "this
    /// turn does not own the user-turn lifecycle" and its guards SUPPRESS the
    /// completion lifecycle (`watcher_completion_lifecycle_applies`,
    /// `inflight_skips_tui_completion_observation`, the early/late TUI completion
    /// gates, the `⏳ → ✅` reaction + `session_transcripts` / `turn_analytics`
    /// persistence). A re-adopted turn DOES still own its user's turn, so its own
    /// `✅`/footer + analytics/transcript must STILL fire — reusing
    /// `relay_ownership_only` would wrongly mute the very prose this fix protects.
    ///
    /// This marker therefore feeds EXACTLY ONE guard: TUI-direct synthetic
    /// `stale_reclaim` eligibility for a PRESENT row. It lets a later starved
    /// injection / task-notification synthetic turn reclaim the mailbox of a
    /// re-adopted real-user owner once that owner is stale
    /// (`terminal_delivery_committed`) — closing the #4018 regression on the
    /// restart-resume path, where the synthetic-owner-only reclaim could never
    /// free a real-user mailbox (#4370). It NEVER by itself triggers a reclaim; a
    /// live, progressing re-adopted turn (matching `user_msg_id`, not committed)
    /// still yields reclaim-reason `None`.
    ///
    /// NOTE (#4370): this marker IS persisted on a DrainRestart-preserved row. The
    /// BROAD identity-refresh save (`save_inflight_state_if_identity_unchanged`)
    /// refuses any row still carrying `restart_mode`, which is precisely why the
    /// marker is written through the NARROW single-field patch
    /// `mark_readopted_from_inflight_if_identity_unchanged`
    /// (`inflight/save_store/identity_gate.rs`) instead: it re-reads under the
    /// sidecar flock, pins the turn identity, flips only this additive bit, and
    /// preserves `restart_mode`. Test `readopted_marker_lands_on_restart_preserved_row_and_never_resurrects`
    /// pins that behavior. So the present-row (Path A) reclaim DOES cover
    /// restart-preserved rows.
    ///
    /// The ROW-ABSENT reclaim (Path B) still does not consult this field — there is
    /// no row left to read — and uses the in-memory
    /// `SharedData::readopted_mailbox_ledger` instead. This field is the
    /// present-row companion signal.
    ///
    /// Additive `#[serde(default)]` field — legacy rows deserialize as `false`
    /// (no `INFLIGHT_STATE_VERSION` bump, #2235 compat convention); set only at
    /// the inflight re-adopt site.
    #[serde(default)]
    pub readopted_from_inflight: bool,
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
    /// #4247 FIX 2: originating `Intervention::preserve_on_cancel()` decision,
    /// persisted so `mailbox_requeue_inflight_for_followup_retry` can rebuild a
    /// PRE-submit busy-timeout requeue with the SAME fail-safe preservation
    /// mark the genuine-human intake computed — instead of unconditionally
    /// reconstructing an unmarked `Intervention` that regresses to main's
    /// drop-on-cancel behavior. In-memory/serde-default only (no DB migration,
    /// no `INFLIGHT_STATE_VERSION` bump per the #2235 compat convention);
    /// legacy rows deserialize as `false`, matching the previous unmarked
    /// reconstruction exactly.
    #[serde(default)]
    pub followup_preserve_on_cancel: bool,
    /// #3871: Discord message ids of the streamed rollover PREFIXES this turn
    /// froze (a `>DISCORD_MSG_LIMIT` answer that rolled over mid-stream). Persisted
    /// alongside `response_sent_offset` so a terminal full-body fallback that runs
    /// in a LATER `'watcher_loop` iteration or after a watcher restart still deletes
    /// every accumulated frozen prefix (no residual duplicate). Empty for legacy
    /// rows / turns that never rolled over.
    #[serde(default)]
    pub streaming_rollover_frozen_msg_ids: Vec<u64>,
}

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
        // The full state struct lived behind the removed SQLite-only gate, so we
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

    #[test]
    fn set_watcher_owner_channel_id_with_zero_channel_id_skips_record_without_panicking() {
        // A persisted channel_id == 0 sentinel must not reach ChannelId::new(0),
        // which panics. The opt_channel_id guard skips the delivery-record write
        // instead. Removing the guard makes this construction panic (#4608).
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            0,
            None,
            7,
            8,
            9,
            "hello".to_string(),
            None,
            Some("AgentDesk-claude-adk".to_string()),
            None,
            None,
            0,
        );
        let changed = state.set_watcher_owner_channel_id(123);
        assert!(changed);
        assert_eq!(state.watcher_owner_channel_id, Some(123));
    }

    #[test]
    fn watcher_owner_channel_id_defaults_absent_legacy_rows_to_none() {
        let state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 9,
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
            "started_at": "2026-06-28 10:00:00",
            "updated_at": "2026-06-28 10:00:00",
            "watcher_owns_live_relay": false
        }))
        .expect("legacy row without owner channel should deserialize");

        assert_eq!(state.watcher_owner_channel_id, None);
        assert_eq!(state.delivery_record_owner_channel_id(), 42);
    }

    #[test]
    fn watcher_owner_channel_id_round_trips_when_present() {
        let json = serde_json::json!({
            "version": 9,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "watcher_owner_channel_id": 99,
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
            "started_at": "2026-06-28 10:00:00",
            "updated_at": "2026-06-28 10:00:00",
            "watcher_owns_live_relay": false
        });
        assert_eq!(json["watcher_owner_channel_id"], serde_json::json!(99));
        let parsed: InflightTurnState =
            serde_json::from_value(json).expect("deserialize owner channel");
        assert_eq!(parsed.watcher_owner_channel_id, Some(99));
        assert_eq!(parsed.delivery_record_owner_channel_id(), 99);
    }

    /// #3805 P2 (PR-A scaffolding): a legacy row written before the field
    /// existed must still deserialize, with `status_panel_generation`
    /// defaulting to 0 (additive `#[serde(default)]`, no version bump per the
    /// #2235 compat convention).
    #[test]
    fn status_panel_generation_defaults_to_zero_for_legacy_rows() {
        let state: InflightTurnState = serde_json::from_value(serde_json::json!({
            "version": 9,
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
            "started_at": "2026-06-28 10:00:00",
            "updated_at": "2026-06-28 10:00:00",
            "watcher_owns_live_relay": false
        }))
        .expect("legacy row without status_panel_generation should deserialize");

        assert_eq!(state.status_panel_generation, 0);
    }

    /// #3805 P2 (PR-A scaffolding): when the field is present it round-trips
    /// through (de)serialization unchanged.
    #[test]
    fn status_panel_generation_round_trips_when_present() {
        let json = serde_json::json!({
            "version": 9,
            "provider": "codex",
            "channel_id": 42,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 7,
            "user_msg_id": 8,
            "status_message_id": 555,
            "status_panel_generation": 3,
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
            "started_at": "2026-06-28 10:00:00",
            "updated_at": "2026-06-28 10:00:00",
            "watcher_owns_live_relay": false
        });
        let parsed: InflightTurnState =
            serde_json::from_value(json).expect("deserialize status_panel_generation");
        assert_eq!(parsed.status_panel_generation, 3);
        let reserialized = serde_json::to_value(&parsed).expect("serialize back");
        assert_eq!(
            reserialized["status_panel_generation"],
            serde_json::json!(3)
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
        let provider_name = provider.as_str().to_string();
        let born_generation = super::super::runtime_store::load_generation();
        let finalizer_turn_id = if user_msg_id != 0 {
            user_msg_id
        } else {
            synthetic_finalizer_turn_id(
                &provider_name,
                channel_id,
                &now,
                tmux_session_name.as_deref(),
                output_path.as_deref(),
                Some(last_offset),
                last_offset,
                born_generation,
            )
        };
        let runtime_kind = input_fifo_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .map(|_| RuntimeHandoffKind::LegacyTmuxWrapper);
        Self {
            version: INFLIGHT_STATE_VERSION,
            provider: provider_name,
            channel_id,
            channel_name,
            watcher_owner_channel_id: Some(channel_id),
            logical_channel_id: Some(channel_id),
            thread_id: None,
            thread_title: None,
            request_owner_user_id,
            user_msg_id,
            finalizer_turn_id,
            status_message_id: None,
            status_panel_generation: 0,
            current_msg_id,
            current_msg_len: 0,
            user_text,
            source: Source::Text,
            session_id,
            tmux_session_name,
            output_path,
            input_fifo_path,
            claude_e_pid: None,
            claude_e_process_starttime: None,
            claude_e_macos_lstart_hash: None,
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
            save_generation: 0,
            born_generation,
            recovery_relay_attempts: 0,
            // #3918: never reposted / zero send-new attempts at turn birth.
            anchor_reposted: false,
            anchor_repost_attempts: 0,
            // #3976: never confirmed-delivered at turn birth.
            session_bound_delivered: false,
            any_tool_used: false,
            has_post_tool_text: false,
            session_key: None,
            delivery_bot: None,
            silent_turn: false,
            dispatch_id: None,
            turn_nonce: Some(uuid::Uuid::new_v4().to_string()),
            last_watcher_relayed_offset: None,
            last_watcher_relayed_generation_mtime_ns: None,
            restart_mode: None,
            restart_generation: None,
            rebind_origin: false,
            // #3581: only the rebind-origin birth site stamps these.
            rebind_origin_created_at_unix: None,
            rebind_origin_deadline_secs: None,
            rebind_origin_birth_generation: None,
            // #4002: only the SystemContinuation synthetic birth site sets this.
            relay_ownership_only: false,
            // #4370: only the inflight re-adopt site sets this.
            readopted_from_inflight: false,
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
            followup_preserve_on_cancel: false,
            streaming_rollover_frozen_msg_ids: Vec::new(),
        }
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }

    pub(in crate::services::discord) fn effective_finalizer_turn_id(&self) -> u64 {
        if self.user_msg_id != 0 {
            return self.user_msg_id;
        }
        if let Some(id) = self.injected_prompt_message_id.filter(|id| *id != 0) {
            return id;
        }
        if self.finalizer_turn_id != 0 {
            return self.finalizer_turn_id;
        }
        synthetic_finalizer_turn_id(
            &self.provider,
            self.channel_id,
            &self.started_at,
            self.tmux_session_name.as_deref(),
            self.output_path.as_deref(),
            self.turn_start_offset,
            self.last_offset,
            self.born_generation,
        )
    }

    pub(in crate::services::discord) fn ensure_finalizer_turn_id(&mut self) -> bool {
        let resolved = self.effective_finalizer_turn_id();
        if self.finalizer_turn_id == resolved {
            false
        } else {
            self.finalizer_turn_id = resolved;
            true
        }
    }

    pub(in crate::services::discord) fn matches_finalizer_turn_id(&self, expected: u64) -> bool {
        expected != 0
            && (self.user_msg_id == expected || self.effective_finalizer_turn_id() == expected)
    }

    pub(in crate::services::discord) fn effective_relay_owner_kind(&self) -> RelayOwnerKind {
        match self.relay_owner_kind {
            RelayOwnerKind::None if self.watcher_owns_live_relay => RelayOwnerKind::Watcher,
            kind => kind,
        }
    }

    /// #4400 (b): is this row the orphaned headless synthetic shape that the
    /// #3107 watcher self-heal (`reacquire_watcher_inflight_for_active_stream`)
    /// re-mints after a stall-watchdog force-clean deleted the real row?
    ///
    /// Zero ids (`user_msg_id == 0 && request_owner_user_id == 0`) exclude both
    /// real user turns AND the #4018 TUI-direct synthetic relay owner
    /// (`request_owner_user_id == 1`), so adopting this shape can never steal a
    /// live turn (invariant I2). Watcher ownership plus non-blank restore
    /// anchors (tmux session + output path) are the self-heal birth stamps;
    /// rebind-origin rows keep their own #3581 replace/reap lifecycle and a
    /// terminal-committed row keeps the committed-cleanup path authoritative.
    ///
    /// Single source of truth shared by the rebind preflight classifier
    /// (`recovery_engine::phase_policy::can_adopt_orphaned_synthetic_watcher_row`),
    /// the adoption-save identity gate
    /// (`save_existing_inflight_rebind_adoption_impl_in_root`), and the
    /// adopted-transcript offset preservation check
    /// (`claude_tui_force_initial_offset_for_adopted_transcript`) — the three
    /// layers must not drift or the adoption either 409s (classifier), 500s
    /// (identity gate), or drops the dead-window backlog (offset rebase).
    pub(in crate::services::discord) fn is_adoptable_orphaned_synthetic_watcher_row(&self) -> bool {
        !self.rebind_origin
            && self.user_msg_id == 0
            && self.request_owner_user_id == 0
            && !self.terminal_delivery_committed
            && self.effective_relay_owner_kind() == RelayOwnerKind::Watcher
            && self
                .tmux_session_name
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
            && self
                .output_path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
    }

    pub(in crate::services::discord) fn set_relay_owner_kind(&mut self, kind: RelayOwnerKind) {
        self.relay_owner_kind = kind;
        self.watcher_owns_live_relay = matches!(kind, RelayOwnerKind::Watcher);
    }

    pub(in crate::services::discord) fn set_watcher_owner_channel_id(
        &mut self,
        owner_channel_id: u64,
    ) -> bool {
        let normalized = (owner_channel_id != 0).then_some(owner_channel_id);
        let changed = self.watcher_owner_channel_id != normalized;
        self.watcher_owner_channel_id = normalized;
        if let (Some(provider), Some(tmux_session_name)) = (
            ProviderKind::from_str(&self.provider),
            self.tmux_session_name.as_deref().filter(|name| !name.is_empty()),
        ) && let Some(owner_channel_id) = normalized
            && let Some(self_channel_id) = opt_channel_id(self.channel_id)
            && let Err(error) =
                crate::services::discord::outbound::delivery_record::record_watcher_owner_channel_context(
                    &provider,
                    self_channel_id,
                    poise::serenity_prelude::ChannelId::new(owner_channel_id),
                    tmux_session_name,
                )
        {
            tracing::info!("⚠ delivery-record owner-channel save failed: {error}");
        }
        changed
    }

    pub(in crate::services::discord) fn delivery_record_owner_channel_id(&self) -> u64 {
        self.watcher_owner_channel_id
            .filter(|id| *id != 0)
            .unwrap_or(self.channel_id)
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
        // #4247 FIX 2: the genuine-human intake's preserve decision, carried
        // forward so a PRE-submit busy-timeout requeue can reconstruct a
        // correctly marked/unmarked `Intervention` (see field doc above).
        preserve_on_cancel: bool,
    ) {
        self.followup_reply_context = reply_context;
        self.followup_has_reply_boundary = has_reply_boundary;
        self.followup_merge_consecutive = merge_consecutive;
        self.followup_pending_uploads = pending_uploads;
        self.followup_voice_announcement = voice_announcement;
        self.followup_preserve_on_cancel = preserve_on_cancel;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    pub(in crate::services::discord) fn matches_state(&self, state: &InflightTurnState) -> bool {
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
