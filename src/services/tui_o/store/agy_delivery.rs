use std::num::NonZeroU64;
use std::path::PathBuf;
use std::{fs, io};

use serde::{Deserialize, Serialize};

use super::{ChannelStore, StoreError, damage, durable};
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
}

pub(crate) struct Journal<'a> {
    store: &'a mut ChannelStore,
    path: PathBuf,
    state: State,
    _lock: durable::LockedFile,
}

impl<'a> Journal<'a> {
    pub(crate) fn open(
        store: &'a mut ChannelStore,
        plan: PlanStamp,
        lease: Option<TurnLease>,
    ) -> Result<Self, StoreError> {
        if lease.is_some_and(|lease| lease.key != plan.key)
            || plan.key.destination.get() != store.init().channel
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
        self.store
            .mutate(|_| Ok(durable::replace(&self.path, &payload)?))?;
        self.state = next;
        Ok(())
    }
}
