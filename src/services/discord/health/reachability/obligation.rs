//! Canonical obligation extraction — 4987 S1 second half + blocker B1′
//! (#5071 T4-B2a).
//!
//! # This file is INACTIVE, like the rest of the tree
//!
//! Nothing in production calls anything below. The [`super`] module docs state
//! the rule for the whole tree and this slice keeps it: T4-B2a lands the
//! canonical framing and the machine that proves the two implementations of it
//! agree, T4-B2b lands the durable ledger, T4-B2c wires the observation task,
//! and only then does 4987 S1 observation start. The consumers that exist
//! today are this file's own tests and
//! `scripts/check_reachability_canonical_equivalence.py`.
//!
//! 4987 §2.2 names the term the relay never had: a *durable obligation*, the
//! "what should have been delivered" side of the subtraction. §2.4 then names
//! the hazard in computing it twice — the in-band Rust prober and the
//! out-of-band Python watchdog would each define "assistant text block" their
//! own way, and then one of the two oracles is always wrong. This file is the
//! Rust half of the single rule; `scripts/relay_watchdog.py`'s
//! `canonical_obligation_records` is the Python half; and
//! `tests/fixtures/relay_obligation/` is the golden corpus both are compared
//! against byte for byte.
//!
//! # The canonical schema
//!
//! One record per physical line, five fields, exactly as 4987 §-1.5 spells it:
//! `(generation, start, end, identity, reason)`.
//!
//! ```text
//! relay_obligation_canonical_v1
//! <generation_mtime_ns>\t<start>\t<end>\t<dev>:<ino>\t<REASON>
//! next_offset\t<where the cursor resumes>
//! ```
//!
//! The `next_offset` trailer is part of the compared bytes, not a convenience.
//! The framing rules below are ALL cursor rules — a partial line holds the
//! cursor, an oversized run passes it — and an encoding of the records alone
//! cannot see the difference. Measured, not assumed: with the trailer omitted,
//! the mutation that makes a partial line advance the cursor SURVIVED the whole
//! corpus.
//!
//! * `start`/`end` are ABSOLUTE byte offsets into the transcript, half-open,
//!   and `end` includes the line terminator. Byte offsets, not block indices,
//!   because a receipt covers a byte range (4987 §-1.3 `IncarnationRange`) and
//!   the subtraction has to be defined on the same coordinate the receipt uses.
//! * `identity` is the `(dev, ino)` of the file the bytes were read from, so a
//!   rotation cannot make two different files' offsets comparable.
//! * `reason` is emitted for EVERY line, not only for obligations. Recording
//!   the skips is what makes the equivalence gate load-bearing: an
//!   implementation that started silently dropping harness-control records
//!   would otherwise agree with the other one on the obligation lines and pass.
//!
//! # Framing (the edge cases 4987 §-1.5 assigns to this slice)
//!
//! * **Partial line** — a chunk that ends without a terminator emits
//!   [`ObligationReason::PartialLine`] and does NOT advance
//!   [`ObligationScan::next_offset`] past it, so the next read takes those
//!   bytes as the head of the completed line. A partial line is never an
//!   obligation: half a JSON record is not evidence of anything.
//! * **CRLF** — the terminator is `\n`; exactly one immediately preceding `\r`
//!   is stripped before classification, and both bytes stay inside `[start,
//!   end)`. A line that is only `\r\n` is [`ObligationReason::BlankLine`].
//! * **Multi-byte** — splitting on `0x0A` cannot land inside a UTF-8 multi-byte
//!   sequence (continuation bytes are `0x80..=0xBF`), so line framing is
//!   codepoint-safe by construction, and a chunk boundary that does fall
//!   mid-character can only produce a `PartialLine`, whose bytes are re-read
//!   whole. Ranges are byte offsets throughout; no codepoint index is ever
//!   emitted.
//! * **Rotation** — handled by carrying `identity` in every record rather than
//!   by anything this function does: [`super::tail::read_incremental`] refuses
//!   to resume a cursor whose `(dev, ino)` moved. This file only guarantees
//!   that a record can never be attributed to a file it was not read from.
//! * **Oversized line** — a single line longer than `oversized_line_limit`
//!   would otherwise pin `next_offset` forever, because no bounded read could
//!   ever see its terminator. It is emitted as
//!   [`ObligationReason::OversizedLine`] and the cursor advances. It is NOT an
//!   obligation, and [`ObligationScan::observation_is_incomplete`] reports it
//!   so that a reader of the scan can spell it `Unknown{ReadTruncated}` — the
//!   read did not see a whole record, and 4987 §-1.4 makes "did not see"
//!   non-GREEN rather than absent. That reader is a later slice's; this one
//!   only makes the fact available.
//!
//! # What the equivalence gate proves, and over what
//!
//! It proves that both implementations produce identical bytes **for the cases
//! in the golden corpus**. It is not a proof over all inputs: the two runtimes
//! do not share a JSON parser or a Unicode whitespace table, so the corpus is
//! where each disputable shape has to be written down. `scripts/
//! check_reachability_canonical_equivalence.py` states the residual differences
//! it knows about.

use super::discovery::TranscriptFileId;

/// First line of every canonical encoding. A version, not decoration: the
/// equivalence gate compares whole files, so a schema change that both sides
/// make in lockstep still has to move this string, and a fixture regenerated
/// under an old schema stops matching.
///
/// The canonical ENCODING exists to be compared against the Python half, so
/// its consumers are this file's fixture test and
/// `scripts/check_reachability_canonical_equivalence.py` — a later slice's
/// observation task will consume [`scan_canonical`], not its serialization.
pub(in crate::services::discord) const CANONICAL_SCHEMA_HEADER: &str =
    "relay_obligation_canonical_v1";

/// Trailer key. Not a number, so it can never be mistaken for a record row,
/// every one of which starts with the generation.
pub(in crate::services::discord) const CANONICAL_NEXT_OFFSET_KEY: &str = "next_offset";

/// The model identity Claude stamps on harness-authored assistant rows.
/// Mirrors `relay_watchdog.py`'s `is_harness_control_assistant_record`, whose
/// docstring records WHY the marker and not the banner text is the test: users
/// and real answers may legitimately contain the same words.
const HARNESS_CONTROL_MODEL: &str = "<synthetic>";

/// The transcript timestamp format, applied to the first 19 characters exactly
/// as `relay_watchdog.py`'s `parse_transcript_ts` does.
const TRANSCRIPT_TS_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";

/// How a physical line was classified. Every line gets exactly one.
///
/// The order of the ladder is part of the canonical schema, not an
/// implementation detail: a record can satisfy several of these at once (a
/// harness-control row with an unparsable timestamp, say), and the two
/// implementations only agree if they ask the questions in the same order.
/// [`classify_line`] documents the order; both halves follow it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ObligationReason {
    /// An assistant record carrying at least one non-empty text block. The one
    /// and only obligation-producing classification (4987 §2.3 E0).
    AssistantText,
    /// The chunk ended without a terminator. Re-read by the next read, never an
    /// obligation, and the cursor does not pass it.
    PartialLine,
    /// An unterminated run at or over `oversized_line_limit`. The cursor DOES
    /// pass it, because otherwise it could never pass anything again.
    OversizedLine,
    /// Empty after the terminator and one optional `\r` are removed.
    BlankLine,
    /// Not parsable as JSON, or not valid UTF-8.
    MalformedJson,
    /// Valid JSON that is not an object, or an object whose `type` is not
    /// `assistant`.
    NonAssistantRecord,
    /// An assistant row stamped with the synthetic harness model identity.
    HarnessControl,
    /// An assistant row whose `timestamp` does not parse.
    UnparsableTimestamp,
    /// An assistant row with no text block that survives whitespace trimming.
    NoAssistantText,
}

impl ObligationReason {
    /// The canonical spelling. These strings are the wire format the
    /// equivalence gate compares, so they are written out one arm at a time
    /// with no catch-all: a new reason cannot reach the corpus without someone
    /// choosing its spelling here and in the Python table.
    ///
    /// Consumed by the encoding, hence by the equivalence corpus — see
    /// [`CANONICAL_SCHEMA_HEADER`].
    pub(in crate::services::discord) fn as_canonical_str(self) -> &'static str {
        match self {
            Self::AssistantText => "ASSISTANT_TEXT",
            Self::PartialLine => "PARTIAL_LINE",
            Self::OversizedLine => "OVERSIZED_LINE",
            Self::BlankLine => "BLANK_LINE",
            Self::MalformedJson => "MALFORMED_JSON",
            Self::NonAssistantRecord => "NON_ASSISTANT_RECORD",
            Self::HarnessControl => "HARNESS_CONTROL",
            Self::UnparsableTimestamp => "UNPARSABLE_TIMESTAMP",
            Self::NoAssistantText => "NO_ASSISTANT_TEXT",
        }
    }

    /// Whether this line places the relay under an obligation to deliver.
    pub(in crate::services::discord) fn is_obligation(self) -> bool {
        matches!(self, Self::AssistantText)
    }

    /// Whether this line means the read did not see a whole record, so a
    /// reader of the scan must report `Unknown{ReadTruncated}` rather than
    /// treat the gap as "nothing was there". No such reader exists yet.
    pub(in crate::services::discord) fn observation_is_incomplete(self) -> bool {
        matches!(self, Self::OversizedLine)
    }
}

/// One canonical record: 4987 §-1.5's `(generation, start, end, identity,
/// reason)`, carried and never interpreted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct CanonicalRecord {
    pub(in crate::services::discord) generation_mtime_ns: i64,
    pub(in crate::services::discord) start: u64,
    pub(in crate::services::discord) end: u64,
    pub(in crate::services::discord) identity: TranscriptFileId,
    pub(in crate::services::discord) reason: ObligationReason,
}

/// What one scan produced, and where the cursor may resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ObligationScan {
    pub(in crate::services::discord) records: Vec<CanonicalRecord>,
    /// The first byte NOT consumed. Equal to the chunk end unless the chunk
    /// ended inside a line short of `oversized_line_limit`, in which case it
    /// points at that line's first byte so the next read frames it whole.
    pub(in crate::services::discord) next_offset: u64,
}

impl ObligationScan {
    /// The obligation-producing records, in file order.
    pub(in crate::services::discord) fn obligations(
        &self,
    ) -> impl Iterator<Item = &CanonicalRecord> {
        self.records
            .iter()
            .filter(|record| record.reason.is_obligation())
    }

    /// Whether any record means this tick's view of the tail has a hole in it.
    pub(in crate::services::discord) fn observation_is_incomplete(&self) -> bool {
        self.records
            .iter()
            .any(|record| record.reason.observation_is_incomplete())
    }
}

/// Classify one line's bytes, with the terminator and one optional `\r`
/// already removed.
///
/// The ladder, in the order both implementations ask it:
///
/// 1. empty ⇒ `BlankLine`;
/// 2. not JSON, or not UTF-8 ⇒ `MalformedJson`;
/// 3. not a JSON object, or `type != "assistant"` ⇒ `NonAssistantRecord`;
/// 4. `message.model == "<synthetic>"` ⇒ `HarnessControl`;
/// 5. `timestamp` does not parse ⇒ `UnparsableTimestamp`;
/// 6. no `message.content[]` entry of `type == "text"` with a non-blank
///    `text` ⇒ `NoAssistantText`;
/// 7. otherwise ⇒ `AssistantText`.
///
/// Steps 3–7 are `relay_watchdog.py`'s `_assistant_blocks_from_record` read as
/// a decision tree instead of as a filter — that function answers "which
/// blocks", this one answers "and if none, why not", which is the half the
/// canonical schema needs and the watchdog never had to name.
///
/// The typed accessors below are part of the agreement, not a Rust convenience:
/// `as_array` on `message.content` and `as_str` on a block's `text` make a
/// wrong-typed field read as ABSENT, and a JSONL transcript is not a
/// schema-checked channel, so those rows arrive. `relay_watchdog.py`'s
/// `_canonical_typed_content` narrows the same two fields for the same reason —
/// without it Python raises where this classifies — and the
/// `schema_type_blocks` corpus case pins the two answers together.
fn classify_line(line: &[u8]) -> ObligationReason {
    if line.is_empty() {
        return ObligationReason::BlankLine;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        return ObligationReason::MalformedJson;
    };
    let Some(record) = value.as_object() else {
        return ObligationReason::NonAssistantRecord;
    };
    if record.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
        return ObligationReason::NonAssistantRecord;
    }
    let message = record.get("message").and_then(serde_json::Value::as_object);
    if message
        .and_then(|message| message.get("model"))
        .and_then(serde_json::Value::as_str)
        == Some(HARNESS_CONTROL_MODEL)
    {
        return ObligationReason::HarnessControl;
    }
    let timestamp = record
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if !transcript_timestamp_parses(timestamp) {
        return ObligationReason::UnparsableTimestamp;
    }
    let Some(message) = message else {
        return ObligationReason::NoAssistantText;
    };
    let has_text = message
        .get("content")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block.as_object().is_some_and(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                        && !block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .is_empty()
                })
            })
        });
    if has_text {
        ObligationReason::AssistantText
    } else {
        ObligationReason::NoAssistantText
    }
}

/// Whether `relay_watchdog.py`'s `parse_transcript_ts` would accept this
/// string: its first 19 CHARACTERS (Python slices code points, so this one
/// does too) parsed as [`TRANSCRIPT_TS_FORMAT`].
fn transcript_timestamp_parses(timestamp: &str) -> bool {
    let head: String = timestamp.chars().take(19).collect();
    chrono::NaiveDateTime::parse_from_str(&head, TRANSCRIPT_TS_FORMAT).is_ok()
}

/// Frame `bytes` (read from `base_offset`) into canonical records.
///
/// `identity` and `generation_mtime_ns` are supplied by the caller because this
/// file stats nothing and reads no clock; they are the incarnation coordinates
/// of 4987 §-1.3 that the caller resolved before reading, which is what
/// [`super::discovery`] produces.
pub(in crate::services::discord) fn scan_canonical(
    bytes: &[u8],
    base_offset: u64,
    generation_mtime_ns: i64,
    identity: TranscriptFileId,
    oversized_line_limit: u64,
) -> ObligationScan {
    let mut records = Vec::new();
    let mut line_start = 0usize;

    let emit = |records: &mut Vec<CanonicalRecord>, start: usize, end: usize, reason| {
        records.push(CanonicalRecord {
            generation_mtime_ns,
            start: base_offset + start as u64,
            end: base_offset + end as u64,
            identity,
            reason,
        });
    };

    while let Some(offset) = bytes[line_start..].iter().position(|byte| *byte == b'\n') {
        let terminator = line_start + offset;
        let mut content_end = terminator;
        if content_end > line_start && bytes[content_end - 1] == b'\r' {
            content_end -= 1;
        }
        let reason = classify_line(&bytes[line_start..content_end]);
        emit(&mut records, line_start, terminator + 1, reason);
        line_start = terminator + 1;
    }

    let remainder = bytes.len() - line_start;
    if remainder == 0 {
        return ObligationScan {
            records,
            next_offset: base_offset + bytes.len() as u64,
        };
    }

    // An unterminated run this long can never be completed inside one bounded
    // read, so refusing to pass it would freeze the cursor permanently. Pass
    // it, classified, and let the caller report the tick as incomplete.
    if remainder as u64 >= oversized_line_limit {
        emit(
            &mut records,
            line_start,
            bytes.len(),
            ObligationReason::OversizedLine,
        );
        return ObligationScan {
            records,
            next_offset: base_offset + bytes.len() as u64,
        };
    }

    emit(
        &mut records,
        line_start,
        bytes.len(),
        ObligationReason::PartialLine,
    );
    ObligationScan {
        records,
        // Deliberately NOT past the partial line: the next read frames it whole.
        next_offset: base_offset + line_start as u64,
    }
}

/// Serialize a scan to the canonical byte stream. Always emits the header and
/// the trailer, including for an empty scan — "no records" and "no output" must
/// not look alike to a gate that compares files.
///
/// See [`CANONICAL_SCHEMA_HEADER`] for why the encoding is corpus-facing rather
/// than runtime-facing.
pub(in crate::services::discord) fn encode_canonical(scan: &ObligationScan) -> String {
    let mut out = String::from(CANONICAL_SCHEMA_HEADER);
    out.push('\n');
    for record in &scan.records {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}:{}\t{}\n",
            record.generation_mtime_ns,
            record.start,
            record.end,
            record.identity.dev,
            record.identity.ino,
            record.reason.as_canonical_str(),
        ));
    }
    out.push_str(&format!(
        "{CANONICAL_NEXT_OFFSET_KEY}\t{}\n",
        scan.next_offset
    ));
    out
}

#[cfg(test)]
#[path = "obligation_tests.rs"]
mod tests;
