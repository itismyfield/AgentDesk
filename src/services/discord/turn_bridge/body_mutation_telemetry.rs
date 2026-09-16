//! #5938 body-mutation telemetry — OBSERVATION ONLY.
//!
//! The mechanism behind #5938 is established: the bridge-local `full_response`
//! and the tmux watcher's accumulator seed each other through the durable
//! inflight row, and `merge_forward_response_progress`
//! (`inflight/save_store/identity_gate/stamp_merge.rs`) is longest-wins
//! fail-open, so a body that already contains the turn twice wins every
//! subsequent merge. What is NOT established is the *interleaving* of the two
//! writes — watcher-first versus bridge-first. Both orders produce the same
//! final length, so the delivered artifact cannot tell them apart, and the
//! repair axis cannot be chosen without measuring it.
//!
//! So this module records and never intervenes. It does not block an append,
//! does not block an assignment, and does not change a merge rule. Blocking
//! here would swallow legitimately repeated model text and manufacture a fresh
//! #5941-class silent loss.
//!
//! Every mutation site emits the SAME record shape. A single `delta_sha8`
//! appearing under two different `site` values inside one turn pins the double
//! write immediately — that identification is the whole point of the record.
//!
//! COVERAGE — WHAT IS RECORDED, AND AN EXPLICIT REFUSAL TO CLAIM COMPLETENESS.
//!
//! READ THE ABSENCE OF A RECORD AS "NO RECORD", NEVER AS "NO MUTATION."
//!
//! That sentence is the whole contract, and it is written this way because the
//! alternative has now failed twice. The first revision of this module asserted
//! "these are ALL the places the bridge-local `full_response` body changes
//! shape"; review produced a counterexample. The second replaced it with a
//! three-class criterion plus a "named, so the boundary is a stated fact"
//! enumeration and the claim that nothing outside it "can introduce a byte from
//! the watcher's side"; review produced two more counterexamples — the terminal
//! `Done` result on the TUI-direct path, which is decoded from the very output
//! file the watcher tails, and the two `TurnBridgeContext.full_response` seeds
//! that copy a durable row before the bridge task even starts. Both are now
//! recorded. What is NOT repaired by recording them is the pattern: a prose
//! completeness claim about a mutation surface this large is a claim the next
//! reader will falsify. So this module no longer makes one, in any form.
//!
//! Concretely, for anyone reading a per-turn readout:
//!   * A record means that site ran and these bytes moved. Trust it.
//!   * NO record does NOT mean the body was untouched. An unexplained step
//!     between one record's `after_len` and the next record's `before_len` is
//!     evidence of an unrecorded mutation, and the right response is to go find
//!     which site did it and add it here — not to conclude nothing happened.
//!   * In particular, "no class-1 record this turn" is NOT evidence of
//!     bridge-first. It is the absence of evidence either way.
//!
//! The classes below describe why each RECORDED site was chosen. They are a
//! rationale for inclusion, not a partition of the surface.
//!
//! 1. CROSS-BOUNDARY ADOPTION — the site copies bytes the bridge did not itself
//!    produce INTO the bridge-local body. These are what the watcher-first /
//!    bridge-first question is read from, so this class gets every site found so
//!    far: `ReconcileFromInflightState` and
//!    `ReconcileToolArmLocalsFromInflightState` (durable inflight row, the
//!    channel the watcher and the bridge seed each other through),
//!    `RecoverBodyFromOutputFile` (the tmux output file, re-read directly by the
//!    empty-response recovery path), `AdoptTerminalDoneResult` (the terminal
//!    `Done` result, which on the TUI-direct path is decoded from that same
//!    output file — see the variant's own doc), and `SeedFromTurnBridgeContext`
//!    (the durable row copied into `TurnBridgeContext.full_response` before the
//!    bridge task starts).
//! 2. STREAM-LOOP ACCUMULATION — the composers that build the body during the
//!    turn, including the one that SHRINKS it: `AppendStreamedTextChunk`,
//!    `AppendToolBoundarySeparator`.
//! 3. SHARED BLANKING — the site empties the bridge-local body AND the durable
//!    row body together, so the bytes leave the shared channel rather than just
//!    the local copy: `ClearResponseDeliveryState`, `SilenceRequeuedResponse`.
//!
//! KNOWN UNRECORDED SITES, as of this writing and WITHOUT any claim that the
//! list is exhaustive — it is a starting point for the next investigation, not a
//! boundary: the two `ProviderErrorPresentation` guidance replacements in
//! `stream_loop/content_arms.rs`; API_FRICTION marker stripping in
//! `post_loop_finalize.rs` and in
//! `terminal_outcome_delivery/empty_response_recovery/handler.rs`; the
//! `CLAUDE_TUI_FOLLOWUP_REQUEUE_DELIVERY_NOTICE` constant in
//! `post_loop_finalize.rs`; the `String::new()` suppressions in
//! `terminal_outcome_delivery.rs`, `handler.rs` (three) and
//! `terminal_outcome_delivery/recovery_retry.rs`; and
//! `prompt_too_long_guidance::render_for_requester` in
//! `terminal_outcome_delivery/cancel_prompt_replace.rs`. Each of those replaces
//! the body during TERMINAL delivery, after the interleaving this module
//! measures has already happened, which is why they were lower priority — NOT a
//! proof that they cannot matter.
//!
//! The WATCHER's own accumulator in `tmux_watcher.rs` is not instrumented here
//! either. A watcher-first turn is EXPECTED to show up as a class-1 record whose
//! `delta_sha8` covers bytes this bridge never appended; that is what the record
//! shape was built for. Per the contract above, a turn without one has not been
//! shown to be bridge-first.

use crate::services::observability::{InvariantViolation, record_invariant_check};
use sha2::{Digest, Sha256};

/// Tracing target for the per-mutation record.
///
/// The `agentdesk::` prefix is load-bearing, not cosmetic: the shipped filter
/// is [`crate::logging::DEFAULT_TRACING_DIRECTIVE`] (`agentdesk=info`), whose
/// target match is a path prefix, so a target without that first segment is
/// REJECTED and the record never reaches `dcserver.stdout.log`. The second
/// segment keeps it greppable/filterable once admitted.
/// `production_filter_admits_the_body_mutation_target` pins both halves of that
/// statement against the shipped directive constant.
const BODY_MUTATION_TARGET: &str = "agentdesk::body_mutation";

/// Upper bound on the body bytes fed to a digest for one mutation record.
///
/// WHY A THRESHOLD AT ALL: `append_streamed_text_chunk` runs on every streaming
/// text tick, so digesting the whole accumulated body per tick is O(n) per tick
/// and O(n²) per turn. The bound makes the per-tick digest cost constant above
/// 1 MiB instead of growing with the turn.
///
/// WHY 1 MiB IS SAFE FOR #5938: the #5938 fingerprint is the *self-duplication*
/// predicate ([`body_is_exact_self_duplicate`]), and that predicate is NEVER
/// bounded — it runs at every length, on every mutation, at every site. This
/// threshold only suppresses the two correlation digests. The observed #5938
/// body was 1198 bytes (599 × 2), the largest body measured on this deployment
/// was 13,005 bytes, and a Discord-deliverable turn body is orders of magnitude
/// below 1 MiB, so in practice the digests are always present for the class
/// this instrumentation was written to identify; above the bound the record
/// still carries `site`, `before_len`, `after_len`, `prefix_len` and the
/// self-duplication verdict, which is enough to order the writers.
///
/// The bound is NOT raised or lowered without moving
/// `digest_limit_is_pinned_at_its_exact_boundary`, which asserts the last
/// digesting length and the first suppressed length are adjacent at exactly
/// this value — a silent re-tune in either direction fails there.
const BODY_MUTATION_DIGEST_LIMIT: usize = 1024 * 1024;

/// Written into `delta_sha8` / `body_sha8` when the body exceeded
/// [`BODY_MUTATION_DIGEST_LIMIT`]. It cannot be confused with a real digest,
/// which is always exactly 8 lowercase hex characters.
const DIGEST_OVER_LIMIT: &str = "over-limit";

/// Shortest body the self-duplication predicate will flag.
///
/// The floor exists because sub-threshold "self-duplicates" are ordinary text,
/// not corruption: `"\n\n"`, `"  "`, `"byebye"` and every two-character repeat
/// trivially satisfy `first_half == second_half`, and flagging them would emit
/// ERROR-level invariant violations on healthy turns and bury the real signal.
///
/// WHY 16 AND NOT 64: this deployment's modal assistant turn is a short Korean
/// acknowledgement, and Hangul costs 3 bytes per syllable, so the whole class
/// sits between 16 and 56 bytes DOUBLED — `"확인했어요!"` doubles to 32,
/// `"완료했습니다."` to 38, `"네, 확인했습니다."` to 48. A 64-byte floor made
/// every one of them invisible, which is the opposite of what the
/// instrumentation is for. The noise the floor has to keep out is shorter than
/// that (`"byebye"` = 6, `"\n"` = 1), and the noise that is *longer* than the
/// floor — laugh runs like `"ㅋ"` × 20 — is excluded by
/// [`has_shorter_repeating_period`] instead, which is the guard that scales.
const SELF_DUPLICATION_MIN_LEN: usize = 16;

/// Separators that can sit BETWEEN the two copies of a doubled body.
///
/// This list is the correction for the original n=1 fingerprint. Both composers
/// insert a paragraph break when the first copy ends on a sentence boundary:
/// `chunk_compose::append_streamed_text_chunk` and the watcher's
/// `tmux_output_stream.rs` assistant-text arm BOTH call
/// `semantic_boundaries::semantic_chunk_separator_needed` and, when it holds,
/// `push_str("\n\n")` before the next segment. So the real shape of a doubled
/// body is `X + "\n\n" + X` whenever `X` ends in one of
/// `semantic_boundaries::semantic_terminal_char`'s members
/// (`. ! ? … 。 ！ ？`) — which is almost every natural-language turn. The
/// observed #5938 body had no separator only because its prompt ended in the
/// digits `COUNT-060`, and a digit is not a terminal char; keying the
/// fingerprint on that accident would have made it fire on one synthetic probe
/// and nothing else.
///
/// `""` keeps the original no-separator case. `"\n"` covers a single-newline
/// join (`append_tool_boundary_separator` trims a trailing `\n` run before
/// re-adding its own break, so an off-by-one newline between copies is
/// reachable without either composer emitting it deliberately).
///
/// `self_duplication_separators_match_the_composed_boundary` drives the REAL
/// `append_streamed_text_chunk` with each terminal char rather than hand-rolling
/// the separator, so this list cannot drift away from what production composes.
const SELF_DUPLICATION_SEPARATORS: [&str; 3] = ["", "\n", "\n\n"];

/// Correlation keys for one mutation record.
///
/// `record_invariant_check` only updates the `guard_fires` counter bucket when
/// BOTH `provider` and `channel_id` are present (`observability/emit.rs`
/// `(Some(provider), Some(channel_id))`), so a site that cannot supply them
/// produces a violation with no correlation key and no bucket movement. Sites
/// that hold an `InflightTurnState` supply both; the streaming append site
/// cannot (see [`BodyMutationCorrelation::unavailable`]).
///
/// #5938 r2 P2-2: the STORED `invariant_violation` event is a different artifact
/// from the tracing line. The line inherits `dispatch_id` / `session_key` /
/// `turn_id` from the enclosing `discord_turn_bridge` span; the stored event
/// does not inherit anything and carries only what this struct hands it. The
/// first revision passed `None` for all three, so a violation row could not be
/// joined to the dispatch or the turn that produced it — exactly the join an
/// #5938 investigation starts from. Every site that holds a row now fills them
/// through [`BodyMutationCorrelation::from_inflight_row`]. `user_msg_id` rather
/// than a formatted `turn_id` is carried so the struct stays `Copy`; `publish`
/// renders it with the repo-wide `discord:<channel>:<user_msg>` spelling that
/// `inflight::turn_id_for_state` already uses, including its `user_msg_id != 0`
/// guard.
#[derive(Debug, Clone, Copy, Default)]
pub(in crate::services::discord::turn_bridge) struct BodyMutationCorrelation<'a> {
    pub(in crate::services::discord::turn_bridge) provider: Option<&'a str>,
    pub(in crate::services::discord::turn_bridge) channel_id: Option<u64>,
    pub(in crate::services::discord::turn_bridge) dispatch_id: Option<&'a str>,
    pub(in crate::services::discord::turn_bridge) session_key: Option<&'a str>,
    pub(in crate::services::discord::turn_bridge) user_msg_id: Option<u64>,
}

impl<'a> BodyMutationCorrelation<'a> {
    /// TEST ONLY: the two `guard_fires` bucket keys and nothing else.
    ///
    /// Every production site holds a row and uses [`Self::from_inflight_row`],
    /// so this exists purely so the tests can assert the difference between a
    /// correlation that CAN move the bucket and [`Self::unavailable`], which
    /// cannot, without standing up an `InflightTurnState` for each one.
    #[cfg(test)]
    pub(in crate::services::discord::turn_bridge) const fn new(
        provider: &'a str,
        channel_id: u64,
    ) -> Self {
        Self {
            provider: Some(provider),
            channel_id: Some(channel_id),
            dispatch_id: None,
            session_key: None,
            user_msg_id: None,
        }
    }

    /// Every key the durable row can supply.
    pub(in crate::services::discord::turn_bridge) fn from_inflight_row(
        state: &'a crate::services::discord::inflight::InflightTurnState,
    ) -> Self {
        Self {
            provider: Some(state.provider.as_str()),
            channel_id: Some(state.channel_id),
            dispatch_id: state.dispatch_id.as_deref(),
            session_key: state.session_key.as_deref(),
            user_msg_id: Some(state.user_msg_id),
        }
    }

    /// No keys — for the streaming append site only.
    ///
    /// `stream_loop/content_arms.rs` sits exactly at its 635-line
    /// `scripts/audit_maintainability_config.toml` cap, so threading either key
    /// down to `append_streamed_text_chunk` would reformat its call site onto
    /// extra lines and fail that gate, and this PR must not raise a cap. The
    /// enclosing `discord_turn_bridge` span (`turn_bridge/mod.rs`) still carries
    /// channel_id / provider / dispatch_id / session_key / turn_id on every line
    /// the append site emits, so the TRACING record stays correlated; only the
    /// `guard_fires` bucket is unreachable from there.
    pub(in crate::services::discord::turn_bridge) const fn unavailable() -> Self {
        Self {
            provider: None,
            channel_id: None,
            dispatch_id: None,
            session_key: None,
            user_msg_id: None,
        }
    }
}

/// Which mutation site produced a record.
///
/// NOT a partition of the mutation surface, and deliberately not presented as
/// one — see the COVERAGE section at the top of this module. Adding a variant
/// here is how an unrecorded site gets recorded; the absence of a variant says
/// nothing about whether such a site exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) enum BodyMutationSite {
    /// `chunk_compose::append_streamed_text_chunk` — the streamed `Text` append
    /// path, driven from `stream_loop/content_arms.rs`.
    AppendStreamedTextChunk,
    /// `chunk_compose::append_tool_boundary_separator` — the per-`ToolUse`
    /// trailing-whitespace truncate plus one `"\n\n"`, driven from
    /// `stream_loop/tool_arms.rs`, whose result is written to the durable row on
    /// the very next statement. This site can SHRINK the body.
    AppendToolBoundarySeparator,
    /// `bridge_entry_persist::reconcile_runtime_locals_from_inflight_state` —
    /// the whole-body assignment from the durable inflight row.
    ReconcileFromInflightState,
    /// `retry_state::clear_response_delivery_state` — the empty-sink rewind that
    /// blanks the local body and the durable row body together.
    ClearResponseDeliveryState,
    /// `stream_loop::tool_arms::authority::reconcile_tool_arm_locals_after_guarded_save`
    /// — the TOOL-ARM mirror of [`Self::ReconcileFromInflightState`]: the same
    /// whole-body adoption from the durable row, run from the two tool-arm
    /// fences instead of from `stream_tick`.
    ///
    /// It is a separate variant rather than a reuse of the `stream_tick` one
    /// because the two differ in exactly the way the verdict cares about. This
    /// one runs only on `GuardedSaveOutcome::Saved`, and one of the two paths
    /// that produce `Saved`
    /// (`inflight/save_store/identity_gate/stream_loop_patch.rs`, the
    /// `!baseline_authority.bridge_owns_relay()` branch its own comment calls
    /// the "Exact watcher/standby self-handoff") first overwrites the row from
    /// the ON-DISK copy. So this site is the bridge adopting a body the WATCHER
    /// staged, and collapsing it into the `stream_tick` variant would hide which
    /// of the two fences carried the watcher's bytes in.
    ReconcileToolArmLocalsFromInflightState,
    /// `terminal_outcome_delivery::queue_retry_silence::apply` — the requeue
    /// silencer, which blanks the local body and the durable row body together
    /// exactly as [`Self::ClearResponseDeliveryState`] does. Same class, same
    /// reason: an unrecorded `N` → `0` on the shared channel is indistinguishable
    /// from loss.
    SilenceRequeuedResponse,
    /// `terminal_outcome_delivery::empty_response_recovery::handler::adopt_recovered_output_file_body`
    /// — the empty-response recovery path re-reading the tmux output file and
    /// replacing the body with what it found.
    ///
    /// A THIRD origin, neither the bridge's own stream nor the durable row: the
    /// bytes come from the file the WATCHER also reads, from
    /// `inflight_state.last_offset` forward. If that offset has fallen behind,
    /// the re-read returns a span the bridge already delivered, which is a way
    /// to manufacture a doubled body on its own. It has to be visible.
    RecoverBodyFromOutputFile,
    /// `context_window::resolve_done_response` — the terminal `Done` result
    /// replacing the streamed body, driven from `stream_loop/content_arms.rs`.
    ///
    /// This looked like a bridge-authored replacement and is not. On the
    /// TUI-direct path the `Done` frame is SYNTHESISED FROM THE WATCHER'S OWN
    /// OUTPUT FILE: `tui_prompt_relay/claude_idle_bridge.rs` builds an
    /// `IdleTerminalSource` whose `transcript_path` is the canonicalised
    /// `output_path` it also hands to
    /// `WatcherClaimIncarnation::capture_for_source`, so it is by construction
    /// the file the tmux watcher tails; the terminal frame travels as
    /// `StreamMessage::ClaudeTuiTerminalDone`, and
    /// `inflight/save_store/identity_gate/runtime_stamp.rs` re-validates that
    /// path against the row before converting it to `StreamMessage::Done {
    /// result, .. }` and writing the SAME bytes to `fresh.full_response`. So
    /// both halves of the #5938 double-write can be this one byte string.
    ///
    /// Two further reasons it cannot stay silent. `resolve_done_response`'s
    /// first arm fires when the streamed body is blank, and
    /// `idle_stream_message_is_content` documents a terminal `Done` as a
    /// legitimate sole carrier of a turn body — so a whole turn can arrive
    /// through here with no other record at all. And
    /// `done_result_supersedes_streamed_partial` triggers on
    /// `terminal.starts_with(streamed) || terminal.ends_with(streamed)` with
    /// `terminal.len() > streamed.len()`, which is exactly the superset shape a
    /// doubled body has; because `record_from_parts` evaluates
    /// [`body_is_exact_self_duplicate`] only when a record is built, an
    /// unrecorded adoption here meant the #5938 fingerprint never looked at the
    /// body that was about to be delivered.
    ///
    /// Correlation is [`BodyMutationCorrelation::unavailable`] for the same
    /// reason the streamed append site uses it: the only production caller sits
    /// in `stream_loop/content_arms.rs`, which is at its 635-line
    /// `scripts/audit_maintainability_config.toml` cap with zero headroom, so a
    /// fifth argument would add a line there. The enclosing `discord_turn_bridge`
    /// span still carries the keys on the tracing line.
    AdoptTerminalDoneResult,
    /// `bridge_entry_persist::seed_bridge_local_body` — the bridge-local body
    /// being BORN from `TurnBridgeContext.full_response` at `turn_bridge/mod.rs`.
    ///
    /// Two of the five production `TurnBridgeContext` constructions seed it from
    /// a durable inflight row —
    /// `recovery_engine/restore_inflight.rs` (`state.full_response.clone()`,
    /// restart recovery) and `tui_prompt_relay/claude_idle_bridge.rs`
    /// (`claim.row.full_response.clone()`, TUI-direct idle continuation, which
    /// its own guard shows is expected to be non-empty whenever
    /// `start_offset >= claim.row.last_offset`). That is a durable-row adoption
    /// by any reading, and it happens BEFORE the bridge task exists, so no
    /// reconcile ever reports it: the shared adopter's `local == durable` no-op
    /// skip guarantees the first `ReconcileFromInflightState` after such a seed
    /// is silent. Recording the birth is what makes those bytes visible at all.
    ///
    /// Recorded from `before = ""` because that is literally true — there was no
    /// bridge-local body a moment earlier — and skipped entirely for an empty
    /// seed, which is not an adoption and is what the other three construction
    /// sites pass.
    SeedFromTurnBridgeContext,
}

impl BodyMutationSite {
    pub(in crate::services::discord::turn_bridge) const fn as_str(self) -> &'static str {
        match self {
            Self::AppendStreamedTextChunk => "chunk_compose::append_streamed_text_chunk",
            Self::AppendToolBoundarySeparator => "chunk_compose::append_tool_boundary_separator",
            Self::ReconcileFromInflightState => {
                "bridge_entry_persist::reconcile_runtime_locals_from_inflight_state"
            }
            Self::ClearResponseDeliveryState => "retry_state::clear_response_delivery_state",
            Self::ReconcileToolArmLocalsFromInflightState => {
                "tool_arms::authority::reconcile_tool_arm_locals_after_guarded_save"
            }
            Self::SilenceRequeuedResponse => "queue_retry_silence::apply",
            Self::RecoverBodyFromOutputFile => {
                "empty_response_recovery::adopt_recovered_output_file_body"
            }
            Self::AdoptTerminalDoneResult => "context_window::resolve_done_response",
            Self::SeedFromTurnBridgeContext => "bridge_entry_persist::seed_bridge_local_body",
        }
    }

    const fn code_location(self) -> &'static str {
        match self {
            Self::AppendStreamedTextChunk => {
                "src/services/discord/turn_bridge/chunk_compose.rs:append_streamed_text_chunk"
            }
            Self::AppendToolBoundarySeparator => {
                "src/services/discord/turn_bridge/chunk_compose.rs:append_tool_boundary_separator"
            }
            Self::ReconcileFromInflightState => {
                "src/services/discord/turn_bridge/bridge_entry_persist.rs:reconcile_runtime_locals_from_inflight_state"
            }
            Self::ClearResponseDeliveryState => {
                "src/services/discord/turn_bridge/retry_state.rs:clear_response_delivery_state"
            }
            Self::ReconcileToolArmLocalsFromInflightState => {
                "src/services/discord/turn_bridge/stream_loop/tool_arms/authority.rs:reconcile_tool_arm_locals_after_guarded_save"
            }
            Self::SilenceRequeuedResponse => {
                "src/services/discord/turn_bridge/terminal_outcome_delivery/queue_retry_silence.rs:apply"
            }
            Self::RecoverBodyFromOutputFile => {
                "src/services/discord/turn_bridge/terminal_outcome_delivery/empty_response_recovery/handler.rs:adopt_recovered_output_file_body"
            }
            Self::AdoptTerminalDoneResult => {
                "src/services/discord/turn_bridge/context_window.rs:resolve_done_response"
            }
            Self::SeedFromTurnBridgeContext => {
                "src/services/discord/turn_bridge/bridge_entry_persist.rs:seed_bridge_local_body"
            }
        }
    }
}

/// One observed body mutation. Built purely so the shape can be asserted in
/// tests without standing up a tracing subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) struct BodyMutationRecord {
    pub(in crate::services::discord::turn_bridge) site: BodyMutationSite,
    pub(in crate::services::discord::turn_bridge) before_len: usize,
    pub(in crate::services::discord::turn_bridge) after_len: usize,
    /// Byte length the before/after bodies share. `after[prefix_len..]` is the
    /// span `delta_sha8` covers; `before_len > prefix_len` means the mutation
    /// discarded a suffix of the previous body rather than extending it.
    pub(in crate::services::discord::turn_bridge) prefix_len: usize,
    pub(in crate::services::discord::turn_bridge) delta_sha8: String,
    pub(in crate::services::discord::turn_bridge) body_sha8: String,
    pub(in crate::services::discord::turn_bridge) self_duplicate: bool,
}

/// First 8 hex characters of the SHA-256 of `bytes`. Byte slices only — never
/// `&str` slicing — so no input can reach a UTF-8 boundary panic.
fn sha8(bytes: &[u8]) -> String {
    let hex = format!("{:x}", Sha256::digest(bytes));
    hex[..8].to_string()
}

/// Length of the shared byte prefix. Byte-wise on purpose: the result is only
/// ever used to slice `&[u8]`, so a midpoint inside a multi-byte codepoint is
/// harmless, whereas a `&str` slice at the same index would panic.
fn common_prefix_len(before: &[u8], after: &[u8]) -> usize {
    before
        .iter()
        .zip(after.iter())
        .take_while(|(left, right)| left == right)
        .count()
}

/// True when `bytes` is some strictly shorter block repeated a whole number of
/// times — `"ㅋㅋ"`, `"abab"`, `"   "`.
///
/// This is the guard that scales past [`SELF_DUPLICATION_MIN_LEN`]. A laugh run
/// (`"ㅋ"` × 20 = 60 bytes) clears the floor, splits into equal halves and would
/// otherwise raise an ERROR on a perfectly healthy turn; its half has period 3,
/// so it is rejected here. A real duplicated turn body is natural-language prose
/// whose minimal period is its own length, so it is NOT rejected — which is the
/// asymmetry `minimal_period_rejects_repeat_runs_but_admits_a_genuine_double`
/// pins from both sides.
///
/// Knuth–Morris–Pratt failure function: `len - failure[len - 1]` is the smallest
/// `p` with `bytes[i] == bytes[i - p]` for all `i >= p`, and that `p` tiles the
/// slice exactly when it divides `len`.
fn has_shorter_repeating_period(bytes: &[u8]) -> bool {
    let len = bytes.len();
    if len < 2 {
        return false;
    }
    let mut failure = vec![0usize; len];
    let mut matched = 0usize;
    for index in 1..len {
        while matched > 0 && bytes[index] != bytes[matched] {
            matched = failure[matched - 1];
        }
        if bytes[index] == bytes[matched] {
            matched += 1;
        }
        failure[index] = matched;
    }
    let period = len - failure[len - 1];
    period < len && len.is_multiple_of(period)
}

/// True when every character is whitespace (or the slice is empty).
///
/// A half is always valid UTF-8 — see the char-boundary argument on
/// [`body_is_exact_self_duplicate`] — so the `str` path is the one that runs and
/// it catches non-ASCII blanks (`U+3000`, `U+00A0`) that a byte test would miss.
/// The byte fallback exists only to keep the predicate total: a panic or an
/// `unwrap` inside observation-only instrumentation would itself be a P0.
fn is_blank(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.trim().is_empty(),
        Err(_) => bytes.iter().all(u8::is_ascii_whitespace),
    }
}

/// #5938 fingerprint: the body is one span, then optionally a composed
/// paragraph separator, then that same span again.
///
/// Everything here is byte comparison, never `&str` slicing, because the
/// midpoint of a body containing multi-byte UTF-8 can land inside a codepoint
/// and `&str` slicing there panics. The byte form cannot panic and agrees with
/// the `&str` form wherever the latter is legal: when the two halves are equal
/// and the separator is ASCII, the split index is necessarily a char boundary
/// (a mid-sequence index holds a continuation byte, which can never equal the
/// body's first byte).
///
/// Three exclusions keep healthy turns quiet, and each one is load-bearing:
/// the [`SELF_DUPLICATION_MIN_LEN`] floor for short repeats, [`is_blank`] for a
/// body that is only whitespace, and [`has_shorter_repeating_period`] for a
/// character run long enough to clear the floor.
///
/// KNOWN FALSE POSITIVES — measured, not hypothetical, and deliberately left in.
/// The predicate asks whether the WHOLE body is `X + sep + X`, so a turn whose
/// entire content happens to be two identical halves is flagged: one repeated
/// code line (`    let x = compute_value(input);` × 2, 67 B), one repeated
/// bullet (`- 로그를 확인한다` × 2, 49 B), one repeated table row
/// (`| id | name | status |` × 2, 45 B), and a deliberately repeated emphasis
/// paragraph joined by `"\n\n"` (118 B) all return true while being perfectly
/// healthy. They stay because every filter that would exclude them — a
/// line-count floor, a "halves must differ in punctuation" rule, a longer
/// minimum — also excludes the short Korean acknowledgement that
/// [`SELF_DUPLICATION_MIN_LEN`] was lowered to 16 to catch, and that class is
/// this deployment's modal turn. The asymmetry is deliberate and the direction
/// is chosen: this is OBSERVATION ONLY (no delivery gate anywhere in
/// `record_invariant_check`), so a false positive costs one ERROR line plus one
/// `guard_fires` tick, while a false negative costs the investigation the
/// module exists for. `known_false_positive_shapes_are_pinned_not_filtered`
/// holds the four shapes so the cost stays a documented number instead of a
/// surprise in the readout.
pub(in crate::services::discord::turn_bridge) fn body_is_exact_self_duplicate(body: &str) -> bool {
    let bytes = body.as_bytes();
    if bytes.len() < SELF_DUPLICATION_MIN_LEN {
        return false;
    }
    SELF_DUPLICATION_SEPARATORS
        .iter()
        .any(|separator| doubled_around(bytes, separator.as_bytes()))
}

/// `bytes == half ++ separator ++ half` for a non-degenerate `half`.
fn doubled_around(bytes: &[u8], separator: &[u8]) -> bool {
    let Some(remainder) = bytes.len().checked_sub(separator.len()) else {
        return false;
    };
    if !remainder.is_multiple_of(2) {
        return false;
    }
    let half = remainder / 2;
    if half == 0 {
        return false;
    }
    // Cheapest discriminator first: the separator is 0-2 bytes, the halves are
    // the whole body. This keeps the per-streaming-tick cost at one memcmp for
    // the separator-less case and a couple of byte loads for the others.
    if &bytes[half..half + separator.len()] != separator {
        return false;
    }
    let (first, second) = (&bytes[..half], &bytes[half + separator.len()..]);
    first == second && !is_blank(first) && !has_shorter_repeating_period(first)
}

fn record_from_parts(
    site: BodyMutationSite,
    before_len: usize,
    prefix_len: usize,
    after: &str,
) -> BodyMutationRecord {
    let after_bytes = after.as_bytes();
    let over_limit = after_bytes.len() > BODY_MUTATION_DIGEST_LIMIT;
    let (delta_sha8, body_sha8) = if over_limit {
        (DIGEST_OVER_LIMIT.to_string(), DIGEST_OVER_LIMIT.to_string())
    } else {
        (sha8(&after_bytes[prefix_len..]), sha8(after_bytes))
    };
    BodyMutationRecord {
        site,
        before_len,
        after_len: after_bytes.len(),
        prefix_len,
        delta_sha8,
        body_sha8,
        self_duplicate: body_is_exact_self_duplicate(after),
    }
}

/// Build the record for a mutation that replaced the body wholesale.
pub(in crate::services::discord::turn_bridge) fn body_mutation_record(
    site: BodyMutationSite,
    before: &str,
    after: &str,
) -> BodyMutationRecord {
    let prefix_len = common_prefix_len(before.as_bytes(), after.as_bytes());
    record_from_parts(site, before.len(), prefix_len, after)
}

/// Build the record for a mutation that only appended, where the caller already
/// knows the retained prefix length. Saves cloning the accumulated body on the
/// streaming hot path.
///
/// The caller is asserting that `after` starts with the `before_len` bytes it
/// previously held; `append_streamed_text_chunk` satisfies this by construction
/// because every branch of it is a `push_str`.
pub(in crate::services::discord::turn_bridge) fn body_append_record(
    site: BodyMutationSite,
    before_len: usize,
    after: &str,
) -> BodyMutationRecord {
    record_from_parts(site, before_len, before_len.min(after.len()), after)
}

fn publish(record: &BodyMutationRecord, correlation: BodyMutationCorrelation<'_>) {
    // Same spelling and same `user_msg_id != 0` guard as
    // `inflight::turn_id_for_state`, so a stored violation joins against the
    // rows the rest of the observability surface already writes.
    let turn_id = match (correlation.channel_id, correlation.user_msg_id) {
        (Some(channel_id), Some(user_msg_id)) if user_msg_id != 0 => {
            Some(format!("discord:{channel_id}:{user_msg_id}"))
        }
        _ => None,
    };
    tracing::info!(
        target: BODY_MUTATION_TARGET,
        site = record.site.as_str(),
        before_len = record.before_len,
        after_len = record.after_len,
        prefix_len = record.prefix_len,
        delta_sha8 = %record.delta_sha8,
        body_sha8 = %record.body_sha8,
        self_duplicate = record.self_duplicate,
        "turn_bridge full_response body mutation"
    );
    record_invariant_check(
        !record.self_duplicate,
        InvariantViolation {
            provider: correlation.provider,
            channel_id: correlation.channel_id,
            // The tracing LINE inherits these from the enclosing
            // `discord_turn_bridge` span; the STORED event inherits nothing, so
            // it gets them from the row the site is holding. The streaming
            // append site still has no row and still stores `None` (see
            // `BodyMutationCorrelation::unavailable`).
            dispatch_id: correlation.dispatch_id,
            session_key: correlation.session_key,
            turn_id: turn_id.as_deref(),
            invariant: BODY_NOT_SELF_DUPLICATED_INVARIANT,
            code_location: record.site.code_location(),
            message: "turn_bridge full_response is its own first half repeated twice (#5938)",
            details: serde_json::json!({
                "site": record.site.as_str(),
                "before_len": record.before_len,
                "after_len": record.after_len,
                "prefix_len": record.prefix_len,
                "delta_sha8": record.delta_sha8,
                "body_sha8": record.body_sha8,
            }),
        },
    );
}

/// The invariant name the #5938 violation is filed under. Named so the tests
/// that prove the violation actually fires can assert the emitted record rather
/// than the call site's source text.
pub(in crate::services::discord::turn_bridge) const BODY_NOT_SELF_DUPLICATED_INVARIANT: &str =
    "turn_bridge_body_not_self_duplicated";

/// Record a wholesale body replacement. Never mutates either argument.
pub(in crate::services::discord::turn_bridge) fn observe_body_mutation(
    site: BodyMutationSite,
    correlation: BodyMutationCorrelation<'_>,
    before: &str,
    after: &str,
) {
    publish(&body_mutation_record(site, before, after), correlation);
}

/// Record an append whose retained prefix length the caller already knows.
/// Never mutates the argument.
pub(in crate::services::discord::turn_bridge) fn observe_body_append(
    site: BodyMutationSite,
    correlation: BodyMutationCorrelation<'_>,
    before_len: usize,
    after: &str,
) {
    publish(&body_append_record(site, before_len, after), correlation);
}

// Visible across `turn_bridge` in test builds ONLY so the per-site tests can
// live next to the production functions they drive (`tool_arms/authority_tests`,
// `queue_retry_silence`, `empty_response_recovery/handler`) and still share one
// tracing-capture harness, rather than each widening a production item's
// visibility to reach this file.
#[cfg(test)]
#[path = "body_mutation_telemetry_tests.rs"]
pub(in crate::services::discord::turn_bridge) mod body_mutation_telemetry_tests;
