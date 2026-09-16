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
//! COVERAGE (why four sites, not two): the readout is a per-turn stream of
//! `before_len` / `after_len` pairs, and an analyst reads a gap between one
//! record's `after_len` and the next record's `before_len` as loss. Every
//! mutation of the bridge-local body must therefore appear, including the two
//! that SHRINK it — `append_tool_boundary_separator` (trailing-whitespace
//! truncate + `"\n\n"`, once per `StreamMessage::ToolUse`, whose result
//! `stream_loop/tool_arms.rs` writes straight into the durable row) and
//! `clear_response_delivery_state` (empty-sink rewind, which blanks the local
//! body and the row body together). Omitting them produced exactly the
//! `after_len=N` → `before_len=N-2` discontinuity this doc warns about.

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
#[derive(Debug, Clone, Copy, Default)]
pub(in crate::services::discord::turn_bridge) struct BodyMutationCorrelation<'a> {
    pub(in crate::services::discord::turn_bridge) provider: Option<&'a str>,
    pub(in crate::services::discord::turn_bridge) channel_id: Option<u64>,
}

impl<'a> BodyMutationCorrelation<'a> {
    /// Both keys, from the durable row that every reconcile/rewind site holds.
    pub(in crate::services::discord::turn_bridge) const fn new(
        provider: &'a str,
        channel_id: u64,
    ) -> Self {
        Self {
            provider: Some(provider),
            channel_id: Some(channel_id),
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
        }
    }
}

/// Which mutation site produced a record. These are ALL the places the
/// bridge-local `full_response` body changes shape.
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
            // The enclosing `discord_turn_bridge` span already carries
            // dispatch_id / session_key / turn_id on every line these sites
            // emit, and no site's signature can take them without reformatting a
            // call site that sits at a line cap.
            dispatch_id: None,
            session_key: None,
            turn_id: None,
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

#[cfg(test)]
#[path = "body_mutation_telemetry_tests.rs"]
mod body_mutation_telemetry_tests;
