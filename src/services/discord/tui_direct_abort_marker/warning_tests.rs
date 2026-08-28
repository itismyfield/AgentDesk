use super::*;
use std::io;
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone)]
struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CapturingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = CapturingWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
fn rejection_warns<F>(emit: F) -> Vec<String>
where
    F: FnOnce(),
{
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(CapturingWriter(buffer.clone()))
        .finish();
    tracing::subscriber::with_default(subscriber, emit);
    String::from_utf8(buffer.lock().unwrap().clone())
        .unwrap()
        .lines()
        .filter(|line| line.contains("DeferredClaim terminal commit evidence rejected fail-closed"))
        .map(str::to_owned)
        .collect()
}
#[test]
fn deferred_claim_probe_mismatch_is_silent_but_evidence_rejection_warns_once() {
    let marker = AbortedAnchorMarker::for_deferred_claim(
        "claude".into(),
        100,
        720,
        "tmux-100".into(),
        10_000,
        (720, "2026-06-10 13:00:00".into()),
        Some(200),
    );
    let probe_warns = rejection_warns(|| {
        assert!(!deferred_claim_live_inflight_is_pinned(
            &marker,
            Some("tmux-100"),
            721,
            "2026-06-10 13:00:01",
            Some(250),
        ));
    });
    assert!(probe_warns.is_empty(), "{probe_warns:?}");
    let mismatched_tombstone = CommitTombstone {
        provider: "claude".into(),
        channel_id: 100,
        tmux_session_name: "tmux-100".into(),
        committed_user_msg_id: 721,
        committed_started_at: "2026-06-10 13:00:01".into(),
        committed_turn_start_offset: Some(250),
        committed_terminal_evidence_offset: Some(260),
        committed_terminal_evidence_offset_recorded: true,
        committed_at_ms: 10_500,
    };
    let evidence_warns = rejection_warns(|| {
        assert!(!commit_tombstone_matches_marker(
            &marker,
            &mismatched_tombstone
        ));
    });
    assert_eq!(evidence_warns.len(), 1, "{evidence_warns:?}");
    assert!(evidence_warns[0].contains("reason=\"user_msg_id_mismatch\""));
    let mut offset_tombstone = mismatched_tombstone;
    offset_tombstone.committed_user_msg_id = 720;
    offset_tombstone.committed_terminal_evidence_offset = Some(150);
    let offset_warns = rejection_warns(|| {
        assert!(!commit_tombstone_matches_marker(&marker, &offset_tombstone));
    });
    assert_eq!(offset_warns.len(), 1, "{offset_warns:?}");
    assert!(
        offset_warns[0]
            .contains("reason=\"terminal_evidence_offset_before_marker_turn_start_offset\"")
    );
    let accepted_warns = rejection_warns(|| {
        assert!(terminal_commit_covers_marker_with_offset(
            10_500,
            &marker,
            720,
            "2026-06-10 13:00:01",
            Some(250),
        ));
    });
    assert!(accepted_warns.is_empty(), "{accepted_warns:?}");
}
