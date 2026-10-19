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

fn cursor_file(fixture: &Fixture) -> Option<Cursor> {
    let name = format!("{}.json", source_key(&fixture.source));
    let bytes = std::fs::read(fixture.channel_dir().join(CURSOR_DIR).join(name)).ok()?;
    Some(serde_json::from_slice(&bytes).unwrap())
}

#[test]
fn transition_retire_writes_retired_and_its_audit_in_one_cursor_over_the_expected_one_only() {
    let fixture = Fixture::new();
    let mut channel = fixture.open();
    channel.attach_source(&fixture.source).unwrap();
    let expected = channel.cursor(&fixture.source).unwrap().clone();
    let changed = Cursor {
        retired: true,
        ..expected.clone()
    };
    let mut moved = record(&fixture.source);
    moved.verified.offset = 1;
    let refused = [
        (changed, record(&fixture.source)),
        (expected.clone(), moved),
    ];
    for (stale, audit) in refused {
        let before = fixture.cursor_bytes();
        let result = channel.retire_transition(&stale, audit);
        assert!(matches!(result, Err(StoreError::Rejected(_))), "{result:?}");
        assert_eq!(fixture.cursor_bytes(), before);
        assert_eq!(channel.cursor(&fixture.source), Some(&expected));
    }
    channel
        .retire_transition(&expected, record(&fixture.source))
        .unwrap();
    let retired = Cursor {
        retired: true,
        transition_cleanup: Some(record(&fixture.source)),
        ..expected
    };
    assert_eq!(channel.cursor(&fixture.source), Some(&retired));
    assert_eq!(cursor_file(&fixture).as_ref(), Some(&retired));
    assert_eq!(fixture.open().cursor(&fixture.source), Some(&retired));
}

#[test]
fn transition_retire_whose_directory_sync_fails_is_not_done_though_the_new_file_shows() {
    use crate::services::tui_o::store::fault::{self, Step};
    let fixture = Fixture::new();
    let mut channel = fixture.open();
    channel.attach_source(&fixture.source).unwrap();
    let expected = channel.cursor(&fixture.source).unwrap().clone();
    let kind = std::io::ErrorKind::Other;
    let planted = fault::plant(&fixture.channel_dir(), Step::ReplaceDirSync, kind, Some(1));
    let result = channel.retire_transition(&expected, record(&fixture.source));
    drop(planted);
    assert!(matches!(result, Err(StoreError::Io(_))), "{result:?}");
    let on_disk = cursor_file(&fixture).unwrap();
    assert!(on_disk.retired, "the rename published the new cursor");
    assert_eq!(on_disk.transition_cleanup, Some(record(&fixture.source)));
    assert_eq!(channel.cursor(&fixture.source), Some(&expected));
    assert_eq!(channel.io_failure(), Some(kind));
    assert!(channel.set_retired(&fixture.source, false).is_err());
}

#[test]
fn transition_facts_keep_each_failed_read_an_error() {
    use crate::services::tui_o::store::cleanup_witness::CLEANUP_DIR;
    use crate::services::tui_o::store::rotation::{BOUNDARY_FILE, CHECKPOINT_FILE};
    let fixture = Fixture::new();
    let mut channel = fixture.open();
    channel.attach_source(&fixture.source).unwrap();
    let facts = channel.read_transition_facts([&fixture.source]);
    assert_eq!(facts.rotation, Ok(Default::default()));
    assert_eq!(facts.checkpoint, Ok(None));
    assert_eq!(facts.witnesses[&source_key(&fixture.source)], Ok(None));
    let dir = fixture.channel_dir();
    std::fs::create_dir_all(dir.join(CLEANUP_DIR)).unwrap();
    let witness = format!("{}.json", source_key(&fixture.source));
    for file in [
        dir.join(BOUNDARY_FILE),
        dir.join(CHECKPOINT_FILE),
        dir.join(CLEANUP_DIR).join(witness),
    ] {
        std::fs::write(file, b"{torn").unwrap();
    }
    assert!(channel.recorded_witness(&fixture.source).is_err());
    let facts = channel.read_transition_facts([&fixture.source]);
    assert!(facts.rotation.is_err() && facts.checkpoint.is_err());
    assert!(facts.witnesses[&source_key(&fixture.source)].is_err());
}
