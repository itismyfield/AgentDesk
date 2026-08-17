//! One row-independent reachability observation tick.
//!
//! This module records transcript facts only. Its state is telemetry for later
//! composition; it never authorizes relay, recovery, or health decisions.

use std::path::Path;

use super::discovery::{
    ResolvedTranscript, TranscriptCandidates, TranscriptResolution, resolve_transcript,
};
use super::ledger::{
    LedgerIncarnation, ObservationCommit, bootstrap_ledger_at, ledger_file_exists, read_ledger_at,
    rebootstrap_ledger_at_if_snapshot_current, record_observation_at,
};
use super::obligation::scan_canonical;
use super::tail::{TAIL_READ_CAP_BYTES, TailCursor, TailOutcome, read_incremental};
use super::verdict::ReachabilityUnknownReason;

/// What one tick managed to persist. This is observation state, not a verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ReachabilityObservationState {
    /// First sight or an explicit incarnation transition starts at the current
    /// end of file; historical bytes are not silently claimed as observed.
    Bootstrapped,
    /// Facts and cursor were committed together. `unknown_reason` records an
    /// incomplete bounded read without discarding the facts that were seen.
    Recorded {
        commit: ObservationCommit,
        unknown_reason: Option<ReachabilityUnknownReason>,
    },
    /// A watcher handoff superseded this tick before it could retire the
    /// currently live ledger incarnation.
    SkippedStaleIncarnation { skipped_observations: u64 },
    /// Observation could not safely advance. Relay execution remains separate.
    Unknown { reason: ReachabilityUnknownReason },
}

/// One coherent watcher incarnation consumed by an observation tick.
///
/// Both the initial acquisition and the under-lock revalidation build this
/// value through [`capture_watcher_incarnation`], keeping transcript identity,
/// generation, and spawn nonce in one comparison domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct WatcherIncarnationSnapshot {
    transcript: ResolvedTranscript,
    incarnation: LedgerIncarnation,
}

pub(in crate::services::discord) type WatcherIncarnationCapture =
    Result<WatcherIncarnationSnapshot, ReachabilityUnknownReason>;

fn unknown(reason: ReachabilityUnknownReason) -> ReachabilityObservationState {
    ReachabilityObservationState::Unknown { reason }
}

pub(in crate::services::discord) fn capture_watcher_incarnation(
    transcript_path: &Path,
    tmux_session_name: &str,
    generation_mtime_ns: i64,
    spawn_nonce: Option<String>,
) -> WatcherIncarnationCapture {
    let transcript = match resolve_transcript(TranscriptCandidates {
        registry_output_path: Some(transcript_path),
        runtime_binding_path: None,
        discovery_roots: &[],
    }) {
        TranscriptResolution::Resolved(transcript) => transcript,
        TranscriptResolution::Unresolved(reason) => return Err(reason),
    };
    if generation_mtime_ns <= 0 {
        return Err(ReachabilityUnknownReason::TranscriptUnresolved);
    }

    let incarnation = LedgerIncarnation::new(
        tmux_session_name.to_string(),
        generation_mtime_ns,
        spawn_nonce,
        transcript.stat.file_id,
    );
    Ok(WatcherIncarnationSnapshot {
        transcript,
        incarnation,
    })
}

/// Resolve, tail, frame, and durably record one channel observation.
pub(in crate::services::discord) fn observe_channel_at<F>(
    ledger_path: &Path,
    snapshot: WatcherIncarnationSnapshot,
    observed_at_epoch_ms: u64,
    revalidate_live_snapshot: F,
) -> ReachabilityObservationState
where
    F: FnOnce() -> Option<WatcherIncarnationSnapshot>,
{
    let WatcherIncarnationSnapshot {
        transcript,
        incarnation,
    } = snapshot;
    let ledger = match read_ledger_at(ledger_path) {
        Some(ledger) => ledger,
        None if ledger_file_exists(ledger_path) => {
            // 4987 §-1.4 counterexample 7: preserve malformed coverage. Never
            // replace it with an empty ledger that could later look healthy.
            return unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable);
        }
        None => {
            return match bootstrap_ledger_at(ledger_path, incarnation, transcript.stat.len) {
                Ok(()) => ReachabilityObservationState::Bootstrapped,
                Err(_) => unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable),
            };
        }
    };

    if !ledger.binds_to(&incarnation) {
        let tmux_session_name = incarnation.tmux_session_name.clone();
        return match rebootstrap_ledger_at_if_snapshot_current(
            ledger_path,
            incarnation,
            transcript.stat.len,
            || revalidate_live_snapshot().map(|snapshot| snapshot.incarnation),
        ) {
            Ok(true) => ReachabilityObservationState::Bootstrapped,
            Ok(false) => {
                tracing::warn!(
                    tmux_session_name,
                    skipped_observations = 1_u64,
                    "reachability observation skipped stale watcher incarnation"
                );
                ReachabilityObservationState::SkippedStaleIncarnation {
                    skipped_observations: 1,
                }
            }
            Err(_) => unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable),
        };
    }

    let cursor = ledger.cursor_offset;
    let tail = read_incremental(
        &transcript.path,
        TailCursor::new(incarnation.identity(), cursor),
    );
    let unknown_reason = tail.unknown_reason();
    let TailOutcome::Read {
        bytes,
        start,
        observed_len,
        cap_truncated,
        ..
    } = tail
    else {
        return unknown(unknown_reason.unwrap_or(ReachabilityUnknownReason::TranscriptUnresolved));
    };
    let scan = scan_canonical(
        &bytes,
        start,
        incarnation.generation_mtime_ns,
        incarnation.identity(),
        TAIL_READ_CAP_BYTES,
    );
    let incomplete = cap_truncated || scan.observation_is_incomplete();

    match record_observation_at(
        ledger_path,
        &incarnation,
        cursor,
        scan.records,
        scan.next_offset,
        observed_len,
        incomplete,
        observed_at_epoch_ms,
    ) {
        Ok(commit) => ReachabilityObservationState::Recorded {
            commit,
            unknown_reason,
        },
        Err(_) => unknown(ReachabilityUnknownReason::ReceiptStoreUnreadable),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;
    use tracing_subscriber::fmt::MakeWriter;

    use super::super::ledger::read_ledger_at;
    use super::*;

    const GENERATION: i64 = 99;
    const ASSISTANT_ROW: &[u8] = b"{\"type\":\"assistant\",\"timestamp\":\"2026-08-17T01:02:03\",\"message\":{\"model\":\"m\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n";

    fn fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let transcript = dir.path().join("transcript.jsonl");
        let ledger = dir.path().join("ledger.json");
        fs::write(&transcript, b"").expect("create transcript");
        (dir, transcript, ledger)
    }

    fn observe(transcript: &Path, ledger: &Path, now: u64) -> ReachabilityObservationState {
        let snapshot = capture_watcher_incarnation(
            transcript,
            "agent-session",
            GENERATION,
            Some("nonce".to_string()),
        )
        .expect("capture watcher incarnation");
        let live_snapshot = snapshot.clone();
        observe_channel_at(ledger, snapshot, now, || Some(live_snapshot))
    }

    fn append(path: &Path, bytes: &[u8]) {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open transcript")
            .write_all(bytes)
            .expect("append transcript");
    }

    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn observation_bootstraps_then_records_obligations_and_cursor() {
        let (_dir, transcript, ledger_path) = fixture();
        assert_eq!(
            observe(&transcript, &ledger_path, 1),
            ReachabilityObservationState::Bootstrapped
        );
        append(&transcript, ASSISTANT_ROW);

        let ReachabilityObservationState::Recorded { commit, .. } =
            observe(&transcript, &ledger_path, 2)
        else {
            panic!("observation should record")
        };
        assert_eq!(commit.obligations_appended, 1);
        let ledger = read_ledger_at(&ledger_path).expect("read ledger");
        assert_eq!(ledger.cursor_offset, ASSISTANT_ROW.len() as u64);
        assert_eq!(ledger.live_obligations().len(), 1);
    }

    #[test]
    fn malformed_ledger_stops_recording_without_returning_an_error() {
        let (_dir, transcript, ledger_path) = fixture();
        fs::write(&ledger_path, b"not-json").expect("write malformed ledger");
        append(&transcript, ASSISTANT_ROW);

        assert_eq!(
            observe(&transcript, &ledger_path, 1),
            ReachabilityObservationState::Unknown {
                reason: ReachabilityUnknownReason::ReceiptStoreUnreadable,
            }
        );
        assert_eq!(
            fs::read(&ledger_path).expect("read malformed ledger"),
            b"not-json"
        );
    }

    #[test]
    fn restarted_observer_resumes_cursor_without_double_counting() {
        let (_dir, transcript, ledger_path) = fixture();
        assert_eq!(
            observe(&transcript, &ledger_path, 1),
            ReachabilityObservationState::Bootstrapped
        );
        append(&transcript, ASSISTANT_ROW);
        let first = observe(&transcript, &ledger_path, 2);
        let second = observe(&transcript, &ledger_path, 3);
        append(&transcript, ASSISTANT_ROW);
        let third = observe(&transcript, &ledger_path, 4);

        let appended = |state| match state {
            ReachabilityObservationState::Recorded { commit, .. } => commit.obligations_appended,
            other => panic!("unexpected observation state: {other:?}"),
        };
        assert_eq!(appended(first), 1);
        assert_eq!(appended(second), 0);
        assert_eq!(appended(third), 1);

        let ledger = read_ledger_at(&ledger_path).expect("read ledger");
        assert_eq!(ledger.cursor_offset, (ASSISTANT_ROW.len() * 2) as u64);
        assert_eq!(ledger.live_obligations().len(), 2);
        assert_eq!(ledger.counters.total_obligations, 2);
    }

    #[test]
    fn stale_snapshot_cannot_retire_the_live_handoff_ledger() {
        let (_dir, stale_transcript, ledger_path) = fixture();
        let live_transcript = stale_transcript.with_file_name("live.jsonl");
        fs::write(&live_transcript, b"").expect("create live transcript");

        assert_eq!(
            observe(&live_transcript, &ledger_path, 1),
            ReachabilityObservationState::Bootstrapped
        );
        append(&live_transcript, ASSISTANT_ROW);
        assert!(matches!(
            observe(&live_transcript, &ledger_path, 2),
            ReachabilityObservationState::Recorded { .. }
        ));
        let ledger_before = read_ledger_at(&ledger_path).expect("read live ledger");
        let bytes_before = fs::read(&ledger_path).expect("read live ledger bytes");
        assert_eq!(ledger_before.live_obligations().len(), 1);

        // This is the leg-B interleaving: X is paired with G2/N2 after the
        // watcher already handed off to Y, whose ledger is now live.
        let stale_snapshot = capture_watcher_incarnation(
            &stale_transcript,
            "agent-session",
            GENERATION,
            Some("nonce".to_string()),
        )
        .expect("capture stale X/G2/N2 snapshot");
        let live_snapshot = capture_watcher_incarnation(
            &live_transcript,
            "agent-session",
            GENERATION,
            Some("nonce".to_string()),
        )
        .expect("capture live Y/G2/N2 snapshot");
        let log_buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::WARN)
            .with_writer(CapturingWriter(log_buffer.clone()))
            .finish();
        let state = tracing::subscriber::with_default(subscriber, || {
            observe_channel_at(&ledger_path, stale_snapshot, 3, || Some(live_snapshot))
        });

        assert_eq!(
            state,
            ReachabilityObservationState::SkippedStaleIncarnation {
                skipped_observations: 1,
            }
        );
        assert_eq!(
            fs::read(&ledger_path).expect("re-read live ledger bytes"),
            bytes_before,
            "a stale tick must not write the live ledger"
        );
        assert_eq!(
            read_ledger_at(&ledger_path).expect("re-read live ledger"),
            ledger_before,
            "the Y ledger and its live obligation must remain unchanged"
        );
        let logs =
            String::from_utf8(log_buffer.lock().expect("log buffer").clone()).expect("utf8 logs");
        assert_eq!(
            logs.matches("reachability observation skipped stale watcher incarnation")
                .count(),
            1,
            "one stale-snapshot log expected; logs={logs}"
        );
        assert!(logs.contains("skipped_observations=1"), "logs={logs}");
    }

    #[test]
    fn matching_handoff_snapshot_retires_and_rebootstraps() {
        let (_dir, old_transcript, ledger_path) = fixture();
        assert_eq!(
            observe(&old_transcript, &ledger_path, 1),
            ReachabilityObservationState::Bootstrapped
        );
        append(&old_transcript, ASSISTANT_ROW);
        assert!(matches!(
            observe(&old_transcript, &ledger_path, 2),
            ReachabilityObservationState::Recorded { .. }
        ));

        let live_transcript = old_transcript.with_file_name("handoff.jsonl");
        fs::write(&live_transcript, b"").expect("create handoff transcript");
        let handoff_snapshot = capture_watcher_incarnation(
            &live_transcript,
            "agent-session",
            GENERATION,
            Some("nonce".to_string()),
        )
        .expect("capture matching handoff snapshot");
        let expected_incarnation = handoff_snapshot.incarnation.clone();
        let live_snapshot = handoff_snapshot.clone();

        assert_eq!(
            observe_channel_at(&ledger_path, handoff_snapshot, 3, || Some(live_snapshot)),
            ReachabilityObservationState::Bootstrapped
        );
        let ledger = read_ledger_at(&ledger_path).expect("read handoff ledger");
        assert_eq!(ledger.incarnation, expected_incarnation);
        assert_eq!(ledger.live_obligations().len(), 0);
        assert_eq!(ledger.counters.retired_incarnation, 1);
    }
}
