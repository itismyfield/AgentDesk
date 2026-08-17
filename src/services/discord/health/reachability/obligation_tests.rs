//! Canonical framing tests, and the Rust half of the Rust↔Python equivalence
//! gate (#5071 T4-B2a = 4987 §-1.5 blocker B1′).
//!
//! The equivalence is proved through the golden corpus rather than by running
//! both runtimes in one process: this file asserts Rust's bytes equal
//! `tests/fixtures/relay_obligation/<case>.expected`, and
//! `scripts/check_reachability_canonical_equivalence.py` asserts Python's bytes
//! equal the same files. Equality with a common third value IS byte-equality
//! between the two, and it is the shape 4987 §2.4 asks for ("both validated
//! against the same golden corpus"). It also makes a one-sided mutation die on
//! exactly one side, which is what the design row requires — a mutation runner
//! that changed BOTH implementations identically would leave the corpus red
//! too, because the corpus is a third party to both.

use std::path::{Path, PathBuf};

use super::*;

const DEV: u64 = 16_777_232;
const INO: u64 = 90_210_123;
const GENERATION: i64 = 1_786_000_000_123_456_789;

fn identity() -> TranscriptFileId {
    TranscriptFileId { dev: DEV, ino: INO }
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/relay_obligation")
}

/// One corpus case, as declared in `cases.json`.
struct Case {
    name: String,
    generation_mtime_ns: i64,
    dev: u64,
    ino: u64,
    base_offset: u64,
    oversized_line_limit: u64,
}

fn corpus() -> Vec<Case> {
    let raw = std::fs::read_to_string(fixture_root().join("cases.json"))
        .expect("the corpus manifest must exist; the gate has no meaning without it");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("cases.json parses");
    let entries = parsed.as_array().expect("cases.json is an array");
    entries
        .iter()
        .map(|entry| Case {
            name: entry["name"].as_str().expect("name").to_string(),
            generation_mtime_ns: entry["generation_mtime_ns"].as_i64().expect("generation"),
            dev: entry["dev"].as_u64().expect("dev"),
            ino: entry["ino"].as_u64().expect("ino"),
            base_offset: entry["base_offset"].as_u64().expect("base_offset"),
            oversized_line_limit: entry["oversized_line_limit"]
                .as_u64()
                .expect("oversized_line_limit"),
        })
        .collect()
}

fn scan_bytes(bytes: &[u8]) -> ObligationScan {
    scan_canonical(bytes, 0, GENERATION, identity(), 1024 * 1024)
}

fn reasons(scan: &ObligationScan) -> Vec<ObligationReason> {
    scan.records.iter().map(|record| record.reason).collect()
}

/// The gate. Every case in the corpus, byte for byte.
///
/// A non-empty corpus is asserted first: a gate that silently found zero cases
/// would pass forever, which is the vacuous-selection failure #5071's §4.1
/// gate 3 exists to forbid.
#[test]
fn canonical_output_matches_the_golden_corpus_byte_for_byte() {
    let cases = corpus();
    assert!(
        cases.len() >= 10,
        "the corpus lost cases; it declares the shapes both implementations \
         are pinned on, and an empty one passes vacuously"
    );
    for case in cases {
        let input = std::fs::read(fixture_root().join(format!("{}.jsonl", case.name)))
            .unwrap_or_else(|_| panic!("input for case {}", case.name));
        let expected = std::fs::read(fixture_root().join(format!("{}.expected", case.name)))
            .unwrap_or_else(|_| panic!("expected output for case {}", case.name));
        let scan = scan_canonical(
            &input,
            case.base_offset,
            case.generation_mtime_ns,
            TranscriptFileId {
                dev: case.dev,
                ino: case.ino,
            },
            case.oversized_line_limit,
        );
        let actual = encode_canonical(&scan);
        assert_eq!(
            actual.as_bytes(),
            expected.as_slice(),
            "case {} diverged from the golden corpus.\n--- rust ---\n{}\n--- golden ---\n{}",
            case.name,
            actual,
            String::from_utf8_lossy(&expected),
        );
    }
}

/// The corpus pins the classification of every line; this pins that the corpus
/// still contains at least one line of each reason the schema defines, so a
/// future edit cannot quietly drop the case that covers a reason and leave the
/// equivalence gate agreeing about nothing.
#[test]
fn the_corpus_exercises_every_canonical_reason() {
    let mut seen: Vec<String> = Vec::new();
    for case in corpus() {
        let expected =
            std::fs::read_to_string(fixture_root().join(format!("{}.expected", case.name)))
                .unwrap_or_else(|_| panic!("expected output for case {}", case.name));
        for line in expected.lines().skip(1) {
            if line.starts_with(CANONICAL_NEXT_OFFSET_KEY) {
                continue;
            }
            let reason = line
                .rsplit('\t')
                .next()
                .expect("a reason column")
                .to_string();
            if !seen.contains(&reason) {
                seen.push(reason);
            }
        }
    }
    for reason in [
        ObligationReason::AssistantText,
        ObligationReason::PartialLine,
        ObligationReason::OversizedLine,
        ObligationReason::BlankLine,
        ObligationReason::MalformedJson,
        ObligationReason::NonAssistantRecord,
        ObligationReason::HarnessControl,
        ObligationReason::UnparsableTimestamp,
        ObligationReason::NoAssistantText,
    ] {
        assert!(
            seen.iter().any(|found| found == reason.as_canonical_str()),
            "no corpus case produces {}; the equivalence gate cannot compare a \
             classification neither side is asked to make",
            reason.as_canonical_str()
        );
    }
}

#[test]
fn an_assistant_text_record_is_the_only_obligation() {
    let scan = scan_bytes(
        b"{\"type\":\"assistant\",\"timestamp\":\"2026-08-17T01:02:03\",\
          \"message\":{\"model\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    );
    assert_eq!(reasons(&scan), vec![ObligationReason::AssistantText]);
    assert_eq!(scan.obligations().count(), 1);
    assert!(!scan.observation_is_incomplete());
}

/// 4987 §2.4: the harness-control exclusion is on the SYNTHETIC MODEL MARKER,
/// not on the banner text, because a real answer may contain the same words.
#[test]
fn a_synthetic_model_record_is_harness_control_not_an_obligation() {
    let scan = scan_bytes(
        b"{\"type\":\"assistant\",\"timestamp\":\"2026-08-17T01:02:03\",\
          \"message\":{\"model\":\"<synthetic>\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    );
    assert_eq!(reasons(&scan), vec![ObligationReason::HarnessControl]);
    assert_eq!(scan.obligations().count(), 0);
}

/// The ladder's ORDER is schema, not detail: a harness row with a broken
/// timestamp satisfies two rungs, and the two implementations only agree
/// because both ask about the marker first.
#[test]
fn harness_control_is_decided_before_the_timestamp() {
    let scan = scan_bytes(
        b"{\"type\":\"assistant\",\"timestamp\":\"nope\",\
          \"message\":{\"model\":\"<synthetic>\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    );
    assert_eq!(reasons(&scan), vec![ObligationReason::HarnessControl]);
}

#[test]
fn crlf_keeps_both_terminator_bytes_inside_the_range() {
    let scan = scan_bytes(b"{\"type\":\"user\"}\r\n\r\n");
    assert_eq!(
        reasons(&scan),
        vec![
            ObligationReason::NonAssistantRecord,
            ObligationReason::BlankLine
        ]
    );
    assert_eq!((scan.records[0].start, scan.records[0].end), (0, 17));
    assert_eq!((scan.records[1].start, scan.records[1].end), (17, 19));
    assert_eq!(scan.next_offset, 19);
}

/// Only ONE `\r` is stripped: `\r\r\n` leaves a `\r` in the content, which is
/// not empty and therefore not a blank line. Pinned because "strip trailing
/// carriage returns" is the natural mis-implementation and it silently changes
/// a classification.
#[test]
fn only_one_carriage_return_is_stripped() {
    let scan = scan_bytes(b"\r\r\n");
    assert_eq!(reasons(&scan), vec![ObligationReason::MalformedJson]);
}

#[test]
fn a_partial_line_holds_the_cursor_at_its_first_byte() {
    let scan = scan_bytes(b"{\"type\":\"user\"}\n{\"type\":\"assi");
    assert_eq!(
        reasons(&scan),
        vec![
            ObligationReason::NonAssistantRecord,
            ObligationReason::PartialLine
        ]
    );
    assert_eq!(
        scan.next_offset, 16,
        "the cursor must stop AT the partial line so the next read frames it whole"
    );
    assert_eq!(scan.obligations().count(), 0);
}

/// A chunk boundary inside a UTF-8 sequence must not reach a decoder at all.
/// The bytes are re-read whole by the next read, so half a character can never be
/// classified — and, in particular, can never be classified as malformed.
#[test]
fn a_chunk_cut_inside_a_multibyte_character_is_only_a_partial_line() {
    let mut bytes = "{\"type\":\"user\",\"t\":\"한\"}".as_bytes().to_vec();
    bytes.truncate(bytes.len() - 4);
    assert!(
        std::str::from_utf8(&bytes).is_err(),
        "fixture must be cut mid-character"
    );
    let scan = scan_bytes(&bytes);
    assert_eq!(reasons(&scan), vec![ObligationReason::PartialLine]);
    assert_eq!(scan.next_offset, 0);
}

/// Ranges are byte offsets, so a multi-byte answer's `end` exceeds its
/// character count. Pinned because a `chars()`-based implementation passes
/// every ASCII test and then hands the receipt subtraction offsets that do not
/// exist in the file.
#[test]
fn ranges_are_byte_offsets_not_character_counts() {
    let line = "{\"type\":\"assistant\",\"timestamp\":\"2026-08-17T01:02:03\",\
                \"message\":{\"model\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"한글\"}]}}\n";
    let scan = scan_bytes(line.as_bytes());
    assert_eq!(reasons(&scan), vec![ObligationReason::AssistantText]);
    assert_eq!(scan.records[0].end, line.len() as u64);
    assert!(
        line.len() > line.chars().count(),
        "fixture must actually contain multi-byte characters"
    );
}

/// An unterminated run at the limit passes rather than pinning the cursor: a
/// line longer than one bounded read can never show its terminator, so
/// refusing to advance would stall the observation permanently.
#[test]
fn an_oversized_unterminated_run_advances_the_cursor_and_marks_the_tick_incomplete() {
    let bytes = vec![b'x'; 64];
    let scan = scan_canonical(&bytes, 100, GENERATION, identity(), 64);
    assert_eq!(reasons(&scan), vec![ObligationReason::OversizedLine]);
    assert_eq!(scan.next_offset, 164);
    assert!(scan.observation_is_incomplete());
    assert_eq!(scan.obligations().count(), 0);

    // One byte under the limit is an ordinary partial line, and holds.
    let bytes = vec![b'x'; 63];
    let scan = scan_canonical(&bytes, 100, GENERATION, identity(), 64);
    assert_eq!(reasons(&scan), vec![ObligationReason::PartialLine]);
    assert_eq!(scan.next_offset, 100);
    assert!(!scan.observation_is_incomplete());
}

/// The limit applies to the unterminated remainder alone. Measuring it against
/// the whole chunk would make a busy channel's every tick look oversized.
#[test]
fn the_oversized_limit_measures_the_remainder_not_the_chunk() {
    let mut bytes = vec![b'a'; 60];
    bytes.push(b'\n');
    bytes.extend(std::iter::repeat_n(b'b', 10));
    let scan = scan_canonical(&bytes, 0, GENERATION, identity(), 64);
    assert_eq!(
        reasons(&scan),
        vec![
            ObligationReason::MalformedJson,
            ObligationReason::PartialLine
        ]
    );
    assert_eq!(scan.next_offset, 61);
}

#[test]
fn an_empty_chunk_produces_the_header_and_nothing_else() {
    let scan = scan_bytes(b"");
    assert!(scan.records.is_empty());
    assert_eq!(scan.next_offset, 0);
    assert_eq!(
        encode_canonical(&scan),
        format!("{CANONICAL_SCHEMA_HEADER}\n{CANONICAL_NEXT_OFFSET_KEY}\t0\n")
    );
}

/// Identity and generation are carried into every record, so byte-identical
/// content under a different incarnation encodes differently. This is what
/// keeps a rotated transcript's offsets from being comparable with the old
/// file's (4987 §-1.3).
#[test]
fn identical_bytes_under_a_different_incarnation_encode_differently() {
    let line = b"{\"type\":\"user\"}\n";
    let first = scan_canonical(line, 0, 1, TranscriptFileId { dev: 1, ino: 2 }, 1024);
    let second = scan_canonical(line, 0, 2, TranscriptFileId { dev: 1, ino: 2 }, 1024);
    let third = scan_canonical(line, 0, 1, TranscriptFileId { dev: 1, ino: 3 }, 1024);
    assert_ne!(
        encode_canonical(&first),
        encode_canonical(&second),
        "generation must be part of the encoding"
    );
    assert_ne!(
        encode_canonical(&first),
        encode_canonical(&third),
        "file identity must be part of the encoding"
    );
}

#[test]
fn a_text_block_that_trims_to_nothing_is_not_an_obligation() {
    let scan = scan_bytes(
        b"{\"type\":\"assistant\",\"timestamp\":\"2026-08-17T01:02:03\",\
          \"message\":{\"model\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"   \"}]}}\n",
    );
    assert_eq!(reasons(&scan), vec![ObligationReason::NoAssistantText]);
}

#[test]
fn one_non_blank_block_among_blank_ones_is_an_obligation() {
    let scan = scan_bytes(
        b"{\"type\":\"assistant\",\"timestamp\":\"2026-08-17T01:02:03\",\
          \"message\":{\"model\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\" \"},\
          {\"type\":\"text\",\"text\":\"real\"}]}}\n",
    );
    assert_eq!(reasons(&scan), vec![ObligationReason::AssistantText]);
}

/// The timestamp rule is the corpus's most divergence-prone rung, because the
/// Python half runs `time.strptime`, which accepts single-digit fields, and
/// nothing forces a Rust date parser to agree. Pinned directly as well as
/// through the corpus.
#[test]
fn the_timestamp_rung_accepts_what_the_python_half_accepts() {
    for (timestamp, expected) in [
        ("2026-08-17T01:02:03.456Z", ObligationReason::AssistantText),
        ("2026-08-17T01:02:03", ObligationReason::AssistantText),
        ("2026-8-7T1:2:3", ObligationReason::AssistantText),
        ("2026-13-17T01:02:03", ObligationReason::UnparsableTimestamp),
        ("2026-08-17", ObligationReason::UnparsableTimestamp),
        ("", ObligationReason::UnparsableTimestamp),
        (
            "not-a-timestamp-at-all",
            ObligationReason::UnparsableTimestamp,
        ),
    ] {
        let line = format!(
            "{{\"type\":\"assistant\",\"timestamp\":\"{timestamp}\",\
             \"message\":{{\"model\":\"m\",\"content\":[{{\"type\":\"text\",\"text\":\"t\"}}]}}}}\n"
        );
        assert_eq!(
            reasons(&scan_bytes(line.as_bytes())),
            vec![expected],
            "timestamp {timestamp:?}"
        );
    }
}

#[test]
fn json_that_is_not_an_object_is_a_non_assistant_record_not_malformed() {
    for line in [&b"[]\n"[..], b"\"str\"\n", b"123\n", b"null\n"] {
        assert_eq!(
            reasons(&scan_bytes(line)),
            vec![ObligationReason::NonAssistantRecord],
            "line {:?}",
            String::from_utf8_lossy(line)
        );
    }
}

#[test]
fn a_line_that_is_not_utf8_is_malformed_json() {
    let scan = scan_bytes(b"{\"type\":\"assistant\",\"t\":\"\xff\xfe\"}\n");
    assert_eq!(reasons(&scan), vec![ObligationReason::MalformedJson]);
}

/// A residual difference, pinned rather than asserted.
///
/// `scripts/check_reachability_canonical_equivalence.py` lists the shapes on
/// which the two halves are known NOT to agree, and one of them is that
/// Python's `json` accepts the non-RFC-8259 literals while `serde_json`'s value
/// parser rejects them: `NaN` alone on a line is a float — hence a
/// `NON_ASSISTANT_RECORD` — over there, and unparsable here. No corpus case can
/// reach it, so this test is what keeps the claim measured. If serde_json ever
/// starts accepting these, this goes red and the residual list is corrected in
/// the same change rather than rotting into a false statement.
#[test]
fn the_json_parsers_disagree_about_the_non_rfc_literals() {
    for line in [&b"NaN\n"[..], b"Infinity\n", b"-Infinity\n"] {
        assert_eq!(
            reasons(&scan_bytes(line)),
            vec![ObligationReason::MalformedJson],
            "line {:?}",
            String::from_utf8_lossy(line)
        );
    }
}
