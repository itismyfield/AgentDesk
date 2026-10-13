use chrono::TimeZone;

use super::super::tests::{enabled, initialized};
use super::super::{OEra, OStore, STORE_DIR_NAME};
use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;

/// The cursor as a binary from before the transition record reads and writes it.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct OlderCursor {
    source: SourceId,
    captured_through: u64,
    prefix_hash: String,
    retired: bool,
}

struct Fixture {
    runtime: tempfile::TempDir,
    store: OStore,
    era: OEra,
    source: SourceId,
}

impl Fixture {
    fn new() -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let path = runtime.path().join("t.jsonl");
        std::fs::write(&path, b"L1\n").unwrap();
        let source = source_id_for("s1", &path).unwrap();
        let store = enabled(runtime.path());
        let era = (store.begin_era(&[7], Utc::now(), |c| Ok(initialized(c, Vec::new())))).unwrap();
        Self {
            runtime,
            store,
            era,
            source,
        }
    }

    fn open(&self) -> ChannelStore {
        self.store.open_channel(&self.era, 7).unwrap().unwrap()
    }

    fn channel_dir(&self) -> PathBuf {
        self.runtime.path().join(STORE_DIR_NAME).join("7")
    }

    fn cursor_bytes(&self) -> Vec<u8> {
        let name = format!("{}.json", source_key(&self.source));
        std::fs::read(self.channel_dir().join(CURSOR_DIR).join(name)).unwrap()
    }

    fn older(&self, retired: bool) -> Vec<u8> {
        let older = OlderCursor {
            source: self.source.clone(),
            captured_through: 0,
            prefix_hash: hex::encode(Sha256::digest(b"")),
            retired,
        };
        serde_json::to_vec(&older).unwrap()
    }
}

fn record(source: &SourceId) -> TransitionCleanup {
    TransitionCleanup {
        job_id: "hrot-e2e-1".into(),
        operator: "operator".into(),
        at: Utc.timestamp_opt(2_000_000, 0).unwrap(),
        reason: "herdr transition".into(),
        cutoff: 62,
        hop_seq: 2,
        verified: VerifiedEnd {
            offset: 0,
            prefix_hash: hex::encode(Sha256::digest(b"")),
            fingerprint: Fingerprint::path(source).unwrap(),
        },
        ended: ExecutionEnd {
            host: "mac-book".into(),
            pane: "AgentDesk-claude-e2e".into(),
            first_seq: 1,
            last_seq: 62,
            basis: "tmux server killed before the switch".into(),
        },
    }
}

#[test]
fn transition_cleanup_absent_keeps_cursor_bytes_and_older_files_read_as_none() {
    let fixture = Fixture::new();
    let mut channel = fixture.open();
    channel.attach_source(&fixture.source).unwrap();
    assert_eq!(fixture.cursor_bytes(), fixture.older(false));
    channel.set_retired(&fixture.source, true).unwrap();
    assert_eq!(fixture.cursor_bytes(), fixture.older(true));
    let reopened = fixture.open();
    let expected = Cursor {
        source: fixture.source.clone(),
        captured_through: 0,
        prefix_hash: hex::encode(Sha256::digest(b"")),
        retired: true,
        transition_cleanup: None,
    };
    assert_eq!(reopened.cursor(&fixture.source), Some(&expected));
}

#[test]
fn transition_cleanup_round_trips_and_an_older_binary_rewrite_drops_it_but_stays_retired() {
    let fixture = Fixture::new();
    let mut channel = fixture.open();
    channel.attach_source(&fixture.source).unwrap();
    let audited = Cursor {
        retired: true,
        transition_cleanup: Some(record(&fixture.source)),
        ..channel.cursor(&fixture.source).unwrap().clone()
    };
    write_cursor(
        &fixture.channel_dir(),
        &source_key(&fixture.source),
        &audited,
    )
    .unwrap();
    drop(channel);
    assert_eq!(fixture.open().cursor(&fixture.source), Some(&audited));
    // An older binary ignores the unknown field, so its rewrite of the cursor loses the record.
    let older: OlderCursor = serde_json::from_slice(&fixture.cursor_bytes()).unwrap();
    let path = (fixture.channel_dir().join(CURSOR_DIR))
        .join(format!("{}.json", source_key(&fixture.source)));
    std::fs::write(path, serde_json::to_vec(&older).unwrap()).unwrap();
    let downgraded = Cursor {
        transition_cleanup: None,
        ..audited
    };
    assert_eq!(fixture.open().cursor(&fixture.source), Some(&downgraded));
}
