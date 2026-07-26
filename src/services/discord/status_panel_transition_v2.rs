//! Dormant, non-authoritative status-panel transition substrate for #4891 Slice 1.
//!
//! This module deliberately has no production caller. It does not create, bind,
//! delete, or mutate any legacy status-panel store. Recovery is represented as a
//! dry-run plan, and an ACK is explicitly unverified until the journal owns it.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A transition's tagged state. `CandidateAcknowledged` is intentionally not a
/// state: an ACK is an observation, not authority to bind a Discord message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaggedState {
    Prepared,
    AckUnverified {
        candidate_message_id: u64,
    },
    BindAuthorized {
        candidate_message_id: u64,
    },
    RetireBeforeDelete {
        candidate_message_id: u64,
    },
    Retired,
    #[serde(rename = "quarantined")]
    Quarantined {
        reason: QuarantineReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    JournalMissing,
    JournalOwned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionState {
    Unprotected,
    Protected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionRecord {
    pub channel_id: u64,
    pub candidate_message_id: Option<u64>,
    pub prior_message_id: Option<u64>,
    pub state: TaggedState,
    pub journal: JournalState,
    pub protection: ProtectionState,
}

impl TransitionRecord {
    pub fn prepared(channel_id: u64) -> Self {
        Self {
            channel_id,
            candidate_message_id: None,
            prior_message_id: None,
            state: TaggedState::Prepared,
            journal: JournalState::JournalMissing,
            protection: ProtectionState::Unprotected,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReduceEvent {
    DiscordAckObserved { candidate_message_id: u64 },
    JournalOwnershipConfirmed,
    RequestRetirement,
    DeleteObserved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReduceError {
    InvalidTransition,
    CandidateMismatch,
}

impl fmt::Display for ReduceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTransition => f.write_str("invalid status-panel transition"),
            Self::CandidateMismatch => f.write_str("status-panel candidate does not match"),
        }
    }
}

/// Pure reducer. ACK and protection are committed together; no I/O or network
/// operation is performed here.
pub fn reduce(
    mut record: TransitionRecord,
    event: ReduceEvent,
) -> Result<TransitionRecord, ReduceError> {
    match event {
        ReduceEvent::DiscordAckObserved {
            candidate_message_id,
        } => {
            if !matches!(record.state, TaggedState::Prepared) || candidate_message_id == 0 {
                return Err(ReduceError::InvalidTransition);
            }
            record.candidate_message_id = Some(candidate_message_id);
            record.protection = ProtectionState::Protected;
            record.state = TaggedState::AckUnverified {
                candidate_message_id,
            };
            Ok(record)
        }
        ReduceEvent::JournalOwnershipConfirmed => {
            let Some(candidate_message_id) = record.candidate_message_id else {
                return Err(ReduceError::InvalidTransition);
            };
            if !matches!(record.state, TaggedState::AckUnverified { .. }) {
                return Err(ReduceError::InvalidTransition);
            }
            if !matches!(record.protection, ProtectionState::Protected) {
                return Err(ReduceError::InvalidTransition);
            }
            record.journal = JournalState::JournalOwned;
            record.state = TaggedState::BindAuthorized {
                candidate_message_id,
            };
            Ok(record)
        }
        ReduceEvent::RequestRetirement => {
            let Some(candidate_message_id) = record.candidate_message_id else {
                return Err(ReduceError::InvalidTransition);
            };
            if !matches!(record.state, TaggedState::BindAuthorized { .. }) {
                return Err(ReduceError::InvalidTransition);
            }
            record.state = TaggedState::RetireBeforeDelete {
                candidate_message_id,
            };
            Ok(record)
        }
        ReduceEvent::DeleteObserved => {
            if !matches!(record.state, TaggedState::RetireBeforeDelete { .. }) {
                return Err(ReduceError::InvalidTransition);
            }
            record.state = TaggedState::Retired;
            Ok(record)
        }
    }
}

/// Binding is admitted only after both typed predicates are true.
pub fn bind_is_authorized(state: &TaggedState, journal: &JournalState) -> bool {
    matches!(state, TaggedState::BindAuthorized { .. })
        && matches!(journal, JournalState::JournalOwned)
}

/// Physical deletion is never admitted before the retire transition.
pub fn retire_before_delete(state: &TaggedState) -> bool {
    matches!(
        state,
        TaggedState::RetireBeforeDelete { .. } | TaggedState::Retired
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineReason {
    Malformed,
    UnknownState,
    CandidateOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateClassification {
    Valid(TransitionRecord),
    MalformedCandidateOnly,
    Malformed,
    UnknownState,
}

/// Classifies a candidate file without mutating it. A malformed candidate-only
/// payload is kept distinct from a missing file so recovery cannot recreate it.
pub fn classify_candidate(raw: &str) -> CandidateClassification {
    let value = match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => value,
        Err(_) => return CandidateClassification::Malformed,
    };
    let Some(state) = value.get("state") else {
        return if value.get("candidate_message_id").is_some() {
            CandidateClassification::MalformedCandidateOnly
        } else {
            CandidateClassification::Malformed
        };
    };
    let Some(state_name) = state.get("state").and_then(serde_json::Value::as_str) else {
        return CandidateClassification::Malformed;
    };
    let known = [
        "prepared",
        "ack_unverified",
        "bind_authorized",
        "retire_before_delete",
        "retired",
        "quarantined",
    ];
    if !known.contains(&state_name) {
        return CandidateClassification::UnknownState;
    }
    match serde_json::from_value::<TransitionRecord>(value) {
        Ok(record) => CandidateClassification::Valid(record),
        Err(_) => CandidateClassification::Malformed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    NoAction,
    KeepPrepared,
    QuarantineMalformedCandidate,
    QuarantineMalformed,
    QuarantineUnknownState,
    RetireBeforeDelete,
}

/// Produces decisions only. It cannot access transport, legacy stores, or the
/// filesystem, so a dry-run can never accidentally perform network action.
pub fn plan_recovery(classification: CandidateClassification) -> RecoveryDecision {
    match classification {
        CandidateClassification::Valid(record) => match record.state {
            TaggedState::Prepared => RecoveryDecision::KeepPrepared,
            TaggedState::RetireBeforeDelete { .. } => RecoveryDecision::RetireBeforeDelete,
            TaggedState::Retired | TaggedState::Quarantined { .. } => RecoveryDecision::NoAction,
            TaggedState::AckUnverified { .. } | TaggedState::BindAuthorized { .. } => {
                RecoveryDecision::NoAction
            }
        },
        CandidateClassification::MalformedCandidateOnly => {
            RecoveryDecision::QuarantineMalformedCandidate
        }
        CandidateClassification::Malformed => RecoveryDecision::QuarantineMalformed,
        CandidateClassification::UnknownState => RecoveryDecision::QuarantineUnknownState,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistenceError {
    Missing,
    Malformed,
    UnknownState,
    RevisionConflict { expected: u64, actual: u64 },
    Io(String),
    UnsupportedPlatform,
}

impl fmt::Display for PersistenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => f.write_str("status-panel channel snapshot is missing"),
            Self::Malformed => f.write_str("status-panel channel snapshot is malformed"),
            Self::UnknownState => f.write_str("status-panel channel snapshot has unknown state"),
            Self::RevisionConflict { expected, actual } => {
                write!(
                    f,
                    "stale status-panel revision: expected {expected}, actual {actual}"
                )
            }
            Self::Io(error) => f.write_str(error),
            Self::UnsupportedPlatform => {
                f.write_str("durable parent-directory fsync is unsupported")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionedSnapshot {
    pub revision: u64,
    pub record: TransitionRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelKey {
    pub provider: String,
    pub token_hash: String,
    pub channel_id: u64,
}

impl ChannelKey {
    pub fn new(
        provider: impl Into<String>,
        token_hash: impl Into<String>,
        channel_id: u64,
    ) -> Self {
        Self {
            provider: provider.into(),
            token_hash: token_hash.into(),
            channel_id,
        }
    }
}

pub struct ChannelPersistence {
    snapshot_path: PathBuf,
    lock_path: PathBuf,
}

impl ChannelPersistence {
    pub fn new(root: impl AsRef<Path>, key: &ChannelKey) -> Result<Self, PersistenceError> {
        if key.channel_id == 0 || !safe_component(&key.provider) || !safe_component(&key.token_hash)
        {
            return Err(PersistenceError::Io(
                "invalid status-panel channel key".to_string(),
            ));
        }
        let dir = root.as_ref().join(&key.provider).join(&key.token_hash);
        Ok(Self {
            snapshot_path: dir.join(format!("channel-{}.json", key.channel_id)),
            lock_path: dir.join(format!("channel-{}.lock", key.channel_id)),
        })
    }

    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }

    pub fn load(&self) -> Result<VersionedSnapshot, PersistenceError> {
        let raw = fs::read_to_string(&self.snapshot_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                PersistenceError::Missing
            } else {
                PersistenceError::Io(error.to_string())
            }
        })?;
        parse_snapshot(&raw)
    }

    /// Writes revision `expected + 1` only when the on-disk revision equals
    /// `expected`. The channel flock is stable and never derived from a nonce.
    pub fn compare_and_swap(
        &self,
        expected: Option<u64>,
        record: &TransitionRecord,
    ) -> Result<VersionedSnapshot, PersistenceError> {
        let _lock = ChannelFileLock::acquire(&self.lock_path)?;
        let actual = match self.load() {
            Ok(snapshot) => Some(snapshot.revision),
            Err(PersistenceError::Missing) => None,
            Err(error) => return Err(error),
        };
        if actual != expected {
            return Err(PersistenceError::RevisionConflict {
                expected: expected.unwrap_or(0),
                actual: actual.unwrap_or(0),
            });
        }
        let snapshot = VersionedSnapshot {
            revision: expected.unwrap_or(0) + 1,
            record: record.clone(),
        };
        write_snapshot(&self.snapshot_path, &snapshot)?;
        Ok(snapshot)
    }
}

fn safe_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && component
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn parse_snapshot(raw: &str) -> Result<VersionedSnapshot, PersistenceError> {
    let value =
        serde_json::from_str::<serde_json::Value>(raw).map_err(|_| PersistenceError::Malformed)?;
    let Some(state) = value
        .get("record")
        .and_then(|record| record.get("state"))
        .and_then(|state| state.get("state"))
        .and_then(serde_json::Value::as_str)
    else {
        return Err(PersistenceError::Malformed);
    };
    let known = [
        "prepared",
        "ack_unverified",
        "bind_authorized",
        "retire_before_delete",
        "retired",
        "quarantined",
    ];
    if !known.contains(&state) {
        return Err(PersistenceError::UnknownState);
    }
    serde_json::from_value(value).map_err(|_| PersistenceError::Malformed)
}

fn write_snapshot(path: &Path, snapshot: &VersionedSnapshot) -> Result<(), PersistenceError> {
    let parent = path
        .parent()
        .ok_or_else(|| PersistenceError::Io("snapshot has no parent".to_string()))?;
    fs::create_dir_all(parent).map_err(|error| PersistenceError::Io(error.to_string()))?;
    let bytes = serde_json::to_vec_pretty(snapshot)
        .map_err(|error| PersistenceError::Io(error.to_string()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("snapshot");
    let temp = parent.join(format!(".{file_name}.tmp-v2"));
    let mut file = File::create(&temp).map_err(|error| PersistenceError::Io(error.to_string()))?;
    file.write_all(&bytes)
        .map_err(|error| PersistenceError::Io(error.to_string()))?;
    failpoint(Failpoint::FileSync)?;
    file.sync_all()
        .map_err(|error| PersistenceError::Io(error.to_string()))?;
    fs::rename(&temp, path).map_err(|error| PersistenceError::Io(error.to_string()))?;
    failpoint(Failpoint::ParentDirectorySync)?;
    sync_parent_directory(parent)
}

fn sync_parent_directory(parent: &Path) -> Result<(), PersistenceError> {
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| PersistenceError::Io(error.to_string()))
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Err(PersistenceError::UnsupportedPlatform)
    }
}

pub struct ChannelFileLock {
    file: File,
}

impl ChannelFileLock {
    fn acquire(path: &Path) -> Result<Self, PersistenceError> {
        let parent = path
            .parent()
            .ok_or_else(|| PersistenceError::Io("lock has no parent".to_string()))?;
        fs::create_dir_all(parent).map_err(|error| PersistenceError::Io(error.to_string()))?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(|error| PersistenceError::Io(error.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(PersistenceError::Io(
                    std::io::Error::last_os_error().to_string(),
                ));
            }
        }
        Ok(Self { file })
    }
}

impl Drop for ChannelFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failpoint {
    FileSync,
    ParentDirectorySync,
}

#[cfg(test)]
static ACTIVE_FAILPOINT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
#[cfg(test)]
static PERSISTENCE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn failpoint(point: Failpoint) -> Result<(), PersistenceError> {
    #[cfg(test)]
    {
        use std::sync::atomic::Ordering;
        let expected = match point {
            Failpoint::FileSync => 1,
            Failpoint::ParentDirectorySync => 2,
        };
        if ACTIVE_FAILPOINT
            .compare_exchange(expected, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(PersistenceError::Io(format!(
                "deterministic failpoint: {point:?}"
            )));
        }
        return Ok(());
    }
    #[cfg(not(test))]
    {
        let _ = point;
        Ok(())
    }
}

#[cfg(test)]
pub fn fail_next_for_test(point: Failpoint) {
    use std::sync::atomic::Ordering;
    let value = match point {
        Failpoint::FileSync => 1,
        Failpoint::ParentDirectorySync => 2,
    };
    ACTIVE_FAILPOINT.store(value, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_is_not_bindable() {
        let record = TransitionRecord::prepared(1);
        assert!(!bind_is_authorized(&record.state, &record.journal));
    }

    #[test]
    fn ack_and_protection_are_atomic_in_reducer() {
        let record = reduce(
            TransitionRecord::prepared(1),
            ReduceEvent::DiscordAckObserved {
                candidate_message_id: 7,
            },
        )
        .unwrap();
        assert_eq!(
            record.state,
            TaggedState::AckUnverified {
                candidate_message_id: 7
            }
        );
        assert_eq!(record.protection, ProtectionState::Protected);
        assert!(!bind_is_authorized(&record.state, &record.journal));
    }

    #[test]
    fn malformed_candidate_only_is_quarantined_without_becoming_missing() {
        let classification = classify_candidate(r#"{"candidate_message_id":7}"#);
        assert_eq!(
            classification,
            CandidateClassification::MalformedCandidateOnly
        );
        assert_eq!(
            plan_recovery(classification),
            RecoveryDecision::QuarantineMalformedCandidate
        );
    }

    #[test]
    fn unknown_state_is_quarantined() {
        let classification = classify_candidate(r#"{"state":{"state":"future_state"}}"#);
        assert_eq!(classification, CandidateClassification::UnknownState);
        assert_eq!(
            plan_recovery(classification),
            RecoveryDecision::QuarantineUnknownState
        );
    }

    #[test]
    fn delete_cannot_precede_retirement() {
        let record = TransitionRecord::prepared(1);
        assert_eq!(
            reduce(record, ReduceEvent::DeleteObserved),
            Err(ReduceError::InvalidTransition)
        );
    }

    #[test]
    fn journal_owned_is_required_for_bind() {
        let mut record = reduce(
            TransitionRecord::prepared(1),
            ReduceEvent::DiscordAckObserved {
                candidate_message_id: 7,
            },
        )
        .unwrap();
        record.state = TaggedState::BindAuthorized {
            candidate_message_id: 7,
        };
        assert!(!bind_is_authorized(
            &record.state,
            &JournalState::JournalMissing
        ));
        assert!(bind_is_authorized(
            &record.state,
            &JournalState::JournalOwned
        ));
    }

    #[test]
    fn retire_must_precede_delete() {
        assert!(!retire_before_delete(&TaggedState::BindAuthorized {
            candidate_message_id: 7
        }));
        assert!(retire_before_delete(&TaggedState::RetireBeforeDelete {
            candidate_message_id: 7
        }));
    }

    #[test]
    fn stale_revision_is_rejected() {
        let _guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store =
            ChannelPersistence::new(root.path(), &ChannelKey::new("claude", "token", 9)).unwrap();
        let record = TransitionRecord::prepared(9);
        let first = store.compare_and_swap(None, &record).unwrap();
        assert_eq!(first.revision, 1);
        let error = store.compare_and_swap(None, &record).unwrap_err();
        assert!(
            matches!(error, PersistenceError::RevisionConflict { .. }),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn parent_fsync_failure_is_fail_closed() {
        let _guard = PERSISTENCE_TEST_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store =
            ChannelPersistence::new(root.path(), &ChannelKey::new("claude", "token", 10)).unwrap();
        fail_next_for_test(Failpoint::ParentDirectorySync);
        let error = store
            .compare_and_swap(None, &TransitionRecord::prepared(10))
            .unwrap_err();
        assert!(matches!(error, PersistenceError::Io(_)));
    }

    #[test]
    fn malformed_is_not_missing() {
        let root = tempfile::tempdir().unwrap();
        let store =
            ChannelPersistence::new(root.path(), &ChannelKey::new("claude", "token", 11)).unwrap();
        fs::create_dir_all(store.snapshot_path().parent().unwrap()).unwrap();
        fs::write(store.snapshot_path(), b"not-json").unwrap();
        assert!(matches!(store.load(), Err(PersistenceError::Malformed)));
    }

    #[test]
    fn recovery_plan_has_no_network_action_under_lock() {
        let plan = plan_recovery(CandidateClassification::Valid(TransitionRecord::prepared(
            12,
        )));
        assert_eq!(plan, RecoveryDecision::KeepPrepared);
    }
}
