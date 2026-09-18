//! #5948 / contract I18: a watcher rewind resends source bytes the sink parser
//! may already hold, and the relay mints a FRESH `StreamFrame::sequence` on
//! every send — so a replay always arrives with a LARGER sequence and no
//! receiver-side sequence test can ever see it. Identity comes from the
//! absolute source byte range instead.

use super::super::tests::{matched, matched_codex};
use super::SessionRelayParser;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::cluster::stream_relay::StreamFrame;

fn assistant(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": text}]}
        })
    )
}

fn result(text: &str) -> String {
    format!(
        "{}\n",
        serde_json::json!({"type": "result", "result": text})
    )
}

/// A streaming frame that names the absolute range its payload was read from,
/// exactly as `turn_stream_collector` now does.
fn spanned(binding: &MatchedChannel, payload: &str, sequence: u64, start: u64) -> StreamFrame {
    StreamFrame {
        session_name: binding.expected_session_name.clone(),
        binding: binding.clone(),
        payload: payload.to_string(),
        sequence,
        terminal_consumed_end: None,
        turn_user_msg_id: 0,
        turn_started_at: String::new(),
        turn_start_offset: None,
        relay_range: None,
        relay_generation_mtime_ns: None,
        relay_source_stamp: None,
        source_span: Some((start, start.saturating_add(payload.len() as u64))),
    }
}

/// A producer that cannot name a range — every pre-#5948 sender.
fn unspanned(binding: &MatchedChannel, payload: &str, sequence: u64) -> StreamFrame {
    StreamFrame {
        source_span: None,
        ..spanned(binding, payload, sequence, 0)
    }
}

fn terminal_spanned(
    binding: &MatchedChannel,
    payload: &str,
    sequence: u64,
    start: u64,
) -> StreamFrame {
    let mut frame = spanned(binding, payload, sequence, start);
    frame.turn_start_offset = Some(start);
    frame.terminal_consumed_end = Some(start.saturating_add(payload.len() as u64));
    frame.turn_user_msg_id = 5948;
    frame.turn_started_at = "2026-09-17T00:00:00Z".to_string();
    frame
}

/// DoD 1: the same source bytes arriving twice inside ONE open turn must land in
/// the turn once. This is the rewind damage #5948 describes — the watcher
/// protects only its own accumulator (`terminal_readiness.rs`), never the sink
/// parser, so the sink used to stack the range on top of itself and render one
/// delivery with doubled prose.
#[test]
fn rewind_resend_of_an_already_folded_range_does_not_double_the_open_turn() {
    let binding = matched("5948");
    let mut parser = SessionRelayParser::default();
    let head = assistant("alpha");
    let replay = format!("{head}{}{}", assistant("beta"), result("done"));

    assert!(
        parser
            .ingest_frame(&spanned(&binding, &head, 1, 1_000))
            .is_empty(),
        "an assistant line alone does not terminate the turn"
    );
    assert_eq!(
        parser.turn_source_end(),
        Some(1_000 + head.len() as u64),
        "the open turn remembers how far into the source it has read"
    );

    // The rewind re-reads from the turn's data start offset, so the SAME bytes
    // come back — carrying a strictly LARGER sequence, which is precisely why
    // `frame.sequence <= last_sequence` can never fire here.
    let resend = spanned(&binding, &replay, 9, 1_000);
    assert!(
        resend.sequence > 1,
        "precondition: the relay stamps a resend with a fresher sequence, so \
         sequence comparison is structurally blind to it"
    );
    let deliveries = parser.ingest_frame(&resend);

    assert_eq!(deliveries.len(), 1, "the completed turn is delivered once");
    assert_eq!(
        deliveries[0].response_text.matches("alpha").count(),
        1,
        "the replayed prefix must appear once, not twice: {:?}",
        deliveries[0].response_text
    );
    assert_eq!(
        deliveries[0].response_text.matches("beta").count(),
        1,
        "the genuinely new tail of the resend must survive: {:?}",
        deliveries[0].response_text
    );
}

/// DoD 2 (and the issue's stated risk): a response that deliberately prints the
/// same sentence twice is byte-identical to a one-line replay. Only the source
/// range separates them, and swallowing the second copy would be a fresh #5941
/// silent loss.
#[test]
fn intentionally_repeated_prose_in_a_later_source_range_is_never_swallowed() {
    let binding = matched("59482");
    let mut parser = SessionRelayParser::default();
    let line = assistant("the same sentence");

    assert!(
        parser
            .ingest_frame(&spanned(&binding, &line, 1, 2_000))
            .is_empty()
    );
    let second = format!("{line}{}", result("done"));
    let deliveries = parser.ingest_frame(&spanned(&binding, &second, 2, 2_000 + line.len() as u64));

    assert_eq!(deliveries.len(), 1);
    assert_eq!(
        deliveries[0]
            .response_text
            .matches("the same sentence")
            .count(),
        2,
        "identical prose at a LATER source range is real output, not a replay: {:?}",
        deliveries[0].response_text
    );
}

/// DoD 3, rewind path A — `loop_poll_prologue.rs` redrive resume. The redrive
/// re-reads a turn it has NOT delivered (#5943 / I16), so the resend lands in a
/// still-open turn and only its fresh tail may be folded. Pins the parser state
/// the resume leaves behind.
#[test]
fn redrive_resume_replays_an_open_turn_and_folds_only_the_fresh_tail() {
    let binding = matched("59483");
    let mut parser = SessionRelayParser::default();
    let first = assistant("one");
    let second = assistant("two");
    let tail = format!("{}{}", assistant("three"), result("done"));
    let base = 4_000u64;

    assert!(
        parser
            .ingest_frame(&spanned(&binding, &first, 1, base))
            .is_empty()
    );
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &second, 2, base + first.len() as u64))
            .is_empty()
    );
    let folded_before = parser.turn_source_end();
    assert_eq!(
        folded_before,
        Some(base + (first.len() + second.len()) as u64)
    );

    // The resume rewinds the watcher to the turn's start; the re-read chunk is
    // forwarded unconditionally (`turn_stream_collector`).
    let replay = format!("{first}{second}{tail}");
    let deliveries = parser.ingest_frame(&spanned(&binding, &replay, 3, base));

    assert_eq!(deliveries.len(), 1);
    for segment in ["one", "two", "three"] {
        assert_eq!(
            deliveries[0].response_text.matches(segment).count(),
            1,
            "`{segment}` must appear exactly once after the resume replay: {:?}",
            deliveries[0].response_text
        );
    }
    assert_eq!(
        parser.turn_source_end(),
        None,
        "handing the turn off clears the watermark — it is turn-scoped, never a \
         process-lifetime ledger on a parser that outlives every turn"
    );
    assert_eq!(parser.buffered_len(), 0, "the turn left nothing buffered");
}

/// DoD 3, rewind path B — `tmux_watcher.rs:1318` terminal-delivery rewind. That
/// rewind IS the retry ("must retry the SAME range next loop"): the turn was
/// already handed off and its POST failed. Suppressing this resend would convert
/// a failed POST into permanent silent loss, so the parser must deliver again.
#[test]
fn terminal_delivery_rewind_after_handoff_still_delivers_the_retry() {
    let binding = matched("59484");
    let mut parser = SessionRelayParser::default();
    let turn = format!("{}{}", assistant("the answer"), result("done"));
    let base = 6_000u64;

    let first = parser.ingest_frame(&terminal_spanned(&binding, &turn, 1, base));
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].response_text, "the answer");
    assert_eq!(
        parser.turn_source_end(),
        None,
        "the handoff clears the watermark, so the retry is not mistaken for a \
         replay of an open turn"
    );

    // The POST failed; the watcher rewound to `turn_data_start_offset` and
    // re-sent the identical range with a fresh sequence.
    let retry = parser.ingest_frame(&terminal_spanned(&binding, &turn, 2, base));
    assert_eq!(
        retry.len(),
        1,
        "the retry must still produce a delivery — the rewind exists to drive it"
    );
    assert_eq!(retry[0].response_text, "the answer");
}

/// The issue's Codex blind spot: without this, "Codex 쪽 재전송은 0% 커버된다".
/// A native Codex turn accumulates through the ordinary `ingest_frame` path like
/// any other provider's — the frames carry spans and a rewind replays them — and
/// only the FINAL body is recovered from the rollout file and injected
/// synthetically by `ingest_verified_native_terminal`. Both halves are pinned
/// here: the replay must not double the turn, and the synthetic injection must
/// leave no watermark that could strand the next turn or swallow the retry.
#[test]
fn native_codex_terminal_path_dedupes_the_open_turn_and_keeps_the_retry() {
    let binding = matched_codex("59485");
    let mut parser = SessionRelayParser::default();
    let head = assistant("codex partial");
    let base = 7_000u64;

    assert!(
        parser
            .ingest_frame(&spanned(&binding, &head, 1, base))
            .is_empty()
    );

    // A rewind on a Codex binding re-sends the identical bytes plus the turn's
    // remainder, exactly as it does for Claude.
    let replay = format!(
        "{head}{}{}",
        assistant("codex tail"),
        result("unused when text streamed")
    );
    let deliveries = parser.ingest_frame(&spanned(&binding, &replay, 2, base));
    assert_eq!(deliveries.len(), 1);
    for segment in ["codex partial", "codex tail"] {
        assert_eq!(
            deliveries[0].response_text.matches(segment).count(),
            1,
            "`{segment}` must survive the Codex replay exactly once: {:?}",
            deliveries[0].response_text
        );
    }

    // The native terminal body is recovered from the rollout file, not from the
    // payload, and is injected synthetically over a cleared buffer.
    let terminal = terminal_spanned(&binding, &head, 3, base);
    let delivered = parser.ingest_verified_native_terminal(&terminal, "codex answer");
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].response_text, "codex answer");
    assert_eq!(
        parser.turn_source_end(),
        None,
        "the synthetic body is not described by the frame's source range, so it \
         must leave no watermark behind to strand the next turn"
    );

    let retry = parser.ingest_verified_native_terminal(&terminal, "codex answer");
    assert_eq!(
        retry.len(),
        1,
        "the Codex terminal retry survives for the same reason the Claude one does"
    );
    assert_eq!(retry[0].response_text, "codex answer");
}

/// A transcript rotation restarts the offsets, so a watermark carried across it
/// would read the new file's byte 0 as already-consumed and delete a real turn.
#[test]
fn a_generation_change_clears_the_resend_watermark() {
    let binding = matched("59486");
    let mut parser = SessionRelayParser::default();
    let head = assistant("before rotation");
    let mut first = spanned(&binding, &head, 1, 8_000);
    first.relay_generation_mtime_ns = Some(1_111);
    assert!(parser.ingest_frame(&first).is_empty());
    assert_eq!(parser.turn_source_end(), Some(8_000 + head.len() as u64));

    // The replacement transcript's offsets restart, so the SAME numbers now name
    // different bytes.
    let rotated = format!("{}{}", assistant("after rotation"), result("done"));
    let mut second = spanned(&binding, &rotated, 2, 8_000);
    second.relay_generation_mtime_ns = Some(2_222);
    let deliveries = parser.ingest_frame(&second);

    assert_eq!(deliveries.len(), 1);
    assert_eq!(
        deliveries[0].response_text, "after rotation",
        "the rotated turn is complete — the stale watermark must not trim it"
    );
}

/// Fail open: a producer that cannot name a range keeps today's behaviour. The
/// sink refuses to guess, because guessing is what turns a dedupe into a loss.
#[test]
fn frames_without_a_source_span_are_folded_whole() {
    let binding = matched("59487");
    let mut parser = SessionRelayParser::default();
    let line = assistant("unnamed range");

    assert!(
        parser
            .ingest_frame(&unspanned(&binding, &line, 1))
            .is_empty()
    );
    assert_eq!(
        parser.turn_source_end(),
        None,
        "an unnamed frame contributes no watermark"
    );
    let deliveries = parser.ingest_frame(&unspanned(
        &binding,
        &format!("{line}{}", result("done")),
        2,
    ));

    assert_eq!(deliveries.len(), 1);
    assert_eq!(
        deliveries[0].response_text.matches("unnamed range").count(),
        2,
        "without a range the parser accumulates exactly as it always did: {:?}",
        deliveries[0].response_text
    );
}

/// Two more fail-open boundaries on the same code path: a GAP between spans (the
/// parser never saw the bytes in between, so nothing may be trimmed) and a span
/// whose width disagrees with its payload (a coordinate the sink cannot trust to
/// slice a body apart).
#[test]
fn a_gap_or_a_disagreeing_span_folds_the_payload_whole() {
    let binding = matched("59488");
    let mut parser = SessionRelayParser::default();
    let head = assistant("head");
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &head, 1, 9_000))
            .is_empty()
    );

    // A gap: the next frame starts well past the watermark.
    let gapped = assistant("after a gap");
    assert!(
        parser
            .ingest_frame(&spanned(
                &binding,
                &gapped,
                2,
                9_000 + head.len() as u64 + 512
            ))
            .is_empty()
    );

    // A span narrower than its payload: the width disagrees, so the body folds
    // whole rather than being sliced on a coordinate the sink cannot trust. Note
    // this span sits entirely BEHIND the watermark — honouring it would erase the
    // payload outright, which is the loss mode this guard exists to stop.
    let mut liar = spanned(
        &binding,
        &format!("{}{}", assistant("tail"), result("done")),
        3,
        0,
    );
    liar.source_span = Some((0, 1));
    let deliveries = parser.ingest_frame(&liar);

    assert_eq!(deliveries.len(), 1);
    for segment in ["head", "after a gap", "tail"] {
        assert_eq!(
            deliveries[0].response_text.matches(segment).count(),
            1,
            "`{segment}` must survive a fail-open fold: {:?}",
            deliveries[0].response_text
        );
    }
}

/// DoD 4: the suppression is observable. #5941 is the standing lesson that a
/// counter with a producer and no consumer hides its own regression, so this
/// pins the producer end — the parser really does bump
/// `relay_resend_suppressed` — while `relay_signal_alert` pins the consumer.
///
/// Channel 594_890 is used by this test alone so the global delta is exact
/// without serializing the suite.
#[test]
fn a_suppressed_resend_increments_the_relay_resend_suppressed_counter() {
    let binding = matched("594890");
    let counters = crate::services::observability::metrics::global();
    let read = || {
        counters
            .snapshot()
            .into_iter()
            .find(|row| row.channel_id == 594_890)
            .map_or(0, |row| row.relay_resend_suppressed)
    };
    let before = read();

    let mut parser = SessionRelayParser::default();
    let line = assistant("observed");
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &line, 1, 10_000))
            .is_empty()
    );
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &line, 2, 10_000))
            .is_empty()
    );

    assert_eq!(
        read() - before,
        1,
        "one suppressed resend must raise exactly one counter increment"
    );
}

/// The watermark is MONOTONIC inside a turn. A rewind can resend an EARLY range
/// after a later one already folded (the watcher rewinds to the turn's data
/// start, not to wherever the sink got to), and if that early frame pulled the
/// watermark back down, the next resend of the middle would be folded a second
/// time — the doubling this whole mechanism exists to stop, reintroduced by the
/// resend it was handed.
#[test]
fn an_early_resend_never_pulls_the_watermark_back_down() {
    let binding = matched("59489");
    let mut parser = SessionRelayParser::default();
    let first = assistant("first range");
    let second = assistant("second range");
    let base = 11_000u64;
    let second_start = base + first.len() as u64;

    assert!(
        parser
            .ingest_frame(&spanned(&binding, &first, 1, base))
            .is_empty()
    );
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &second, 2, second_start))
            .is_empty()
    );
    let high_water = parser.turn_source_end();
    assert_eq!(high_water, Some(second_start + second.len() as u64));

    // The rewind replays the FIRST range on its own — entirely behind the mark.
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &first, 3, base))
            .is_empty()
    );
    assert_eq!(
        parser.turn_source_end(),
        high_water,
        "a fully-suppressed early range must not rewind the watermark"
    );

    // ...and then the second range again, which must still be recognised.
    let closing = format!("{second}{}", result("done"));
    let deliveries = parser.ingest_frame(&spanned(&binding, &closing, 4, second_start));

    assert_eq!(deliveries.len(), 1);
    for segment in ["first range", "second range"] {
        assert_eq!(
            deliveries[0].response_text.matches(segment).count(),
            1,
            "`{segment}` must appear once after both ranges were resent: {:?}",
            deliveries[0].response_text
        );
    }
}

/// The counter must fire for a PARTIAL resend too — a replay carrying the
/// already-folded head plus a fresh tail is the common rewind shape, and a
/// counter that only saw whole-frame suppression would under-report it.
#[test]
fn a_partially_suppressed_resend_increments_the_counter_too() {
    let binding = matched("594891");
    let counters = crate::services::observability::metrics::global();
    let read = || {
        counters
            .snapshot()
            .into_iter()
            .find(|row| row.channel_id == 594_891)
            .map_or(0, |row| row.relay_resend_suppressed)
    };
    let before = read();

    let mut parser = SessionRelayParser::default();
    let head = assistant("head");
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &head, 1, 20_000))
            .is_empty()
    );
    let replay = format!("{head}{}", assistant("tail"));
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &replay, 2, 20_000))
            .is_empty()
    );

    assert_eq!(
        read() - before,
        1,
        "a partial resend is still a suppression"
    );
}

/// `split_at_checked` declines when the watermark lands INSIDE a multi-byte
/// scalar — a producer coordinate widened by a lossy decode can put it there.
/// The payload must then fold whole rather than vanish, and a fail-open fold is
/// not a suppression.
#[test]
fn an_overlap_inside_a_multibyte_scalar_folds_the_payload_whole() {
    let binding = matched("594892");
    let counters = crate::services::observability::metrics::global();
    let read = || {
        counters
            .snapshot()
            .into_iter()
            .find(|row| row.channel_id == 594_892)
            .map_or(0, |row| row.relay_resend_suppressed)
    };
    let before = read();

    let mut parser = SessionRelayParser::default();
    let base = 30_000u64;
    let line = format!("{}{}", assistant("가나다"), result("끝"));
    let inside = line.find('가').expect("korean text") + 1;
    assert!(!line.is_char_boundary(inside));
    // A complete filler line whose range ends one byte into `가`, so the JSON
    // that follows still parses on its own line.
    let filler = format!("{}\n", "x".repeat(inside - 1));
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &filler, 1, base))
            .is_empty()
    );

    let deliveries = parser.ingest_frame(&spanned(&binding, &line, 2, base));
    assert_eq!(
        deliveries.len(),
        1,
        "a fail-open fold must not lose the turn"
    );
    assert_eq!(deliveries[0].response_text.matches("가나다").count(), 1);
    assert_eq!(read() - before, 0);
}

/// The synthetic native terminal body is NOT the bytes its frame's span names.
/// The frame's own turn ends inside the same call, so the one path where the
/// fold's side effect outlives it is a frame the sink skips (unparseable
/// channel id): there a width-matching span left on the synthetic frame would
/// seed the watermark with a range never folded, and the next genuine frame in
/// that range would be cut.
#[test]
fn a_synthetic_native_terminal_never_seeds_the_watermark() {
    let binding = matched_codex("codex-native-5979");
    let mut parser = SessionRelayParser::default();
    let base = 40_000u64;
    let response = "codex answer";
    let synthetic_len = result(response).len();
    let terminal = terminal_spanned(&binding, &"y".repeat(synthetic_len), 1, base);
    assert_eq!(
        terminal.source_span,
        Some((base, base + synthetic_len as u64))
    );

    assert!(
        parser
            .ingest_verified_native_terminal(&terminal, response)
            .is_empty()
    );
    assert_eq!(
        parser.turn_source_end(),
        None,
        "a synthetic body must leave no watermark even when its width matches"
    );

    let genuine = assistant("real");
    let before = parser.buffered_len();
    assert!(
        parser
            .ingest_frame(&spanned(&binding, &genuine, 2, base))
            .is_empty()
    );
    assert_eq!(parser.buffered_len() - before, genuine.len());
}
