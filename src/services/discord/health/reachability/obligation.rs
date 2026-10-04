//! Canonical obligation extraction — 4987 S1 second half + blocker B1′
//! (#5071 T4-B2a).
//!
//! Durable-obligation framing is checked against the Rust golden corpus.
//!
//! Canonical schema (4987 §-1.5): one record per physical line,
//! `(generation, start, end, identity, reason)`. `start`/`end` are absolute
//! half-open byte offsets (the receipt's `IncarnationRange` coordinate);
//! `identity` is `(dev, ino)`, so a rotation cannot make two files' offsets
//! comparable; `reason` is emitted for EVERY line, not only obligations, so a
//! silently-dropped record type cannot pass the golden-corpus test unnoticed.
//! See [`classify_line`] for the framing ladder (partial line, CRLF,
//! multi-byte, rotation, oversized line).

use super::discovery::TranscriptFileId;

/// Versioned encoding header pinned by the golden-corpus fixture test.
pub(in crate::services::discord) const CANONICAL_SCHEMA_HEADER: &str =
    "relay_obligation_canonical_v1";

/// Trailer key. Not a number, so it can never be mistaken for a record row,
/// every one of which starts with the generation.
pub(in crate::services::discord) const CANONICAL_NEXT_OFFSET_KEY: &str = "next_offset";

/// Harness-authored rows carry this marker; banner text alone is not evidence.
const HARNESS_CONTROL_MODEL: &str = "<synthetic>";

/// Timestamp format applied to the first 19 characters.
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
    /// golden-corpus test compares, so they are written out one arm at a time
    /// with no catch-all: a new reason cannot reach the corpus without someone
    /// choosing its spelling here and in the golden corpus.
    ///
    /// Consumed by the encoding, hence by the golden corpus — see
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
/// Typed accessors classify wrong-typed content as absent instead of panicking.
/// The `schema_type_blocks` corpus case pins this behavior.
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

/// Parse the first 19 Unicode characters with the transcript timestamp format.
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
