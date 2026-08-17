//! One row-independent reachability observation tick.
//!
//! This module records transcript facts only. Its state is telemetry for later
//! composition; it never authorizes relay, recovery, or health decisions.

use std::path::Path;

use super::discovery::{TranscriptCandidates, TranscriptResolution, resolve_transcript};
use super::ledger::{
    LedgerIncarnation, ObservationCommit, bootstrap_ledger_at, ledger_file_exists, read_ledger_at,
    record_observation_at,
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
    /// Observation could not safely advance. Relay execution remains separate.
    Unknown { reason: ReachabilityUnknownReason },
}

fn unknown(reason: ReachabilityUnknownReason) -> ReachabilityObservationState {
    ReachabilityObservationState::Unknown { reason }
}

/// Resolve, tail, frame, and durably record one channel observation.
pub(in crate::services::discord) fn observe_channel_at(
    ledger_path: &Path,
    transcript_path: &Path,
    tmux_session_name: &str,
    generation_mtime_ns: i64,
    spawn_nonce: Option<String>,
    observed_at_epoch_ms: u64,
) -> ReachabilityObservationState {
    let transcript = match resolve_transcript(TranscriptCandidates {
        registry_output_path: Some(transcript_path),
        runtime_binding_path: None,
        discovery_roots: &[],
    }) {
        TranscriptResolution::Resolved(transcript) => transcript,
        TranscriptResolution::Unresolved(reason) => return unknown(reason),
    };
    if generation_mtime_ns <= 0 {
        return unknown(ReachabilityUnknownReason::TranscriptUnresolved);
    }

    let incarnation = LedgerIncarnation::new(
        tmux_session_name.to_string(),
        generation_mtime_ns,
        spawn_nonce,
        transcript.stat.file_id,
    );
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
        // A valid, different incarnation is retired explicitly and starts at
        // its current EOF. `bootstrap_ledger_at` rechecks under the file lock.
        return match bootstrap_ledger_at(ledger_path, incarnation, transcript.stat.len) {
            Ok(()) => ReachabilityObservationState::Bootstrapped,
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
        generation_mtime_ns,
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
    use std::io::Write;

    use tempfile::TempDir;

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
        observe_channel_at(
            ledger,
            transcript,
            "agent-session",
            GENERATION,
            Some("nonce".to_string()),
            now,
        )
    }

    fn append(path: &Path, bytes: &[u8]) {
        fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open transcript")
            .write_all(bytes)
            .expect("append transcript");
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
}
