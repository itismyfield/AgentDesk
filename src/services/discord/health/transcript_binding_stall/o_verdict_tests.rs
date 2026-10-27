use std::sync::{Arc, Mutex};

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::writer::progress::{ChannelProgress, Publisher};

const T: u64 = BOUND_TRANSCRIPT_FROZEN_ALERT_SECS;

fn o_view(
    witness: OWitness,
    turn_open: bool,
    turn_age: u64,
    idle: u64,
) -> TranscriptBindingStallView {
    TranscriptBindingStallView {
        relay_attached: true,
        tmux_alive: true,
        bound_transcript_idle_secs: Some(idle),
        inflight_present: true,
        turn_age_secs: Some(turn_age),
        o: Some(OTurn { witness, turn_open }),
        ..Default::default()
    }
}

fn seen(debt: usize, unsealed: bool, still_secs: u64) -> OWitness {
    OWitness::Seen {
        debt,
        unsealed,
        still_secs,
    }
}

/// 10-22 13:11Z: a prompt 28 s old, waiting on a transcript whose last turn ended 9841 s ago.
#[test]
fn a_turn_younger_than_the_threshold_is_not_frozen_on_an_old_transcript() {
    let legacy = TranscriptBindingStallView {
        relay_attached: true,
        tmux_alive: true,
        bound_transcript_idle_secs: Some(9841),
        inflight_present: true,
        turn_age_secs: Some(28),
        ..Default::default()
    };
    assert_eq!(
        classify_transcript_binding_stall(legacy),
        TranscriptBindingStall::None
    );
    let o = o_view(seen(0, false, 9841), true, 28, 9841);
    assert_eq!(
        classify_transcript_binding_stall(o),
        TranscriptBindingStall::None
    );
    assert_eq!(
        classify_transcript_binding_stall(TranscriptBindingStallView {
            turn_age_secs: Some(T),
            ..legacy
        }),
        TranscriptBindingStall::FrozenWithLiveTurn,
        "the same turn, once it has waited the threshold, still alerts"
    );
}

/// 10-23 01:56Z: Codex ended the turn with an empty answer, O posted its footer and moved no
/// further, and the mailbox kept the turn open for 30 minutes.
#[test]
fn the_drained_turn_left_open_alerts_as_an_open_mailbox() {
    let stall = classify_transcript_binding_stall(o_view(seen(0, false, 300), true, 306, 306));
    assert_eq!(stall, TranscriptBindingStall::ODrainedTurnOpen);
    assert!(stall.is_alerting());
    assert!(stall.describe().contains("mailbox still holds it open"));
}

#[test]
fn o_debt_alerts_only_once_the_ledger_has_stood_still() {
    assert_eq!(
        classify_transcript_binding_stall(o_view(seen(2, false, T - 1), true, 900, 900)),
        TranscriptBindingStall::None,
        "O still preparing pieces is delivery in progress"
    );
    assert_eq!(
        classify_transcript_binding_stall(o_view(seen(2, true, T), true, 900, 0)),
        TranscriptBindingStall::ODeliveryStalled,
        "owed pieces with a ledger still for the threshold stall, whatever the transcript does"
    );
}

#[test]
fn an_idle_o_channel_with_a_leftover_row_is_quiet() {
    let idle = o_view(seen(0, false, 86_400), false, 86_400, 86_400);
    assert!(
        idle.inflight_present,
        "fixture: the Legacy row is still on disk"
    );
    assert_eq!(
        classify_transcript_binding_stall(idle),
        TranscriptBindingStall::None
    );
}

#[test]
fn the_native_turn_at_work_is_not_a_delivery_stall() {
    assert_eq!(
        classify_transcript_binding_stall(o_view(seen(0, true, 900), true, 900, 900)),
        TranscriptBindingStall::None,
        "an announced unit without its seal is a tool or answer still running"
    );
    assert_eq!(
        classify_transcript_binding_stall(o_view(seen(0, false, 2_000), true, 900, 900)),
        TranscriptBindingStall::None,
        "O posting nothing since before the turn began is the native turn thinking"
    );
    assert_eq!(
        classify_transcript_binding_stall(o_view(seen(0, false, 600), true, 900, T - 1)),
        TranscriptBindingStall::None,
        "a transcript still growing is the native turn still writing"
    );
}

#[test]
fn an_o_turn_without_a_running_writer_fails_closed() {
    assert_eq!(
        classify_transcript_binding_stall(o_view(OWitness::Unwitnessed, true, T, 0)),
        TranscriptBindingStall::OUnwitnessed
    );
    assert_eq!(
        classify_transcript_binding_stall(o_view(OWitness::Unwitnessed, false, T, 0)),
        TranscriptBindingStall::None
    );
}

#[test]
fn a_position_first_seen_after_a_restart_starts_unobserved() {
    let channel = 7_333_101;
    let now = tokio::time::Instant::now();
    let at = |serial| ChannelProgress {
        debt: 1,
        unsealed: false,
        serial,
    };
    let still = |witness| match witness {
        OWitness::Seen { still_secs, .. } => still_secs,
        OWitness::Unwitnessed => panic!("a published position is witnessed"),
    };
    assert_eq!(
        still(o_witness::witness_for_tests(channel, Some(at(5)), now)),
        0
    );
    let later = now + std::time::Duration::from_secs(T);
    assert_eq!(
        still(o_witness::witness_for_tests(channel, Some(at(5)), later)),
        T
    );
    assert_eq!(
        still(o_witness::witness_for_tests(channel, Some(at(6)), later)),
        0
    );
    assert_eq!(
        o_witness::witness_for_tests(channel, None, later),
        OWitness::Unwitnessed
    );
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_logs(run: impl FnOnce()) -> String {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .without_time()
        .with_writer(captured.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    tracing::subscriber::with_default(subscriber, run);
    String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned()
}

fn old_transcript() -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().expect("transcript fixture");
    let old = filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() - 3_600, 0);
    filetime::set_file_mtime(file.path(), old).expect("age transcript");
    file
}

/// Every snapshot consumer of a tick and every later tick reads the same stall.
#[test]
fn a_continuing_stall_warns_once_across_consumers_and_ticks() {
    let transcript = old_transcript();
    let path = transcript.path().to_string_lossy().into_owned();
    let read = |turn_age_secs| {
        resolve_transcript_binding_stall(
            "codex",
            Some("AgentDesk-codex-7333-warn-once"),
            Some(&path),
            None,
            true,
            Some(true),
            TurnFacts {
                channel_id: 7_333_201,
                turn_open: true,
                turn_age_secs: Some(turn_age_secs),
                inflight_present: true,
            },
        )
    };
    let logs = capture_logs(|| {
        for _tick in 0..3 {
            for _consumer in 0..5 {
                assert_eq!(read(3_600), TranscriptBindingStall::FrozenWithLiveTurn);
            }
        }
        assert_eq!(read(10), TranscriptBindingStall::None);
        assert_eq!(read(3_600), TranscriptBindingStall::FrozenWithLiveTurn);
    });
    let warns = logs
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("frozen_with_live_turn"))
        .count();
    assert_eq!(warns, 2, "one WARN per start of the stall:\n{logs}");
    assert_eq!(
        logs.matches("delivery binding stall cleared").count(),
        1,
        "{logs}"
    );
}

#[tokio::test]
async fn an_o_delivery_stall_reaches_health_and_leaves_when_o_moves() {
    let channel = 7_333_301;
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[(
        channel,
        RuntimeHandoffKind::CodexTui,
    )]);
    let actor = Publisher::new(channel);
    let at = |serial| ChannelProgress {
        debt: 2,
        unsealed: false,
        serial,
    };
    actor.publish(at(10));
    let read = || {
        resolve_transcript_binding_stall(
            "codex",
            Some("AgentDesk-codex-7333-o-health"),
            None,
            None,
            false,
            Some(true),
            TurnFacts {
                channel_id: channel,
                turn_open: true,
                turn_age_secs: Some(900),
                inflight_present: false,
            },
        )
    };
    let reason = format!("o_stall:o_delivery_stalled:codex:{channel}");
    assert_eq!(
        read(),
        TranscriptBindingStall::None,
        "a process that just started has not seen the ledger stand still"
    );
    o_witness::backdate_for_tests(channel, 10, T + 100);
    assert_eq!(read(), TranscriptBindingStall::ODeliveryStalled);
    assert!(health_reasons().contains(&reason), "{:?}", health_reasons());
    actor.publish(at(11));
    assert_eq!(read(), TranscriptBindingStall::None);
    assert!(!health_reasons().contains(&reason));
}
