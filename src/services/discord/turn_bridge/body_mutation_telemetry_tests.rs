//! #5938 body-mutation telemetry tests.
//!
//! These pin four things the instrumentation has to keep being true:
//! both mutation sites emit a record, the self-duplication predicate separates
//! a doubled body from an ordinary even-length one, multi-byte UTF-8 across the
//! midpoint does not panic, and the observation never changes the body.

use super::*;
use crate::services::discord::turn_bridge::bridge_entry_persist::adopt_full_response_from_inflight_row;
use crate::services::discord::turn_bridge::chunk_compose::append_streamed_text_chunk;
use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct CapturingWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `body` with a thread-local tracing subscriber and return what it logged.
fn captured_logs<F: FnOnce()>(body: F) -> String {
    let writer = CapturingWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = writer.buffer.lock().unwrap().clone();
    String::from_utf8(bytes).expect("captured log is utf-8")
}

/// A body long enough to clear `SELF_DUPLICATION_MIN_LEN` without being a
/// self-duplicate: 70 bytes, even length, both halves different.
const ORDINARY_EVEN_BODY: &str =
    "The quick brown fox jumps over the lazy dog while the cat naps nearby.";

#[test]
fn ordinary_even_body_fixture_is_even_and_long_enough_to_be_a_real_negative() {
    // Guards the negative test below from decaying into a vacuous one: if the
    // fixture stopped being even, or slipped under the length floor, it would
    // pass for the wrong reason.
    assert!(ORDINARY_EVEN_BODY.len().is_multiple_of(2));
    assert!(ORDINARY_EVEN_BODY.len() >= 64);
}

#[test]
fn append_site_emits_a_body_mutation_record() {
    let logs = captured_logs(|| {
        let mut body = String::from("COUNT-001\n");
        append_streamed_text_chunk(&mut body, "COUNT-002\n");
    });

    assert!(
        logs.contains("site=\"chunk_compose::append_streamed_text_chunk\""),
        "append site must publish a body-mutation record, got: {logs}"
    );
    assert!(logs.contains("before_len=10"), "got: {logs}");
    assert!(logs.contains("after_len=20"), "got: {logs}");
    assert!(logs.contains("delta_sha8="), "got: {logs}");
    assert!(logs.contains("body_sha8="), "got: {logs}");
}

#[test]
fn assignment_site_emits_a_body_mutation_record() {
    let logs = captured_logs(|| {
        let mut local = String::from("COUNT-001\n");
        adopt_full_response_from_inflight_row(&mut local, "COUNT-001\nCOUNT-002\n");
        assert_eq!(local, "COUNT-001\nCOUNT-002\n");
    });

    assert!(
        logs.contains(
            "site=\"bridge_entry_persist::reconcile_runtime_locals_from_inflight_state\""
        ),
        "assignment site must publish a body-mutation record, got: {logs}"
    );
    assert!(logs.contains("before_len=10"), "got: {logs}");
    assert!(logs.contains("after_len=20"), "got: {logs}");
}

/// The identification this instrumentation exists for: the watcher writing
/// `X` into the durable row and the bridge appending the same `X` produce the
/// SAME `delta_sha8` under two different `site` values.
#[test]
fn the_same_delta_hashes_identically_at_both_sites() {
    let seed = "COUNT-001\n".repeat(6);
    let delta = "COUNT-007\n";
    let grown = format!("{seed}{delta}");

    let appended = body_append_record(
        BodyMutationSite::AppendStreamedTextChunk,
        seed.len(),
        &grown,
    );
    let adopted = body_mutation_record(BodyMutationSite::ReconcileFromInflightState, &seed, &grown);

    assert_ne!(appended.site, adopted.site);
    assert_eq!(appended.delta_sha8, adopted.delta_sha8);
    assert_eq!(appended.body_sha8, adopted.body_sha8);
    assert_eq!(appended.prefix_len, seed.len());
    assert_eq!(adopted.prefix_len, seed.len());
}

#[test]
fn self_duplication_predicate_catches_an_exactly_doubled_body() {
    let single = "COUNT-".to_string() + &"0123456789".repeat(6);
    assert!(single.len() >= 64, "fixture must clear the length floor");
    let doubled = format!("{single}{single}");

    assert!(body_is_exact_self_duplicate(&doubled));
    assert!(
        body_mutation_record(
            BodyMutationSite::ReconcileFromInflightState,
            &single,
            &doubled,
        )
        .self_duplicate
    );
}

#[test]
fn self_duplication_predicate_ignores_an_ordinary_even_length_body() {
    assert!(!body_is_exact_self_duplicate(ORDINARY_EVEN_BODY));
    assert!(
        !body_mutation_record(
            BodyMutationSite::AppendStreamedTextChunk,
            "",
            ORDINARY_EVEN_BODY,
        )
        .self_duplicate
    );
}

#[test]
fn self_duplication_predicate_ignores_short_repeats_below_the_floor() {
    // Legitimate model text, not corruption — the floor exists so these do not
    // raise ERROR-level invariant violations on healthy turns.
    for body in ["\n\n", "  ", "byebye", "abab"] {
        assert!(
            !body_is_exact_self_duplicate(body),
            "{body:?} is under the length floor and must not be flagged"
        );
    }
}

/// `body[..len / 2]` panics when the midpoint lands inside a codepoint, and a
/// panic in observation-only instrumentation would itself be a P0. Every input
/// here puts a multi-byte character across the midpoint.
#[test]
fn multibyte_utf8_across_the_midpoint_does_not_panic() {
    let straddlers = [
        // 3-byte Hangul syllables; many of these have odd-length prefixes.
        "가나다라마바사아자차카타파하거너더러머버서어저처커터퍼허고노도로모보소오조초코토포호구"
            .to_string(),
        // Mixed ASCII + 4-byte emoji so the midpoint can fall mid-sequence.
        format!("{}🙂🙃😀😃😄😁😆😅🤣😂🙂🙃😀😃😄😁😆😅🤣😂", "x".repeat(23)),
        // Odd total length with a multi-byte tail.
        format!("{}한", "y".repeat(62)),
        // A genuinely doubled multi-byte body: the predicate must answer true
        // here without slicing through a codepoint.
        "한글 본문 반복 테스트 입니다 예시 문자열".repeat(2),
    ];

    for body in straddlers {
        // Must not panic; the value itself is not what this test pins.
        let _ = body_is_exact_self_duplicate(&body);
        let record = body_mutation_record(BodyMutationSite::AppendStreamedTextChunk, "", &body);
        assert_eq!(record.after_len, body.len());
        // A pathological byte-prefix can land inside a codepoint too.
        let mut mutated = body.clone();
        mutated.push('힣');
        let _ = body_mutation_record(
            BodyMutationSite::ReconcileFromInflightState,
            &body,
            &mutated,
        );
        let _ = body_append_record(
            BodyMutationSite::AppendStreamedTextChunk,
            body.len().saturating_sub(1),
            &mutated,
        );
    }
}

#[test]
fn the_doubled_multibyte_body_is_still_recognised_as_a_self_duplicate() {
    let half = "한글 본문 반복 테스트 입니다 예시 문자열";
    assert!(
        half.len() >= 32,
        "half must be long enough to double past 64"
    );
    let doubled = half.repeat(2);
    assert!(body_is_exact_self_duplicate(&doubled));
}

/// The instrumentation is observation only: it must not block, trim, rewrite or
/// otherwise touch the body at either site.
#[test]
fn observation_leaves_the_body_byte_identical() {
    // Append site: the #3608 boundary rules still decide the result, and the
    // observation adds nothing to it.
    let cases: [(&str, &str, &str); 4] = [
        ("", "hello", "hello"),
        ("a\n\n", "\n\nworld", "a\n\nworld"),
        (
            "```\ncode\n\n",
            "\n\nstill code",
            "```\ncode\n\n\n\nstill code",
        ),
        // Legitimately repeated text must survive verbatim — blocking it is the
        // #5941-class silent loss this PR refuses to create.
        ("REPEAT", "REPEAT", "REPEATREPEAT"),
    ];
    for (seed, chunk, expected) in cases {
        let mut body = String::from(seed);
        append_streamed_text_chunk(&mut body, chunk);
        assert_eq!(body, expected, "seed={seed:?} chunk={chunk:?}");
    }

    // Assignment site: the durable body is adopted whole, self-duplicate or
    // not, and the source is untouched.
    let durable = "COUNT-".to_string() + &"0123456789".repeat(6);
    let durable = durable.repeat(2);
    let mut local = String::from("COUNT-0123456789");
    adopt_full_response_from_inflight_row(&mut local, &durable);
    assert_eq!(local, durable);
    assert!(body_is_exact_self_duplicate(&local));

    // And the pure record builders never mutate their inputs.
    let before = String::from("before");
    let after = String::from("before-after");
    let _ = body_mutation_record(BodyMutationSite::AppendStreamedTextChunk, &before, &after);
    let _ = body_append_record(
        BodyMutationSite::ReconcileFromInflightState,
        before.len(),
        &after,
    );
    assert_eq!(before, "before");
    assert_eq!(after, "before-after");
}

#[test]
fn over_limit_bodies_skip_the_digests_but_keep_the_self_duplication_verdict() {
    // One byte past the 1 MiB digest bound, and an exact self-duplicate, so the
    // #5938 fingerprint has to survive the threshold that suppresses hashing.
    let half = "z".repeat(512 * 1024 + 1);
    let body = half.repeat(2);
    assert!(body.len() > 1024 * 1024);

    let record = body_mutation_record(BodyMutationSite::ReconcileFromInflightState, "", &body);
    assert_eq!(record.delta_sha8, "over-limit");
    assert_eq!(record.body_sha8, "over-limit");
    assert!(
        record.self_duplicate,
        "the digest threshold must never blind the #5938 fingerprint"
    );
    assert_eq!(record.after_len, body.len());
}

#[test]
fn record_shape_is_identical_at_both_sites() {
    // "같은 형태의 구조화 기록": the two sites differ only in `site`.
    let before = "seed-body-that-is-long-enough-to-matter";
    let after = "seed-body-that-is-long-enough-to-matter+delta";
    let appended = body_append_record(
        BodyMutationSite::AppendStreamedTextChunk,
        before.len(),
        after,
    );
    let adopted = body_mutation_record(BodyMutationSite::ReconcileFromInflightState, before, after);

    assert_eq!(appended.before_len, adopted.before_len);
    assert_eq!(appended.after_len, adopted.after_len);
    assert_eq!(appended.prefix_len, adopted.prefix_len);
    assert_eq!(appended.delta_sha8, adopted.delta_sha8);
    assert_eq!(appended.body_sha8, adopted.body_sha8);
    assert_eq!(appended.self_duplicate, adopted.self_duplicate);
    assert_eq!(
        appended.site.as_str(),
        "chunk_compose::append_streamed_text_chunk"
    );
    assert_eq!(
        adopted.site.as_str(),
        "bridge_entry_persist::reconcile_runtime_locals_from_inflight_state"
    );
}

/// A wholesale assignment that DISCARDS a suffix is recorded as such, so the
/// interleaving readout can tell "extended" from "replaced".
#[test]
fn a_shrinking_assignment_records_a_prefix_shorter_than_before_len() {
    let record = body_mutation_record(
        BodyMutationSite::ReconcileFromInflightState,
        "shared-prefix-LOCAL-TAIL",
        "shared-prefix-DURABLE",
    );
    assert_eq!(record.before_len, "shared-prefix-LOCAL-TAIL".len());
    assert_eq!(record.after_len, "shared-prefix-DURABLE".len());
    assert_eq!(record.prefix_len, "shared-prefix-".len());
    assert!(record.prefix_len < record.before_len);
}
