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
//! Both mutation sites emit the SAME record shape. A single `delta_sha8`
//! appearing under two different `site` values inside one turn pins the double
//! write immediately — that identification is the whole point of the record.

use crate::services::observability::{InvariantViolation, record_invariant_check};
use sha2::{Digest, Sha256};

/// Tracing target for the per-mutation record. Kept under the `agentdesk`
/// prefix so the shipped `agentdesk=info` directive (`src/logging.rs`) admits
/// it without an operator changing `RUST_LOG`, while still being narrow enough
/// to filter on when reading `dcserver.stdout.log`.
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
/// bounded — it runs at every length, on every mutation, at both sites. This
/// threshold only suppresses the two correlation digests. The observed #5938
/// body was 1198 bytes (599 × 2) and a Discord-deliverable turn body is orders
/// of magnitude below 1 MiB, so in practice the digests are always present for
/// the class this instrumentation was written to identify; above the bound the
/// record still carries `site`, `before_len`, `after_len`, `prefix_len` and the
/// self-duplication verdict, which is enough to order the two writers.
const BODY_MUTATION_DIGEST_LIMIT: usize = 1024 * 1024;

/// Written into `delta_sha8` / `body_sha8` when the body exceeded
/// [`BODY_MUTATION_DIGEST_LIMIT`]. It cannot be confused with a real digest,
/// which is always exactly 8 lowercase hex characters.
const DIGEST_OVER_LIMIT: &str = "over-limit";

/// Shortest body the self-duplication predicate will flag.
///
/// Sub-threshold "self-duplicates" are ordinary text, not corruption: `"\n\n"`,
/// `"  "`, `"byebye"`, `"hahaha"` and every two-character repeat trivially
/// satisfy `first_half == second_half`. Flagging them would emit ERROR-level
/// invariant violations on healthy turns and bury the real signal. 64 bytes is
/// ~19× below the observed #5938 body (1198 bytes) and far below any
/// Discord-deliverable assistant turn, so the fingerprint this instrumentation
/// exists to catch is never suppressed by the floor.
const SELF_DUPLICATION_MIN_LEN: usize = 64;

/// Which mutation site produced a record. The two variants are the only two
/// places the bridge-local `full_response` body changes shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord::turn_bridge) enum BodyMutationSite {
    /// `chunk_compose::append_streamed_text_chunk` — the streamed `Text` append
    /// path, driven from `stream_loop/content_arms.rs`.
    AppendStreamedTextChunk,
    /// `bridge_entry_persist::reconcile_runtime_locals_from_inflight_state` —
    /// the whole-body assignment from the durable inflight row.
    ReconcileFromInflightState,
}

impl BodyMutationSite {
    pub(in crate::services::discord::turn_bridge) const fn as_str(self) -> &'static str {
        match self {
            Self::AppendStreamedTextChunk => "chunk_compose::append_streamed_text_chunk",
            Self::ReconcileFromInflightState => {
                "bridge_entry_persist::reconcile_runtime_locals_from_inflight_state"
            }
        }
    }

    const fn code_location(self) -> &'static str {
        match self {
            Self::AppendStreamedTextChunk => {
                "src/services/discord/turn_bridge/chunk_compose.rs:append_streamed_text_chunk"
            }
            Self::ReconcileFromInflightState => {
                "src/services/discord/turn_bridge/bridge_entry_persist.rs:reconcile_runtime_locals_from_inflight_state"
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

/// #5938 fingerprint: the body is exactly its own first half, twice.
///
/// Compares byte slices rather than `body[..len / 2] == body[len / 2..]`,
/// because the midpoint of a body containing multi-byte UTF-8 can land inside a
/// codepoint and `&str` slicing there panics. A panic path in observation-only
/// instrumentation would itself be a P0, so the byte form is the only form
/// used. The two answers agree whenever the `&str` form is legal, and when the
/// halves really are equal the midpoint is necessarily a char boundary.
pub(in crate::services::discord::turn_bridge) fn body_is_exact_self_duplicate(body: &str) -> bool {
    let bytes = body.as_bytes();
    let len = bytes.len();
    if len < SELF_DUPLICATION_MIN_LEN || !len.is_multiple_of(2) {
        return false;
    }
    let half = len / 2;
    bytes[..half] == bytes[half..]
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

fn publish(record: &BodyMutationRecord) {
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
            // The enclosing `discord_turn_bridge` span (turn_bridge/mod.rs:252)
            // already carries channel_id / provider / dispatch_id / session_key
            // / turn_id on every line these two sites emit, and neither site's
            // signature can take them: `content_arms.rs` sits exactly at its
            // 635-line namespace cap, so threading arguments through the append
            // call would require raising a cap, which this PR must not do.
            provider: None,
            channel_id: None,
            dispatch_id: None,
            session_key: None,
            turn_id: None,
            invariant: "turn_bridge_body_not_self_duplicated",
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

/// Record a wholesale body replacement. Never mutates either argument.
pub(in crate::services::discord::turn_bridge) fn observe_body_mutation(
    site: BodyMutationSite,
    before: &str,
    after: &str,
) {
    publish(&body_mutation_record(site, before, after));
}

/// Record an append whose retained prefix length the caller already knows.
/// Never mutates the argument.
pub(in crate::services::discord::turn_bridge) fn observe_body_append(
    site: BodyMutationSite,
    before_len: usize,
    after: &str,
) {
    publish(&body_append_record(site, before_len, after));
}

#[cfg(test)]
#[path = "body_mutation_telemetry_tests.rs"]
mod body_mutation_telemetry_tests;
