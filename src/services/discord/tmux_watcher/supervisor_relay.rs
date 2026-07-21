//! #3479 Phase-1 rank-1 extraction (1/2): the tmux watcher's supervisor
//! relay-FORWARD half — the session-bound ACK target / forward-result types and
//! the chunk/terminal forward functions plus the commit-fence builder. PURE MOVE
//! from `tmux_watcher.rs` (zero logic change) to shrink the frozen root file
//! below its maintainability baseline.
//!
//! The ACK-outcome + terminal-resend-decision half lives in the sibling
//! `session_bound_ack` module (split only to keep each child within the
//! `src/services/discord/tmux_watcher/**` 700-line namespace cap). This cluster
//! has ZERO coupling to `shared`/`http`/`InflightTurnState`; it depends only on
//! `crate::services::cluster::stream_relay` and the per-channel delivery lease
//! types in `crate::services::discord` (referenced by fully qualified path or
//! function-local `use`). Items are `pub(super)` so the parent watcher loop and
//! the root helper `discard_watcher_pending_buffer_after_suppressed_turn` keep
//! calling them by their original names.

use crate::services::cluster::stream_relay::{RelayDroppedFrame, RelayTurnIdentity};

/// E5 (#2412): forward a freshly-read tmux output chunk into the
/// supervisor-owned [`StreamRelay`] (if one exists for the session). The
/// supervisor's [`RelayProducerRegistry`] is the bridge — it hands the
/// production tmux watcher a clonable
/// [`crate::services::cluster::stream_relay::RelayProducer`] keyed by
/// `tmux_session_name`. The producer's MPSC absorbs the chunk; the
/// relay task drains it into the configured [`RelaySink`]. In production
/// that sink parses provider JSONL and performs Discord terminal delivery
/// for eligible session-bound inflight shapes; metrics-only fallback
/// runtimes still count frames via
/// [`crate::services::cluster::registry_adapter_sink::RegistryAdapterSink`].
///
/// `cached_producer` caches a single producer clone to avoid taking the
/// registry RwLock on every chunk read; it is refreshed from the registry
/// when the cache is empty or when an attempted send observed a torn-down
/// relay (`try_send_frame` returned `false`). When the registry has no
/// producer for this session (flag off, supervisor not running, or this
/// session simply not in the registry's matched set) the function is a
/// total no-op and adds no measurable overhead vs the pre-E5 hot path.
#[derive(Clone)]
pub(super) struct SessionBoundRelayAckTarget {
    pub(super) metrics: std::sync::Arc<crate::services::cluster::stream_relay::RelayMetrics>,
    pub(super) sequence: u64,
    /// #3041 P1-3 (codex P1-3 R6): the `turn_start_offset` of the turn this ACK
    /// target belongs to — taken from the terminal frame's commit fence (the ONLY
    /// frame that yields an ack target). The watcher's per-turn forward carries the
    /// stored ack forward ONLY when it belongs to the turn currently being
    /// ACK-waited (same `turn_start_offset`); a stale ack from a FINISHED/DIFFERENT
    /// turn is reset to `None` so a new turn never inherits a previous turn's
    /// terminal sequence (no false-Delivered black-hole). `None` means the fence
    /// carried no `turn_start_offset` (legacy / no pinned identity) — treated as
    /// "no turn binding", so it is never reused across a turn boundary.
    pub(super) turn_start_offset: Option<u64>,
}

#[derive(Clone)]
pub(super) struct SupervisorRelayForward {
    pub(super) mirrored: bool,
    pub(super) ack_target: Option<SessionBoundRelayAckTarget>,
    pub(super) evicted_frames: Vec<RelayDroppedFrame>,
    /// The first relay sequence assigned by this forward. The watcher carries
    /// the first sequence for the current accumulation window so resolve-time
    /// drop checks can degrade `fully_mirrored` without trusting frame identity.
    pub(super) first_forwarded_sequence: Option<u64>,
    /// #3041 P1-3 (codex P1-3 R7): TRUE when this forward SPLIT a result-bearing
    /// physical chunk and a NON-EMPTY trailing tail (a LATER turn's bytes) followed
    /// the just-completed turn's terminal frame. This is the turn-boundary signal:
    /// after the just-completed turn (A) consumes its own terminal ACK, the watcher
    /// must RESET the stored ack to `None` so the trailing turn (B) — which is
    /// processed from the leftover buffer on a later pass, possibly while
    /// `turn_identity_for_panel` is STILL pinned to A's offset — can NEVER inherit
    /// A's finished ACK. With no inherited ack B reads `MissingTarget` → §3.2
    /// reconciliation (committed-offset SendFull-or-Skip) → B is never black-holed.
    /// Only the SPLIT terminal forward sets this; every other forward leaves it
    /// `false` (no boundary crossed).
    pub(super) trailing_turn_follows: bool,
    /// When `trailing_turn_follows` is true, this is the first sequence assigned
    /// to the later turn's tail. It becomes the carried first sequence for the
    /// leftover buffer after the current turn consumes its terminal ACK.
    pub(super) trailing_first_forwarded_sequence: Option<u64>,
}

impl SupervisorRelayForward {
    fn mirrored_without_ack() -> Self {
        Self {
            mirrored: true,
            ack_target: None,
            evicted_frames: Vec::new(),
            first_forwarded_sequence: None,
            trailing_turn_follows: false,
            trailing_first_forwarded_sequence: None,
        }
    }

    fn not_mirrored() -> Self {
        Self {
            mirrored: false,
            ack_target: None,
            evicted_frames: Vec::new(),
            first_forwarded_sequence: None,
            trailing_turn_follows: false,
            trailing_first_forwarded_sequence: None,
        }
    }
}

pub(super) struct SupervisorRelayTurnState {
    pub(super) fully_mirrored: bool,
    pub(super) first_forwarded_sequence: Option<u64>,
}

pub(super) fn remember_supervisor_relay_first_forwarded_sequence(
    first_forwarded_sequence: &mut Option<u64>,
    forward: &SupervisorRelayForward,
) {
    if first_forwarded_sequence.is_none() {
        *first_forwarded_sequence = forward.first_forwarded_sequence;
    }
}

pub(super) fn supervisor_relay_forward_fully_mirrors_turn(
    forward: &SupervisorRelayForward,
    current_turn: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> bool {
    if !forward.mirrored {
        return false;
    }
    if forward.evicted_frames.is_empty() {
        return true;
    }
    let Some(current_turn) = current_turn else {
        return false;
    };
    let Some(current_start_offset) = current_turn.turn_start_offset else {
        return false;
    };
    !forward.evicted_frames.iter().any(|evicted| {
        let victim = &evicted.turn_identity;
        !victim.has_strict_turn_start_offset()
            || (victim.turn_user_msg_id == current_turn.user_msg_id
                && victim.turn_started_at == current_turn.started_at
                && victim.turn_start_offset == Some(current_start_offset))
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_initial_supervisor_relay_forward(
    all_data_fully_mirrored: &mut bool,
    all_data_session_bound_relay_ack: &mut Option<SessionBoundRelayAckTarget>,
    all_data_first_forwarded_sequence: &mut Option<u64>,
    split_trailing_turn_follows: &mut bool,
    forward: &SupervisorRelayForward,
    initial_buffer_was_empty: bool,
    leftover_is_empty: bool,
    restored_assistant_text_seen: bool,
    current_turn: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> SupervisorRelayTurnState {
    *all_data_session_bound_relay_ack = carry_session_bound_ack_for_turn(
        all_data_session_bound_relay_ack.take(),
        forward.ack_target.clone(),
        current_turn.and_then(|identity| identity.turn_start_offset),
    );
    apply_supervisor_relay_forward_to_accumulated_state(
        all_data_fully_mirrored,
        all_data_first_forwarded_sequence,
        forward,
        initial_buffer_was_empty,
        current_turn,
    );
    *split_trailing_turn_follows |= forward.trailing_turn_follows;
    let turn_state = SupervisorRelayTurnState {
        fully_mirrored: *all_data_fully_mirrored && !restored_assistant_text_seen,
        first_forwarded_sequence: *all_data_first_forwarded_sequence,
    };
    carry_supervisor_relay_trailing_first_forwarded_sequence(
        all_data_first_forwarded_sequence,
        forward,
        leftover_is_empty,
    );
    turn_state
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_streaming_supervisor_relay_forward(
    all_data_fully_mirrored: &mut bool,
    all_data_session_bound_relay_ack: &mut Option<SessionBoundRelayAckTarget>,
    all_data_first_forwarded_sequence: &mut Option<u64>,
    session_bound_relay_turn_fully_mirrored: &mut bool,
    session_bound_relay_turn_first_forwarded_sequence: &mut Option<u64>,
    split_trailing_turn_follows: &mut bool,
    forward: &SupervisorRelayForward,
    chunk_buffer_was_empty: bool,
    leftover_is_empty: bool,
    current_turn: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) {
    *all_data_session_bound_relay_ack = carry_session_bound_ack_for_turn(
        all_data_session_bound_relay_ack.take(),
        forward.ack_target.clone(),
        current_turn.and_then(|identity| identity.turn_start_offset),
    );
    if session_bound_relay_turn_first_forwarded_sequence.is_none() {
        *session_bound_relay_turn_first_forwarded_sequence = forward.first_forwarded_sequence;
    }
    *split_trailing_turn_follows |= forward.trailing_turn_follows;
    let forward_fully_mirrored = apply_supervisor_relay_forward_to_accumulated_state(
        all_data_fully_mirrored,
        all_data_first_forwarded_sequence,
        forward,
        chunk_buffer_was_empty,
        current_turn,
    );
    *session_bound_relay_turn_fully_mirrored &= forward_fully_mirrored;
    carry_supervisor_relay_trailing_first_forwarded_sequence(
        all_data_first_forwarded_sequence,
        forward,
        leftover_is_empty,
    );
}

fn apply_supervisor_relay_forward_to_accumulated_state(
    all_data_fully_mirrored: &mut bool,
    all_data_first_forwarded_sequence: &mut Option<u64>,
    forward: &SupervisorRelayForward,
    buffer_was_empty: bool,
    current_turn: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> bool {
    if buffer_was_empty {
        *all_data_first_forwarded_sequence = forward.first_forwarded_sequence;
    } else {
        remember_supervisor_relay_first_forwarded_sequence(
            all_data_first_forwarded_sequence,
            forward,
        );
    }
    let forward_fully_mirrored = supervisor_relay_forward_fully_mirrors_turn(forward, current_turn);
    if buffer_was_empty {
        *all_data_fully_mirrored = forward_fully_mirrored;
    } else {
        *all_data_fully_mirrored &= forward_fully_mirrored;
    }
    forward_fully_mirrored
}

fn carry_supervisor_relay_trailing_first_forwarded_sequence(
    first_forwarded_sequence: &mut Option<u64>,
    forward: &SupervisorRelayForward,
    leftover_is_empty: bool,
) {
    if forward.trailing_turn_follows {
        *first_forwarded_sequence = forward.trailing_first_forwarded_sequence;
    } else if leftover_is_empty {
        *first_forwarded_sequence = None;
    }
}

/// #3041 P1-3 (codex P1-3 R6): turn-scope the session-bound terminal ACK target so
/// a NEW turn never inherits a FINISHED turn's stale ack.
///
/// The watcher carries `all_data_session_bound_relay_ack` across `'watcher_loop`
/// passes. A single chunk can hold `result(A) + assistant(B) + result(B)`: A rides
/// a terminal frame (ack = A.seq) while B finishes inside the discarded split tail.
/// On the next pass B is processed from the leftover buffer with no fresh frame, so
/// under the legacy "store only when `Some`" rule the stored ack stays pinned to A
/// and A's `Delivered` FALSELY satisfies B's ACK → B black-holed.
///
/// The ack is keyed by `current_turn_start_offset` (the same
/// `InflightTurnIdentity.turn_start_offset` the terminal fence stamps — monotonic
/// per turn — NOT the per-pass `turn_data_start_offset`):
///   * `fresh` is `Some` → this pass forwarded a terminal frame; adopt it.
///   * `fresh` is `None` → keep `stored` ONLY when it is the SAME turn
///     (`stored.turn_start_offset == current_turn_start_offset`, both `Some`); a
///     cross-turn / unbound `stored` drops to `None`. A `None` target makes
///     `wait_for_session_bound_relay_delivery_ack` return `MissingTarget` (NOT
///     `Delivered`), so the watcher falls through to §3.2 reconciliation against
///     `committed_relay_offset` → re-sent at worst (possible duplicate), NEVER
///     black-holed.
pub(super) fn carry_session_bound_ack_for_turn(
    stored: Option<SessionBoundRelayAckTarget>,
    fresh: Option<SessionBoundRelayAckTarget>,
    current_turn_start_offset: Option<u64>,
) -> Option<SessionBoundRelayAckTarget> {
    if let Some(fresh) = fresh {
        return Some(fresh);
    }
    match (stored, current_turn_start_offset) {
        // Same turn (both bound to the same pinned `turn_start_offset`): an ack set
        // earlier in THIS turn survives a later non-terminal pass.
        (Some(ack), Some(current)) if ack.turn_start_offset == Some(current) => Some(ack),
        // A stale ack from a finished/different turn — or any case where the turn
        // binding is unknown on either side — is never consulted: reset to `None`
        // so the new turn reconciles instead of satisfying its ACK against the
        // previous turn's sequence.
        _ => None,
    }
}

pub(super) fn forward_chunk_to_supervisor_relay(
    tmux_session_name: &str,
    chunk: &str,
    registry: &std::sync::Arc<
        crate::services::cluster::relay_producer_registry::RelayProducerRegistry,
    >,
    cached_producer: &mut Option<crate::services::cluster::stream_relay::RelayProducer>,
) -> SupervisorRelayForward {
    forward_chunk_to_supervisor_relay_inner(
        tmux_session_name,
        chunk,
        registry,
        cached_producer,
        None,
        None,
    )
}

pub(super) fn forward_chunk_to_supervisor_relay_for_turn(
    tmux_session_name: &str,
    chunk: &str,
    registry: &std::sync::Arc<
        crate::services::cluster::relay_producer_registry::RelayProducerRegistry,
    >,
    cached_producer: &mut Option<crate::services::cluster::stream_relay::RelayProducer>,
    turn_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> SupervisorRelayForward {
    let frame_identity = relay_turn_identity_for_session(turn_identity, tmux_session_name);
    forward_chunk_to_supervisor_relay_inner(
        tmux_session_name,
        chunk,
        registry,
        cached_producer,
        None,
        frame_identity,
    )
}

/// #3041 P1-3 (Part a, B1): forward the RESULT-bearing chunk as a TERMINAL frame
/// carrying the commit fence (`terminal.consumed_end` + the pinned turn identity).
/// Every non-terminal chunk goes through `forward_chunk_to_supervisor_relay` with
/// no fence (unchanged behaviour). Only the result-bearing chunk — detected AFTER
/// `process_watcher_lines` sets `found_result` — uses this so the commit data rides
/// the exact frame that triggers the sink's terminal delivery (FIFO single-task: a
/// separate later frame would arrive after the delivery already dispatched).
pub(super) fn forward_terminal_chunk_to_supervisor_relay(
    tmux_session_name: &str,
    chunk: &str,
    registry: &std::sync::Arc<
        crate::services::cluster::relay_producer_registry::RelayProducerRegistry,
    >,
    cached_producer: &mut Option<crate::services::cluster::stream_relay::RelayProducer>,
    terminal: crate::services::cluster::stream_relay::TerminalCommitFence,
) -> SupervisorRelayForward {
    forward_chunk_to_supervisor_relay_inner(
        tmux_session_name,
        chunk,
        registry,
        cached_producer,
        Some(terminal),
        None,
    )
}

/// #3041 P1-3 (codex P1-3 issue 1): forward a RESULT-bearing physical chunk that
/// may ALSO contain a trailing LATER-turn tail. `leftover_len` is the post-parse
/// `all_data.len()` — the bytes `process_watcher_lines` did NOT consume (the next
/// turn's bytes). We split `decoded` at that boundary and:
///   1. forward the just-completed turn's bytes (`terminal_part`) on a TERMINAL
///      frame carrying THIS turn's commit fence, and
///   2. forward the trailing later-turn bytes (`tail_part`) on a SEPARATE
///      NON-terminal frame so they are still mirrored into the sink's parser and
///      the later turn is never black-holed (it gets its own fence when it
///      completes on a later pass).
///
/// The returned ACK target is the TERMINAL frame's (so the watcher's terminal-ACK
/// wait correlates to THIS turn's delivery, not the trailing fragment). `mirrored`
/// is the AND of both forwards. When there is no trailing tail this is exactly the
/// single terminal forward.
pub(super) fn forward_terminal_chunk_with_trailing_to_supervisor_relay(
    tmux_session_name: &str,
    decoded: &str,
    leftover_len: usize,
    registry: &std::sync::Arc<
        crate::services::cluster::relay_producer_registry::RelayProducerRegistry,
    >,
    cached_producer: &mut Option<crate::services::cluster::stream_relay::RelayProducer>,
    terminal: crate::services::cluster::stream_relay::TerminalCommitFence,
) -> SupervisorRelayForward {
    let (terminal_part, tail_part) =
        split_decoded_chunk_at_terminal_boundary(decoded, leftover_len);
    let terminal_forward = forward_terminal_chunk_to_supervisor_relay(
        tmux_session_name,
        terminal_part,
        registry,
        cached_producer,
        terminal,
    );
    if tail_part.is_empty() {
        return terminal_forward;
    }
    // Forward the later-turn tail as its OWN non-terminal frame (no fence). This
    // keeps it in the sink's parser stream so the later turn is mirrored; its
    // terminal fence rides a future result-bearing chunk. We keep the TERMINAL
    // frame's ack_target (the watcher waits on THIS turn's delivery) and AND the
    // tail's mirror flag so a failed tail forward still surfaces "not fully
    // mirrored".
    //
    // #3041 P1-3 (codex P1-3 issue 1 R4 — DEFERRED multi-RESULT edge, #3151): a
    // per-result split that gives turn B its OWN terminal fence is INFEASIBLE in
    // this pass — a fence requires B's PINNED turn identity (`turn_start_offset` +
    // `user_msg_id` + `started_at`), but only turn A's identity
    // (`turn_identity_for_panel`) is loaded here; B's inflight is established on a
    // LATER watcher loop pass. Any fence we emitted for B's bytes would carry A's
    // identity and the sink's STRICT identity gate would (correctly) BLOCK it. So B
    // rides this fence-less tail: it is MIRRORED (no black-hole) and gets its own
    // real fence when B completes on a later pass. If this tail already contains
    // B's COMPLETE result, the sink posts B from it; the watcher's later SendFull
    // may then re-post B → a possible DUPLICATE (never a black-hole). The
    // ACK-correlation hazard is closed in the sink (`deliver`): a fence-less frame
    // reports `FrameAccepted`, NEVER a terminal commit, so B's post can never
    // satisfy turn A's terminal-ACK and the ACK stays bound to A's terminal frame.
    let tail_forward =
        forward_chunk_to_supervisor_relay(tmux_session_name, tail_part, registry, cached_producer);
    let mirrored = terminal_forward.mirrored && tail_forward.mirrored;
    let ack_target = terminal_forward.ack_target;
    let first_forwarded_sequence = terminal_forward
        .first_forwarded_sequence
        .or(tail_forward.first_forwarded_sequence);
    let trailing_first_forwarded_sequence = tail_forward.first_forwarded_sequence;
    let mut evicted_frames = terminal_forward.evicted_frames;
    evicted_frames.extend(tail_forward.evicted_frames);
    SupervisorRelayForward {
        mirrored,
        ack_target,
        evicted_frames,
        first_forwarded_sequence,
        // #3041 P1-3 (codex P1-3 R7): a NON-EMPTY trailing tail means a LATER turn's
        // bytes followed THIS turn's terminal frame inside ONE physical chunk — a
        // turn-boundary signal. The watcher resets the stored ack AFTER this turn
        // consumes its own terminal ACK, so the trailing turn never inherits this
        // finished turn's ACK (R7 black-hole close), regardless of whether
        // `turn_identity_for_panel` has refreshed to the trailing turn yet.
        trailing_turn_follows: true,
        trailing_first_forwarded_sequence,
    }
}

/// #3041 P1-3 (codex P1-3 issue 1 — multi-turn-chunk black-hole close): split the
/// freshly-decoded physical chunk at the consumed-terminal boundary so a TERMINAL
/// frame carries ONLY the just-completed turn's bytes, and the trailing bytes of a
/// LATER turn ride a SEPARATE (non-terminal) frame.
///
/// A single physical read can contain turn A's `result` PLUS turn B's first bytes.
/// `process_watcher_lines` stops at A's `result` and leaves B's bytes in the
/// outer-scope `all_data` (the `leftover_len` after the parse). If we forwarded the
/// WHOLE chunk on A's terminal frame, B's bytes would be consumed by the sink's
/// parser as part of A's frame; on the NEXT loop pass `decoded.text` can be empty,
/// so `forward_chunk_to_supervisor_relay_inner` emits NO frame for B → B is never
/// delivered (black-hole), and the now-stale ACK for A can be reused for B
/// (mis-commit). Splitting here forwards A's bytes terminal (with A's fence) and
/// B's trailing bytes as their own non-terminal frame, so B is still mirrored and
/// — when B completes — gets its OWN terminal frame + fence on a later pass.
///
/// The trailing `min(decoded.len(), leftover_len)` bytes of `decoded` are the part
/// that survived the parse as leftover (B's bytes); the rest is A's terminal
/// payload. `leftover_len` is the post-parse `all_data.len()` (bytes the parser did
/// NOT consume). The split index is clamped to a UTF-8 char boundary (defensive —
/// the leftover always begins on a JSONL `\n` line boundary in practice).
pub(super) fn split_decoded_chunk_at_terminal_boundary(
    decoded: &str,
    leftover_len: usize,
) -> (&str, &str) {
    let trailing = leftover_len.min(decoded.len());
    let mut split = decoded.len() - trailing;
    while split < decoded.len() && !decoded.is_char_boundary(split) {
        // A multibyte scalar straddles the nominal split: keep it whole on the
        // terminal side rather than panic-slicing mid-scalar. The leftover begins
        // one boundary later; the sink reorders nothing within a single line, so a
        // whole extra line on the terminal side is harmless and never drops bytes.
        split += 1;
    }
    decoded.split_at(split)
}

pub(super) fn forward_chunk_to_supervisor_relay_inner(
    tmux_session_name: &str,
    chunk: &str,
    registry: &std::sync::Arc<
        crate::services::cluster::relay_producer_registry::RelayProducerRegistry,
    >,
    cached_producer: &mut Option<crate::services::cluster::stream_relay::RelayProducer>,
    terminal: Option<crate::services::cluster::stream_relay::TerminalCommitFence>,
    frame_identity: Option<RelayTurnIdentity>,
) -> SupervisorRelayForward {
    if chunk.is_empty() {
        return SupervisorRelayForward::mirrored_without_ack();
    }
    if cached_producer.is_none() {
        *cached_producer = registry.get_producer(tmux_session_name);
    }
    let Some(producer) = cached_producer.as_ref() else {
        return SupervisorRelayForward::not_mirrored();
    };
    // The relay treats each `try_send_frame` call as one frame. The caller
    // decodes only complete UTF-8 prefixes, so a multibyte scalar split across
    // file reads is forwarded after the next read completes it instead of being
    // replaced with U+FFFD.
    let payload = chunk.to_string();
    // #3041 P1-3 R6: capture the terminal frame's `turn_start_offset` BEFORE the
    // fence is moved into the send so the resulting ack target can be turn-scoped
    // (a stored ack is reused across a watcher pass ONLY when it belongs to the
    // turn now being ACK-waited). A non-terminal frame has no fence → no ack
    // target is produced (the `outcome.sequence.map` below yields `None`).
    let ack_turn_start_offset = terminal.as_ref().and_then(|fence| fence.turn_start_offset);
    let outcome = match terminal {
        Some(fence) => producer.try_send_terminal_frame_with_sequence(payload, fence),
        None => producer.try_send_frame_with_sequence_and_identity(payload, frame_identity),
    };
    if !outcome.is_alive() {
        // Relay was torn down between our registry read and the send —
        // drop the cache so the next chunk re-resolves. If the supervisor
        // republishes for the same session name (Updated event), the
        // next call will hit the new producer.
        *cached_producer = None;
        return SupervisorRelayForward::not_mirrored();
    }
    let ack_target = outcome.sequence.map(|sequence| SessionBoundRelayAckTarget {
        metrics: producer.metrics().clone(),
        sequence,
        turn_start_offset: ack_turn_start_offset,
    });
    let evicted_frames = outcome.dropped_oldest.into_iter().collect();
    SupervisorRelayForward {
        mirrored: true,
        ack_target,
        evicted_frames,
        first_forwarded_sequence: outcome.sequence,
        // A single forward of one frame never crosses a turn boundary; only the
        // split helper sets this when it forwards a separate trailing tail.
        trailing_turn_follows: false,
        trailing_first_forwarded_sequence: None,
    }
}

fn relay_turn_identity_for_session(
    identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    tmux_session_name: &str,
) -> Option<RelayTurnIdentity> {
    let identity = identity?;
    if identity.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    let turn_start_offset = identity.turn_start_offset?;
    Some(RelayTurnIdentity {
        turn_user_msg_id: identity.user_msg_id,
        turn_started_at: identity.started_at.clone(),
        turn_start_offset: Some(turn_start_offset),
    })
}

pub(super) fn terminal_event_consumed_offset(current_offset: u64, unprocessed_tail: &str) -> u64 {
    current_offset.saturating_sub(unprocessed_tail.len() as u64)
}

pub(super) fn suppressed_terminal_confirmed_end(
    current_offset: u64,
    unprocessed_tail: &str,
) -> u64 {
    terminal_event_consumed_offset(current_offset, unprocessed_tail)
}

/// #3041 P1-3 (Part a, B1 — frame-carried commit fence): build the
/// `TerminalCommitFence` to ride on the RESULT-bearing chunk's frame, or `None`
/// when this chunk is not the terminal one / has no real consumed range / has no
/// pinned turn identity to gate the sink's advance.
///
/// The fence carries the watcher's AUTHORITATIVE consumed-terminal `end`
/// (`terminal_event_consumed_offset(current_offset, all_data)` == the watcher's own
/// lease `end`) plus the PINNED turn identity (`user_msg_id` + `started_at`, #3141
/// pinned-id semantics — from the turn-start inflight snapshot, filtered to THIS
/// session). The sink advances `confirmed_end_offset` to `end` on a CONFIRMED
/// delivery ONLY when this identity still matches the channel's current inflight
/// (delayed-old-frame / wrong-turn protection). We DO NOT gate on `sink_can_own`:
/// the fence is inert unless the sink confirms a delivery AND the identity matches,
/// so carrying it on every real terminal chunk is safe.
pub(super) fn watcher_terminal_commit_fence(
    found_result: bool,
    turn_data_start_offset: u64,
    consumed_end: u64,
    pinned_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    tmux_session_name: &str,
) -> Option<crate::services::cluster::stream_relay::TerminalCommitFence> {
    if !found_result || consumed_end <= turn_data_start_offset {
        return None;
    }
    let identity = pinned_identity?;
    // Only pin an identity for THIS tmux session (the panel-identity snapshot is
    // already filtered to it, but guard defensively so a cross-session snapshot
    // can never seed a wrong-turn fence).
    if identity.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return None;
    }
    // #3041 P1-3 (codex P1-3 issue 2 R4): a fence MUST carry a real
    // `turn_start_offset`. The sink's identity gate is STRICT on this field (no
    // None fallback) so two consecutive `user_msg_id == 0` (TUI-direct) turns in
    // the same one-second `started_at` cannot collide. If the would-be terminal
    // turn's start offset is unknown, DO NOT emit a fence: returning None forwards
    // a non-terminal frame instead, and the watcher reconciliation's SendFull then
    // safely delivers this turn (no black-hole, no weakly-gated advance).
    let turn_start_offset = identity.turn_start_offset?;
    Some(
        crate::services::cluster::stream_relay::TerminalCommitFence {
            consumed_end,
            turn_user_msg_id: identity.user_msg_id,
            turn_started_at: identity.started_at.clone(),
            // A fence ALWAYS carries a real `turn_start_offset` (guaranteed above)
            // so the sink's identity gate can disambiguate two consecutive
            // `user_msg_id == 0` turns started in the same second.
            turn_start_offset: Some(turn_start_offset),
        },
    )
}

#[cfg(test)]
#[path = "supervisor_relay_tests.rs"]
mod tests;
