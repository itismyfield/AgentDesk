use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde::{Deserialize, Serialize};

use super::{OStore, STORE_DIR_NAME, StoreError, damage, durable};
use crate::domain::file_sync::fsync_parent_dir;
use uuid::Uuid;

pub(crate) type Hash = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TurnKey {
    pub destination: NonZeroU64,
    pub id: NonZeroU64,
    pub nonce: Uuid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TurnLease {
    pub key: TurnKey,
    pub revision: NonZeroU64,
}

pub(crate) struct PlanStamp {
    pub key: TurnKey,
    pub fingerprint: Hash,
    pub last_seq: u64,
    pub pieces: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Stage {
    Footer,
    Inflight,
    Release,
    Queue,
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Attempt {
    Prepared,
    HandedOff,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub message: NonZeroU64,
    pub bot: NonZeroU64,
}

/// The original an AGY execution answers: a Discord message or an authenticated iMessage digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgyOriginal {
    Discord { message: u64 },
    Imessage { digest: String },
}

/// The nonce binding a journal carries once inflight custody hands its execution over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgyBinding {
    pub logical_channel: u64,
    pub original: AgyOriginal,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct State {
    version: u32,
    key: TurnKey,
    revision: NonZeroU64,
    fingerprint: Hash,
    last_seq: u64,
    pub receipts: Vec<Receipt>,
    pub footer: Option<Receipt>,
    pub attempt: Option<Attempt>,
    pub ended: Option<Stage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<AgyBinding>,
}

/// AGY delivery custody under `o_store/<destination>`. It holds no pane state, so a pane `init`
/// there, readable or not, is another owner and the journal refuses to open.
pub(crate) struct AgyJournalRoot {
    dir: PathBuf,
    destination: u64,
    /// The kind of the first I/O error, which leaves the journal refusing writes.
    failed: Option<io::ErrorKind>,
}

impl AgyJournalRoot {
    pub(crate) fn open(runtime_root: &Path, destination: NonZeroU64) -> Result<Self, StoreError> {
        let store = OStore::existing(runtime_root)
            .ok_or_else(|| StoreError::Rejected("agy delivery has no O store".into()))?;
        if store.read_init(destination.get())?.is_some() {
            return Err(damage("agy delivery destination holds a pane init"));
        }
        let dir = runtime_root
            .join(STORE_DIR_NAME)
            .join(destination.get().to_string());
        durable::ensure_dir(&dir)?;
        Ok(Self {
            dir,
            destination: destination.get(),
            failed: None,
        })
    }

    pub(crate) fn destination(&self) -> u64 {
        self.destination
    }

    fn mutate<T>(&mut self, op: impl FnOnce() -> Result<T, StoreError>) -> Result<T, StoreError> {
        if self.failed.is_some() {
            return Err(StoreError::Rejected(
                "reopen the agy journal after an I/O error".into(),
            ));
        }
        let result = op();
        if let Err(StoreError::Io(error)) = &result {
            self.failed = Some(error.kind());
        }
        result
    }
}

/// The turn key and binding a delivery journal recorded for one execution id, read without its lock.
pub(crate) fn read_binding(
    runtime_root: &Path,
    destination: u64,
    id: u64,
) -> Result<Option<(TurnKey, AgyBinding)>, StoreError> {
    let path = runtime_root
        .join(STORE_DIR_NAME)
        .join(destination.to_string())
        .join("agy-delivery")
        .join(format!("{id}.json"));
    match durable::read_json::<State>(&path)? {
        Some(state) if state.version != 1 => Err(damage("agy delivery journal version unknown")),
        saved => Ok(saved.and_then(|state| state.binding.map(|binding| (state.key, binding)))),
    }
}

pub(crate) struct Journal<'a> {
    store: &'a mut AgyJournalRoot,
    path: PathBuf,
    state: State,
    _lock: durable::LockedFile,
}

impl<'a> Journal<'a> {
    pub(crate) fn open(
        store: &'a mut AgyJournalRoot,
        plan: PlanStamp,
        lease: Option<TurnLease>,
    ) -> Result<Self, StoreError> {
        if lease.is_some_and(|lease| lease.key != plan.key)
            || plan.key.destination.get() != store.destination
        {
            return Err(damage("agy delivery identity differs from custody"));
        }
        let dir = store.dir.join("agy-delivery");
        let path = dir.join(format!("{}.json", plan.key.id));
        durable::ensure_dir(&dir)?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
        let lock = durable::LockedFile::try_lock(
            options.open(dir.join(format!("{}.lock", plan.key.id)))?,
        )?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(damage("agy delivery journal is not a file"));
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let saved = durable::read_json::<State>(&path)?;
        let state = match saved {
            Some(state) => {
                if state.version != 1
                    || state.key != plan.key
                    || lease.is_some_and(|lease| state.revision != lease.revision)
                    || state.fingerprint != plan.fingerprint
                    || state.last_seq != plan.last_seq
                    || state.receipts.len() > plan.pieces
                    || (state.ended.is_some() && state.receipts.len() != plan.pieces)
                    || (state.ended.is_some_and(|stage| stage != Stage::Footer)
                        && (state.footer.is_none() || state.attempt.is_some()))
                    || (matches!(state.ended, None | Some(Stage::Footer)) && state.footer.is_some())
                    || (state.ended.is_none()
                        && state.receipts.len() == plan.pieces
                        && state.attempt.is_some())
                {
                    return Err(damage("agy Done, lease or delivery ledger conflict"));
                }
                // A previous replace can have published before failing directory sync.
                fsync_parent_dir(&path)?;
                state
            }
            None => State {
                version: 1,
                key: plan.key,
                revision: lease
                    .ok_or_else(|| StoreError::Rejected("agy recovery requires a journal".into()))?
                    .revision,
                fingerprint: plan.fingerprint,
                last_seq: plan.last_seq,
                receipts: Vec::new(),
                footer: None,
                attempt: None,
                ended: None,
                binding: None,
            },
        };
        Ok(Self {
            store,
            path,
            state,
            _lock: lock,
        })
    }

    pub(crate) fn state(&self) -> &State {
        &self.state
    }

    pub(crate) fn lease(&self) -> TurnLease {
        TurnLease {
            key: self.state.key,
            revision: self.state.revision,
        }
    }

    pub(crate) fn commit(&mut self, f: impl FnOnce(&mut State)) -> Result<(), StoreError> {
        let mut next = self.state.clone();
        f(&mut next);
        let payload = serde_json::to_vec(&next)?;
        let path = &self.path;
        self.store
            .mutate(|| Ok(durable::replace(path, &payload)?))?;
        self.state = next;
        Ok(())
    }
}

#[cfg(test)]
#[path = "agy_delivery_root_tests.rs"]
mod root_tests;
