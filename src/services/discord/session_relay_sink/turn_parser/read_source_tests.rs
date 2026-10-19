//! An open turn whose bytes move to another read file: the new file folds under
//! its own watermark and framing, and a superseded watcher's frames stay out.

use std::sync::Arc;

use super::super::tests::{matched, matched_codex};
use super::SessionRelayParser;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::cluster::stream_relay::{
    ReadSource, RelaySink, RelaySinkOutcome, SourceFileIdentity, StreamFrame,
};
use crate::services::observability::metrics;

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

/// A watcher frame read from file `ino` by watcher `producer`, spanning
/// `[start, start + payload.len())` of that file.
fn read(
    binding: &MatchedChannel,
    payload: &str,
    sequence: u64,
    start: u64,
    (ino, producer): (u64, u64),
) -> StreamFrame {
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
        source_span: Some((start, start + payload.len() as u64)),
        read_source: Some(ReadSource {
            file: SourceFileIdentity::Unix { dev: 7120, ino },
            producer,
        }),
    }
}

fn counters(channel_id: u64) -> (u64, u64) {
    metrics::global()
        .snapshot()
        .into_iter()
        .find(|row| row.channel_id == channel_id)
        .map_or((0, 0), |row| {
            (
                row.relay_read_source_switched,
                row.relay_stale_source_frame_dropped,
            )
        })
}

fn single_body(deliveries: Vec<super::SessionRelayDelivery>) -> String {
    assert_eq!(deliveries.len(), 1, "the open turn is delivered once");
    deliveries.into_iter().next().unwrap().response_text
}

/// A newer watcher's file B starts below A's watermark; its bytes must reach
/// the open turn, for a new watcher and for the same watcher after a rename.
#[test]
fn newer_watcher_on_another_file_delivers_its_bytes_in_the_open_turn() {
    for (case, b_producer) in [(712_001_u64, 6), (712_002, 5)] {
        let binding = matched(&case.to_string());
        let before = counters(case);
        let mut parser = SessionRelayParser::default();
        let x = assistant("x-from-file-a");
        assert!(
            parser
                .ingest_frame(&read(&binding, &x, 1, 10_000, (1, 5)))
                .is_empty()
        );

        let tail = format!("{}{}", assistant("tail-from-file-b"), result(""));
        let body = single_body(parser.ingest_frame(&read(&binding, &tail, 2, 0, (2, b_producer))));

        assert_eq!(body.matches("x-from-file-a").count(), 1, "{body:?}");
        assert_eq!(body.matches("tail-from-file-b").count(), 1, "{body:?}");
        let after = counters(case);
        assert_eq!((after.0 - before.0, after.1 - before.1), (1, 0));
    }
}

/// APPEND keeps A's output even when B repeats it; the repeat is recorded, not lost.
#[test]
fn a_file_that_repeats_the_turn_appends_instead_of_replacing() {
    let binding = matched("712003");
    let mut parser = SessionRelayParser::default();
    let x = assistant("x-from-file-a");
    assert!(
        parser
            .ingest_frame(&read(&binding, &x, 1, 4_000, (1, 5)))
            .is_empty()
    );
    let b = format!("{x}{}{}", assistant("tail-from-file-b"), result(""));
    let body = single_body(parser.ingest_frame(&read(&binding, &b, 2, 0, (2, 6))));
    assert_eq!(body.matches("x-from-file-a").count(), 2, "{body:?}");
    assert_eq!(body.matches("tail-from-file-b").count(), 1, "{body:?}");
}

/// A's incomplete trailing record must not swallow B's first record.
#[test]
fn a_partial_record_on_one_file_never_joins_the_next_files_first_record() {
    let binding = matched("712004");
    let mut parser = SessionRelayParser::default();
    let x = assistant("x-from-file-a");
    let unfinished = assistant("unfinished-a-line");
    let a = format!("{x}{}", &unfinished[..20]);
    assert!(
        parser
            .ingest_frame(&read(&binding, &a, 1, 1_000, (1, 5)))
            .is_empty()
    );

    // B's span sits past A's watermark, so only framing could hide B here.
    let b = format!("{}{}", assistant("y-from-file-b"), result("y-from-file-b"));
    let body = single_body(parser.ingest_frame(&read(&binding, &b, 2, 5_000, (2, 6))));

    assert_eq!(body.matches("x-from-file-a").count(), 1, "{body:?}");
    assert_eq!(body.matches("y-from-file-b").count(), 1, "{body:?}");
}

/// When A comes back through a new watcher reading from the record boundary,
/// its parked partial record and watermark resume exactly once.
#[test]
fn a_partial_record_resumes_when_its_file_returns() {
    let binding = matched("712005");
    let mut parser = SessionRelayParser::default();
    let x = assistant("x-from-file-a");
    let resumed = assistant("resumed-a-line");
    let p = 1_000 + x.len() as u64;
    let a = format!("{x}{}", &resumed[..20]);
    assert!(
        parser
            .ingest_frame(&read(&binding, &a, 1, 1_000, (1, 5)))
            .is_empty()
    );
    let y = assistant("y-from-file-b");
    assert!(
        parser
            .ingest_frame(&read(&binding, &y, 2, 5_000, (2, 6)))
            .is_empty()
    );

    let again = format!("{resumed}{}", result(""));
    let body = single_body(parser.ingest_frame(&read(&binding, &again, 3, p, (1, 7))));

    for text in ["x-from-file-a", "y-from-file-b", "resumed-a-line"] {
        assert_eq!(body.matches(text).count(), 1, "{text}: {body:?}");
    }
}

/// A successor on the same file keeps the resend watermark: only `[F, E)` folds.
#[test]
fn successor_on_the_same_file_keeps_the_resend_watermark() {
    let binding = matched("712006");
    let mut parser = SessionRelayParser::default();
    let head = assistant("head-once");
    assert!(
        parser
            .ingest_frame(&read(&binding, &head, 1, 2_000, (1, 5)))
            .is_empty()
    );
    let replay = format!("{head}{}{}", assistant("fresh-tail"), result(""));
    let body = single_body(parser.ingest_frame(&read(&binding, &replay, 2, 2_000, (1, 6))));
    assert_eq!(body.matches("head-once").count(), 1, "{body:?}");
    assert_eq!(body.matches("fresh-tail").count(), 1, "{body:?}");
}

/// A superseded watcher's late frame never reaches the turn B owns, before or
/// after B's turn is delivered.
#[test]
fn superseded_watcher_frames_never_reach_the_open_turn() {
    let case = 712_007_u64;
    let binding = matched(&case.to_string());
    let before = counters(case);
    let mut parser = SessionRelayParser::default();
    let x = assistant("x-from-file-a");
    assert!(
        parser
            .ingest_frame(&read(&binding, &x, 1, 1_000, (1, 5)))
            .is_empty()
    );
    let y = assistant("y-from-file-b");
    assert!(
        parser
            .ingest_frame(&read(&binding, &y, 2, 0, (2, 6)))
            .is_empty()
    );
    let late = assistant("late-from-file-a");
    let late_start = 1_000 + x.len() as u64;
    assert!(
        parser
            .ingest_frame(&read(&binding, &late, 3, late_start, (1, 5)))
            .is_empty()
    );

    let body = single_body(parser.ingest_frame(&read(&binding, &result(""), 4, 100, (2, 6))));
    assert!(!body.contains("late-from-file-a"), "{body:?}");
    assert_eq!(body.matches("y-from-file-b").count(), 1, "{body:?}");

    let late_turn = format!("{}{}", assistant("late-turn-from-a"), result(""));
    let next_start = late_start + late.len() as u64;
    let late_turn = read(&binding, &late_turn, 5, next_start, (1, 5));
    assert!(
        parser.ingest_frame(&late_turn).is_empty(),
        "an old watcher cannot open a new turn after B's delivery"
    );
    let after = counters(case);
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 2));
}

/// The sink refuses a superseded watcher's native terminal before any re-read:
/// without a source witness a re-read would fail instead of resolving it.
#[tokio::test]
async fn a_superseded_native_terminal_is_not_delivered_and_never_reread() {
    let binding = matched_codex("712008");
    let sink = super::super::SessionBoundDiscordRelaySink::new(Arc::new(
        crate::services::discord::health::HealthRegistry::new(),
    ));
    for frame in [
        read(&binding, &assistant("x-from-file-a"), 1, 1_000, (1, 5)),
        read(&binding, &assistant("y-from-file-b"), 2, 0, (2, 6)),
    ] {
        assert_eq!(
            sink.deliver(&frame).await.unwrap(),
            RelaySinkOutcome::FrameAccepted
        );
    }
    let native = format!(
        "{}\n",
        serde_json::json!({"type": "event_msg", "payload": {
            "type": "task_complete", "last_agent_message": "late-from-file-a"}})
    );
    let mut terminal = read(&binding, &native, 3, 1_100, (1, 5));
    terminal.turn_start_offset = Some(1_000);
    terminal.terminal_consumed_end = Some(1_100 + native.len() as u64);

    assert_eq!(
        sink.deliver(&terminal).await.unwrap(),
        RelaySinkOutcome::TerminalNotDelivered
    );
}

/// Returning to an earlier file restores its watermark; the next turn on a
/// third file starts clean.
#[test]
fn returning_to_an_earlier_file_restores_its_watermark() {
    let binding = matched("712009");
    let mut parser = SessionRelayParser::default();
    let x = assistant("x-from-file-a");
    assert!(
        parser
            .ingest_frame(&read(&binding, &x, 1, 0, (1, 5)))
            .is_empty()
    );
    let y = assistant("y-from-file-b");
    assert!(
        parser
            .ingest_frame(&read(&binding, &y, 2, 0, (2, 6)))
            .is_empty()
    );
    let back = format!("{x}{}{}", assistant("w-from-file-a"), result(""));
    let body = single_body(parser.ingest_frame(&read(&binding, &back, 3, 0, (1, 7))));
    for text in ["x-from-file-a", "y-from-file-b", "w-from-file-a"] {
        assert_eq!(body.matches(text).count(), 1, "{text}: {body:?}");
    }

    let next = format!("{}{}", assistant("q-from-file-c"), result(""));
    let body = single_body(parser.ingest_frame(&read(&binding, &next, 4, 0, (3, 8))));
    assert_eq!(body, "q-from-file-c");
}

/// A verified native terminal from a newer file stays a canonical replacement:
/// the earlier file's accumulated output is not appended to it.
#[test]
fn a_newer_files_native_terminal_replaces_instead_of_appending() {
    let binding = matched_codex("712010");
    let mut parser = SessionRelayParser::default();
    let a = assistant("a-only-output");
    assert!(
        parser
            .ingest_frame(&read(&binding, &a, 1, 1_000, (1, 5)))
            .is_empty()
    );
    let mut terminal = read(&binding, "native terminal record\n", 2, 0, (2, 6));
    terminal.turn_start_offset = Some(0);
    terminal.terminal_consumed_end = Some(23);
    let body = single_body(parser.ingest_verified_native_terminal(&terminal, "b canonical"));
    assert_eq!(body, "b canonical");
}

#[test]
fn incident_native_watermark_then_wrapper_switch_preserves_fresh_1755_bytes() {
    let channel = 7_139_111;
    let binding = matched(&channel.to_string());
    let before = counters(channel);
    let mut parser = SessionRelayParser::default();
    let old = assistant("native-before-restart");
    assert!(
        parser
            .ingest_frame(&read(
                &binding,
                &old,
                1,
                3_964_928 - old.len() as u64,
                (1, 1)
            ))
            .is_empty()
    );
    let mut first = assistant("wrapper-prefix");
    first.push_str(&" ".repeat(16_384 - first.len()));
    assert!(
        parser
            .ingest_frame(&read(&binding, &first, 2, 0, (2, 10)))
            .is_empty()
    );
    let late = assistant("stale-native-must-not-arrive");
    assert!(
        parser
            .ingest_frame(&read(&binding, &late, 3, 3_964_928, (1, 1)))
            .is_empty()
    );
    let mut fresh = assistant("fresh-wrapper-suffix");
    let end = result("");
    fresh.push_str(&" ".repeat(1_755 - fresh.len() - end.len()));
    fresh.push_str(&end);
    assert!(
        parser
            .ingest_frame(&read(&binding, &fresh[..312], 4, 1_506_404, (2, 10)))
            .is_empty()
    );
    let body =
        single_body(parser.ingest_frame(&read(&binding, &fresh[312..], 5, 1_506_716, (2, 10))));
    assert!(body.contains("native-before-restart"));
    assert!(body.contains("wrapper-prefix"));
    assert_eq!(body.matches("fresh-wrapper-suffix").count(), 1);
    assert!(!body.contains("stale-native-must-not-arrive"));
    let after = counters(channel);
    assert_eq!((after.0 - before.0, after.1 - before.1), (1, 1));
}
