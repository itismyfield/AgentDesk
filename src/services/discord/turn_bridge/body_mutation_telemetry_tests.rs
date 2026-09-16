//! #5938 body-mutation telemetry tests.
//!
//! Four things have to keep being true and each has its own section below:
//! every mutation site emits a record, the self-duplication predicate separates
//! a doubled body from ordinary text AT THE SIZES THIS DEPLOYMENT ACTUALLY
//! PRODUCES, multi-byte UTF-8 across the midpoint does not panic, and the
//! observation never changes the body.
//!
//! The predicate tests deliberately drive the REAL composers
//! (`append_streamed_text_chunk`, `append_tool_boundary_separator`) instead of
//! hand-rolling separators, because the first revision of this fingerprint was
//! fitted to a single observed body whose halves happened to abut with no
//! separator, and hand-rolled fixtures are exactly what let that pass review.

use super::*;
use crate::services::discord::turn_bridge::bridge_entry_persist::adopt_full_response_from_inflight_row;
use crate::services::discord::turn_bridge::chunk_compose::{
    append_streamed_text_chunk, append_tool_boundary_separator,
};
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
///
/// Shared with the per-site tests that live next to their production functions
/// (`tool_arms/authority_tests`, `queue_retry_silence`,
/// `empty_response_recovery/handler`), so all of them assert the same emitted
/// artifact instead of re-deriving a capture harness each.
pub(in crate::services::discord::turn_bridge) fn captured_logs<F: FnOnce()>(body: F) -> String {
    let writer = CapturingWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = writer.buffer.lock().unwrap().clone();
    String::from_utf8(bytes).expect("captured log is utf-8")
}

/// Same, but through the filter the SHIPPED process installs.
///
/// `tracing_env_filter()` itself folds in `RUST_LOG`, which a developer or CI
/// lane may have set to anything, so the directive constant is applied on its
/// own here: this asks the exact question "does the shipped directive admit this
/// target", with no ambient input that could answer it accidentally.
fn captured_logs_under_production_filter<F: FnOnce()>(body: F) -> String {
    let writer = CapturingWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer.clone())
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            crate::logging::DEFAULT_TRACING_DIRECTIVE,
        ))
        .finish();
    tracing::subscriber::with_default(subscriber, body);
    let bytes = writer.buffer.lock().unwrap().clone();
    String::from_utf8(bytes).expect("captured log is utf-8")
}

fn correlation() -> BodyMutationCorrelation<'static> {
    BodyMutationCorrelation::new("codex", 4_259_612)
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
    assert!(ORDINARY_EVEN_BODY.len() >= SELF_DUPLICATION_MIN_LEN);
}

// ---------------------------------------------------------------------------
// Target admission (MS3): a target without the `agentdesk` prefix is dropped by
// the shipped filter, so the record would never exist in production while every
// unfiltered test still passed.
// ---------------------------------------------------------------------------

#[test]
fn production_filter_admits_the_body_mutation_target() {
    let logs = captured_logs_under_production_filter(|| {
        let mut body = String::from("COUNT-001\n");
        append_streamed_text_chunk(&mut body, "COUNT-002\n");
    });

    assert!(
        logs.contains("site=\"chunk_compose::append_streamed_text_chunk\""),
        "the shipped `{}` directive must admit `{BODY_MUTATION_TARGET}`; got: {logs}",
        crate::logging::DEFAULT_TRACING_DIRECTIVE,
    );
}

#[test]
fn a_target_outside_the_agentdesk_tree_would_be_dropped_by_the_shipped_filter() {
    // The counterfactual that gives the test above its teeth: the same record
    // emitted under a bare second-segment target is INVISIBLE in production.
    // Without this, "the filter admits it" could be true of any target at all.
    let logs = captured_logs_under_production_filter(|| {
        tracing::info!(target: "body_mutation", "turn_bridge full_response body mutation");
    });
    assert!(
        logs.is_empty(),
        "an unprefixed target must be rejected by `{}`; got: {logs}",
        crate::logging::DEFAULT_TRACING_DIRECTIVE,
    );
    assert!(
        BODY_MUTATION_TARGET.starts_with("agentdesk"),
        "the shipped target must keep the prefix the filter matches on"
    );
}

// ---------------------------------------------------------------------------
// Emission (MS2): the invariant violation must actually fire on a duplicate and
// must stay silent otherwise.
// ---------------------------------------------------------------------------

#[test]
fn a_self_duplicated_body_raises_the_invariant_violation() {
    let half = "알겠습니다. 바로 진행할게요.";
    let doubled = half.repeat(2);
    assert!(body_is_exact_self_duplicate(&doubled));

    let logs = captured_logs(|| {
        observe_body_mutation(
            BodyMutationSite::ReconcileFromInflightState,
            correlation(),
            "",
            &doubled,
        );
    });

    assert!(
        logs.contains(BODY_NOT_SELF_DUPLICATED_INVARIANT),
        "a doubled body must raise `{BODY_NOT_SELF_DUPLICATED_INVARIANT}`; got: {logs}"
    );
    assert!(
        logs.contains("self_duplicate=true"),
        "the tracing record must carry the verdict too; got: {logs}"
    );
}

#[test]
fn an_ordinary_body_raises_no_invariant_violation() {
    let logs = captured_logs(|| {
        observe_body_mutation(
            BodyMutationSite::ReconcileFromInflightState,
            correlation(),
            "",
            ORDINARY_EVEN_BODY,
        );
    });
    assert!(
        !logs.contains(BODY_NOT_SELF_DUPLICATED_INVARIANT),
        "a healthy body must not raise the invariant; got: {logs}"
    );
    assert!(logs.contains("self_duplicate=false"), "got: {logs}");
}

#[test]
fn the_violation_carries_the_correlation_keys_it_was_given() {
    // `record_invariant_check` only moves the `guard_fires` bucket when BOTH
    // keys are present, so a site that passes them must be visibly different
    // from one that cannot.
    let doubled = "알겠습니다. 바로 진행할게요.".repeat(2);
    let logs = captured_logs(|| {
        observe_body_mutation(
            BodyMutationSite::ReconcileFromInflightState,
            correlation(),
            "",
            &doubled,
        );
    });
    // `emit_invariant_log!` renders an absent key as `provider=""` /
    // `channel_id=0`, so these two assertions genuinely separate Some from None.
    assert!(
        logs.contains("provider=\"codex\""),
        "provider must reach the violation: {logs}"
    );
    assert!(
        logs.contains("channel_id=4259612"),
        "channel_id must reach the violation: {logs}"
    );

    // And the site that structurally cannot supply them renders the empty pair,
    // which is what makes `BodyMutationCorrelation::unavailable()` visible in the
    // readout rather than silently indistinguishable.
    let unfilled = captured_logs(|| {
        observe_body_mutation(
            BodyMutationSite::AppendStreamedTextChunk,
            BodyMutationCorrelation::unavailable(),
            "",
            &doubled,
        );
    });
    assert!(unfilled.contains("provider=\"\""), "got: {unfilled}");
    assert!(unfilled.contains("channel_id=0"), "got: {unfilled}");
}

// ---------------------------------------------------------------------------
// Per-site emission.
// ---------------------------------------------------------------------------

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
        adopt_full_response_from_inflight_row(
            &mut local,
            "COUNT-001\nCOUNT-002\n",
            BodyMutationSite::ReconcileFromInflightState,
            correlation(),
        );
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

/// #5938 P1-3: `stream_tick` stages the local body into the row and then reads
/// it straight back, so an equal adoption is the common case and recording it
/// would drown the stream in `prefix_len == after_len` no-ops.
#[test]
fn an_equal_adoption_is_not_a_mutation_and_is_not_recorded() {
    let logs = captured_logs(|| {
        let mut local = String::from("COUNT-001\nCOUNT-002\n");
        adopt_full_response_from_inflight_row(
            &mut local,
            "COUNT-001\nCOUNT-002\n",
            BodyMutationSite::ReconcileFromInflightState,
            correlation(),
        );
        // Identity is preserved: the body the caller sees is unchanged.
        assert_eq!(local, "COUNT-001\nCOUNT-002\n");
    });
    assert!(
        logs.is_empty(),
        "an adoption that changes nothing must emit nothing; got: {logs}"
    );
}

#[test]
fn a_changed_adoption_is_still_recorded_after_the_no_op_skip() {
    // The other side of the skip: suppressing no-ops must not suppress signal.
    for (local, durable) in [
        ("COUNT-001\n", "COUNT-001\nCOUNT-002\n"),
        ("COUNT-001\nCOUNT-002\n", "COUNT-001\n"),
        ("", "COUNT-001\n"),
        ("COUNT-001\n", ""),
    ] {
        let logs = captured_logs(|| {
            let mut body = String::from(local);
            adopt_full_response_from_inflight_row(
                &mut body,
                durable,
                BodyMutationSite::ReconcileFromInflightState,
                correlation(),
            );
            assert_eq!(body, durable);
        });
        assert!(
            logs.contains(
                "site=\"bridge_entry_persist::reconcile_runtime_locals_from_inflight_state\""
            ),
            "{local:?} -> {durable:?} changed the body and must be recorded; got: {logs}"
        );
    }
}

/// #5938 P1-2: the tool-boundary separator rewrites the body — including
/// SHRINKING it — and `stream_loop/tool_arms.rs` writes the result into the
/// durable row on the next statement. An unobserved shrink reads as loss.
#[test]
fn the_tool_boundary_site_emits_a_body_mutation_record() {
    let logs = captured_logs(|| {
        let mut body = String::from("first answer   ");
        append_tool_boundary_separator(&mut body, correlation());
        assert_eq!(body, "first answer\n\n");
    });

    assert!(
        logs.contains("site=\"chunk_compose::append_tool_boundary_separator\""),
        "tool-boundary site must publish a record, got: {logs}"
    );
    assert!(logs.contains("before_len=15"), "got: {logs}");
    assert!(logs.contains("after_len=14"), "got: {logs}");
    assert!(
        logs.contains("prefix_len=12"),
        "the record must show the retained prefix, not a pretend append; got: {logs}"
    );
}

#[test]
fn the_tool_boundary_site_reports_a_rewrite_to_itself_as_a_full_prefix() {
    // The reason the pre-image is cloned rather than reconstructed from
    // `trim_end().len()`: here the truncate+push is the identity, and the honest
    // record is prefix_len == after_len, not a fabricated two-byte delta.
    let logs = captured_logs(|| {
        let mut body = String::from("first answer\n\n");
        append_tool_boundary_separator(&mut body, correlation());
        assert_eq!(body, "first answer\n\n");
    });
    assert!(logs.contains("before_len=14"), "got: {logs}");
    assert!(logs.contains("after_len=14"), "got: {logs}");
    assert!(logs.contains("prefix_len=14"), "got: {logs}");
}

#[test]
fn the_tool_boundary_site_stays_silent_on_an_empty_body() {
    // The helper is a documented no-op there; an emitted record would invent a
    // mutation that never happened.
    let logs = captured_logs(|| {
        let mut body = String::new();
        append_tool_boundary_separator(&mut body, correlation());
        assert!(body.is_empty());
    });
    assert!(logs.is_empty(), "got: {logs}");
}

/// #5938 P1-2 (second half): `retry_state::clear_response_delivery_state` blanks
/// the bridge-local body AND the durable row body together. Unobserved, the
/// record stream shows `after_len=N` and then, with no record in between, a
/// later `before_len=0` — indistinguishable from the loss class this
/// instrumentation is hunting. Driven through the REAL function, so deleting
/// its observation fails here.
#[test]
fn the_empty_sink_rewind_site_emits_a_body_mutation_record() {
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::turn_bridge::retry_state::clear_response_delivery_state;
    use crate::services::provider::ProviderKind;

    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        5_938_002,
        None,
        1,
        2,
        0,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    row.full_response = "COUNT-001\nCOUNT-002\n".to_string();
    row.response_sent_offset = 20;
    let mut local = String::from("COUNT-001\nCOUNT-002\n");
    let mut offset = 20usize;

    let logs = captured_logs(|| {
        clear_response_delivery_state(&mut local, &mut offset, &mut row);
    });

    // The rewind itself is untouched by the observation.
    assert!(local.is_empty());
    assert_eq!(offset, 0);
    assert!(row.full_response.is_empty());
    assert_eq!(row.response_sent_offset, 0);

    assert!(
        logs.contains("site=\"retry_state::clear_response_delivery_state\""),
        "the rewind must publish a body-mutation record; got: {logs}"
    );
    assert!(logs.contains("before_len=20"), "got: {logs}");
    assert!(logs.contains("after_len=0"), "got: {logs}");
    assert!(logs.contains("prefix_len=0"), "got: {logs}");
    // The row holds both correlation keys, so the rewind is joinable.
    assert!(
        logs.contains("provider=\"codex\"") || !logs.contains("[invariant]"),
        "an emitted violation from this site must carry the row's provider: {logs}"
    );
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

// ---------------------------------------------------------------------------
// The fingerprint itself.
// ---------------------------------------------------------------------------

#[test]
fn self_duplication_predicate_catches_an_exactly_doubled_body() {
    let single = "COUNT-".to_string() + &"0123456789".repeat(6);
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

/// THE P0-3 REGRESSION. Both composers insert `"\n\n"` between two segments
/// when the first ends on a `semantic_boundaries::semantic_terminal_char`, so a
/// doubled natural-language turn is `X + "\n\n" + X`, not `X + X`. The first
/// revision of this predicate only recognised the second shape and was therefore
/// blind to every duplicate whose first copy ended in a sentence.
///
/// The separator is NOT hand-rolled: each case is composed by the production
/// `append_streamed_text_chunk`, so the fixture cannot drift away from what the
/// bridge (and, via the same predicate, the watcher) actually emits.
#[test]
fn self_duplication_separators_match_the_composed_boundary() {
    // Every member of `semantic_boundaries::semantic_terminal_char`.
    for terminal in ['.', '!', '?', '…', '。', '！', '？'] {
        let half = format!("모든 변경을 반영했습니다{terminal}");
        let mut composed = String::new();
        append_streamed_text_chunk(&mut composed, &half);
        append_streamed_text_chunk(&mut composed, &half);

        assert_eq!(
            composed,
            format!("{half}\n\n{half}"),
            "{terminal:?}: the production composer must insert the paragraph break \
             this predicate has to see through"
        );
        assert!(
            body_is_exact_self_duplicate(&composed),
            "{terminal:?}: a separator-joined double is still the #5938 shape: {composed:?}"
        );
    }
}

#[test]
fn self_duplication_predicate_sees_through_every_recognised_separator() {
    let half = "Review complete. Nothing further to change.";
    for separator in ["", "\n", "\n\n"] {
        let body = format!("{half}{separator}{half}");
        assert!(
            body_is_exact_self_duplicate(&body),
            "separator {separator:?} must not hide the duplicate"
        );
    }
    // A separator the composers never emit is not silently accepted: the body
    // below is a double joined by " ", and reading it as a duplicate would mean
    // the predicate had become "contains the same text twice", which is a much
    // broader and much noisier claim.
    assert!(!body_is_exact_self_duplicate(&format!("{half} {half}")));
}

/// This deployment's modal assistant turn is a short Korean acknowledgement.
/// Hangul is 3 bytes per syllable, so the whole class doubles to 32-56 bytes and
/// the previous 64-byte floor made every one of them invisible.
#[test]
fn short_korean_acknowledgements_are_the_modal_shape_and_must_be_caught() {
    for half in [
        "네, 확인했습니다.",
        "완료했습니다.",
        "수정 완료했습니다.",
        "확인했어요!",
        "알겠습니다. 바로 진행할게요.",
    ] {
        assert!(
            half.len() < 64,
            "{half:?} must be under the old floor or this test proves nothing"
        );
        for separator in ["", "\n\n"] {
            let body = format!("{half}{separator}{half}");
            assert!(
                body_is_exact_self_duplicate(&body),
                "{half:?} + {separator:?}: doubled length {} must be flagged",
                body.len()
            );
        }
    }
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
    for body in ["\n\n", "  ", "byebye", "abab", "ㅋㅋㅋㅋ", "\n", "hahaha"] {
        assert!(
            body.len() < SELF_DUPLICATION_MIN_LEN,
            "{body:?} must be under the floor or this case tests the wrong guard"
        );
        assert!(
            !body_is_exact_self_duplicate(body),
            "{body:?} is under the length floor and must not be flagged"
        );
    }
}

/// The guard that scales past the floor. A laugh run clears 16 bytes easily, so
/// the floor alone cannot exclude it; its half has a period of one character.
/// The other side matters just as much: a genuine double must NOT be rejected
/// just because the predicate got stricter.
#[test]
fn minimal_period_rejects_repeat_runs_but_admits_a_genuine_double() {
    for run in [
        "ㅋ".repeat(20),
        "하".repeat(30),
        "!".repeat(64),
        "ab".repeat(40),
    ] {
        assert!(
            run.len() >= SELF_DUPLICATION_MIN_LEN,
            "{run:?} must clear the floor so the period guard is what rejects it"
        );
        assert!(
            !body_is_exact_self_duplicate(&run),
            "a character run is legitimate model text, not #5938: {run:?}"
        );
    }

    // Aperiodic halves stay flagged at the same lengths.
    for half in [
        "프로젝트 설정을 갱신했습니다.",
        "Deployment finished cleanly.",
    ] {
        let doubled = half.repeat(2);
        assert!(!has_shorter_repeating_period(half.as_bytes()));
        assert!(
            body_is_exact_self_duplicate(&doubled),
            "{half:?} is aperiodic and its double must still be flagged"
        );
    }

    // Unit test of the helper itself, so a regression is attributable.
    assert!(has_shorter_repeating_period(b"abab"));
    assert!(has_shorter_repeating_period(b"abcabcabc"));
    assert!(has_shorter_repeating_period("ㅋㅋ".as_bytes()));
    assert!(!has_shorter_repeating_period(b"abcabca"));
    assert!(!has_shorter_repeating_period(b"12345678"));
    assert!(!has_shorter_repeating_period(b"a"));
}

#[test]
fn a_whitespace_only_half_is_never_a_duplicate() {
    // Aperiodic whitespace clears both the floor and the period guard, so this
    // exclusion is the only thing standing between a blank body and an ERROR.
    let half = " \n\t \n\t\t \n\r \t";
    assert!(!has_shorter_repeating_period(half.as_bytes()));
    let doubled = half.repeat(2);
    assert!(doubled.len() >= SELF_DUPLICATION_MIN_LEN);
    assert!(!body_is_exact_self_duplicate(&doubled));
    assert!(!body_is_exact_self_duplicate(&format!("{half}\n\n{half}")));
}

#[test]
fn the_floor_is_pinned_at_its_exact_boundary() {
    // One byte either side of `SELF_DUPLICATION_MIN_LEN`, with an aperiodic
    // non-blank half both times, so only the floor decides.
    let admitted = "aXbYcZdW";
    let body = admitted.repeat(2);
    assert_eq!(body.len(), SELF_DUPLICATION_MIN_LEN);
    assert!(body_is_exact_self_duplicate(&body));

    let rejected = "aXbYcZd";
    let shorter = rejected.repeat(2);
    assert_eq!(shorter.len(), SELF_DUPLICATION_MIN_LEN - 2);
    assert!(!body_is_exact_self_duplicate(&shorter));
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
        // Same, joined by the separator the composers insert.
        "한글 본문 반복 테스트 입니다.\n\n한글 본문 반복 테스트 입니다.".to_string(),
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
    let doubled = half.repeat(2);
    assert!(body_is_exact_self_duplicate(&doubled));
    assert!(body_is_exact_self_duplicate(&format!("{half}\n\n{half}")));
}

// ---------------------------------------------------------------------------
// Observation-only.
// ---------------------------------------------------------------------------

/// The instrumentation is observation only: it must not block, trim, rewrite or
/// otherwise touch the body at any site.
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

    // Tool-boundary site: trim-then-separator is unchanged by the observation,
    // including when the result is a flagged self-duplicate.
    let mut boundary = String::from("네, 확인했습니다.\n\n네, 확인했습니다.   ");
    append_tool_boundary_separator(&mut boundary, correlation());
    assert_eq!(boundary, "네, 확인했습니다.\n\n네, 확인했습니다.\n\n");

    // Assignment site: the durable body is adopted whole, self-duplicate or
    // not, and the source is untouched.
    let durable = "COUNT-".to_string() + &"0123456789".repeat(6);
    let durable = durable.repeat(2);
    let mut local = String::from("COUNT-0123456789");
    adopt_full_response_from_inflight_row(
        &mut local,
        &durable,
        BodyMutationSite::ReconcileFromInflightState,
        correlation(),
    );
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
    // The half must be APERIODIC — a `"z"` run would be rejected by
    // `has_shorter_repeating_period` and this test would pass for the wrong
    // reason — so the trailing `"c"` breaks every tiling of the `"ab"` body.
    let half = format!("{}c", "ab".repeat(256 * 1024));
    assert!(!has_shorter_repeating_period(half.as_bytes()));
    let body = half.repeat(2);
    assert!(body.len() > BODY_MUTATION_DIGEST_LIMIT);

    let record = body_mutation_record(BodyMutationSite::ReconcileFromInflightState, "", &body);
    assert_eq!(record.delta_sha8, DIGEST_OVER_LIMIT);
    assert_eq!(record.body_sha8, DIGEST_OVER_LIMIT);
    assert!(
        record.self_duplicate,
        "the digest threshold must never blind the #5938 fingerprint"
    );
    assert_eq!(record.after_len, body.len());
}

/// P2 (MS5): the threshold is a tuning number nothing else pins, so a silent
/// re-tune in EITHER direction — up to 2 MiB, down to a kilobyte — would leave
/// every other test green while changing what production records. This asserts
/// the last digesting length and the first suppressed length are adjacent at
/// exactly the shipped value.
#[test]
fn digest_limit_is_pinned_at_its_exact_boundary() {
    assert_eq!(BODY_MUTATION_DIGEST_LIMIT, 1024 * 1024);

    let at_limit = "q".repeat(BODY_MUTATION_DIGEST_LIMIT);
    let record = body_mutation_record(BodyMutationSite::AppendStreamedTextChunk, "", &at_limit);
    assert_ne!(record.delta_sha8, DIGEST_OVER_LIMIT);
    assert_eq!(record.delta_sha8.len(), 8);
    assert!(record.delta_sha8.chars().all(|ch| ch.is_ascii_hexdigit()));

    let past_limit = "q".repeat(BODY_MUTATION_DIGEST_LIMIT + 1);
    let record = body_mutation_record(BodyMutationSite::AppendStreamedTextChunk, "", &past_limit);
    assert_eq!(record.delta_sha8, DIGEST_OVER_LIMIT);
    assert_eq!(record.body_sha8, DIGEST_OVER_LIMIT);
}

#[test]
fn record_shape_is_identical_at_every_site() {
    // "같은 형태의 구조화 기록": the sites differ only in `site`.
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

    // Every variant carries a distinct, non-empty site label and a code location
    // that names its own file, so a record can be traced back without guessing.
    let sites = [
        BodyMutationSite::AppendStreamedTextChunk,
        BodyMutationSite::AppendToolBoundarySeparator,
        BodyMutationSite::ReconcileFromInflightState,
        BodyMutationSite::ClearResponseDeliveryState,
        BodyMutationSite::ReconcileToolArmLocalsFromInflightState,
        BodyMutationSite::SilenceRequeuedResponse,
        BodyMutationSite::RecoverBodyFromOutputFile,
    ];
    let labels: std::collections::BTreeSet<&str> = sites.iter().map(|site| site.as_str()).collect();
    assert_eq!(labels.len(), sites.len(), "site labels must be distinct");
    for site in sites {
        assert!(
            site.code_location()
                .starts_with("src/services/discord/turn_bridge/")
        );
        assert!(site.code_location().contains(".rs:"));
    }
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

// ---------------------------------------------------------------------------
// Predicate cost that is accepted rather than hidden (#5938 r2 P2-1).
// ---------------------------------------------------------------------------

/// The self-duplication predicate asks whether the WHOLE body is `X + sep + X`,
/// so a healthy turn that happens to be two identical halves is flagged. These
/// four shapes were measured against the shipped predicate and all four return
/// true. They are pinned — not filtered — because every filter that excludes
/// them also excludes the short Korean acknowledgement the floor was lowered to
/// 16 to catch, and the instrumentation is observation only: a false positive
/// costs one ERROR line, a false negative costs the investigation.
///
/// If a later change makes one of these return false, that is a REAL narrowing
/// of the predicate and this test is where the trade-off gets re-argued.
#[test]
fn known_false_positive_shapes_are_pinned_not_filtered() {
    let repeated_code_line = ["    let x = compute_value(input);"; 2].join("\n");
    let repeated_bullet = ["- 로그를 확인한다"; 2].join("\n");
    let repeated_table_row = ["| id | name | status |"; 2].join("\n");
    let repeated_emphasis = ["다시 한번 말합니다. 절대 배포하지 마세요."; 2].join("\n\n");

    for (label, body, expected_len) in [
        ("repeated code line", &repeated_code_line, 67usize),
        ("repeated bullet", &repeated_bullet, 49),
        ("repeated table row", &repeated_table_row, 45),
        ("repeated emphasis paragraph", &repeated_emphasis, 118),
    ] {
        assert_eq!(
            body.len(),
            expected_len,
            "{label}: fixture size changed, so the measured cost changed too"
        );
        assert!(
            body_is_exact_self_duplicate(body),
            "{label}: known false positive, documented on `body_is_exact_self_duplicate`"
        );
    }
}

/// The other half of the same trade-off, restated where the false positives are
/// pinned: the noise classes the module names ARE excluded, so the predicate is
/// not simply "any body with an even split".
#[test]
fn the_named_noise_classes_stay_excluded_beside_the_false_positives() {
    assert!(!body_is_exact_self_duplicate(&"ㅋ".repeat(20)));
    assert!(!body_is_exact_self_duplicate("byebye"));
    assert!(!body_is_exact_self_duplicate("\n\n"));
    assert!(!body_is_exact_self_duplicate("        "));
}

// ---------------------------------------------------------------------------
// Stored-event correlation (#5938 r2 P2-2).
// ---------------------------------------------------------------------------

/// The tracing LINE inherits dispatch/session/turn from the enclosing span; the
/// STORED `invariant_violation` event inherits nothing. A site holding a row
/// must therefore hand all of them over explicitly, and `from_inflight_row` is
/// what does it.
#[test]
fn from_inflight_row_carries_every_key_the_row_holds() {
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        5_938_021,
        None,
        343_742_347_365_974_026,
        77_012,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    row.dispatch_id = Some("dispatch-5938-keys".to_string());
    row.session_key = Some("adk-session-keys".to_string());

    let correlation = BodyMutationCorrelation::from_inflight_row(&row);
    assert_eq!(correlation.provider, Some("codex"));
    assert_eq!(correlation.channel_id, Some(5_938_021));
    assert_eq!(correlation.dispatch_id, Some("dispatch-5938-keys"));
    assert_eq!(correlation.session_key, Some("adk-session-keys"));
    assert_eq!(correlation.user_msg_id, Some(77_012));

    let doubled = "알겠습니다. 바로 진행할게요.".repeat(2);
    let logs = captured_logs(|| {
        observe_body_mutation(
            BodyMutationSite::ReconcileFromInflightState,
            BodyMutationCorrelation::from_inflight_row(&row),
            "",
            &doubled,
        );
    });
    assert!(
        logs.contains("dispatch_id=\"dispatch-5938-keys\""),
        "got: {logs}"
    );
    assert!(
        logs.contains("session_key=\"adk-session-keys\""),
        "got: {logs}"
    );
    // `turn_id_for_state`'s spelling, so the stored row joins the rest of the
    // observability surface.
    assert!(
        logs.contains("turn_id=\"discord:5938021:77012\""),
        "got: {logs}"
    );
}

/// A row with no anchored user message produces NO turn_id rather than a
/// `discord:<channel>:0` that would join against nothing — the same guard
/// `inflight::turn_id_for_state` applies.
#[test]
fn an_unanchored_row_emits_no_turn_id() {
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::ProviderKind;

    let row = InflightTurnState::new(
        ProviderKind::Codex,
        5_938_022,
        None,
        343_742_347_365_974_026,
        0,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    let doubled = "알겠습니다. 바로 진행할게요.".repeat(2);
    let logs = captured_logs(|| {
        observe_body_mutation(
            BodyMutationSite::ReconcileFromInflightState,
            BodyMutationCorrelation::from_inflight_row(&row),
            "",
            &doubled,
        );
    });
    assert!(logs.contains("turn_id=\"\""), "got: {logs}");
}
