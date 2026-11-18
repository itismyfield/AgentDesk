use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Serialize, de::DeserializeOwned};

use crate::services::discord::runtime_store;

const LOCK_FILE: &str = "gate.lock";
const ENTRIES_DIR: &str = "entries";

// The host's durable gate directory, held by one writer process for its whole lifetime.
pub(super) struct JournalDir {
    root: PathBuf,
    _lock: File,
}

#[derive(Debug)]
pub(super) enum OpenError {
    Busy,
    Io(io::Error),
}

impl From<io::Error> for OpenError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl JournalDir {
    pub(super) fn open(root: &Path) -> Result<Self, OpenError> {
        let entries = root.join(ENTRIES_DIR);
        fs::create_dir_all(&entries)?;
        runtime_store::fsync_parent_dir(root)?;
        runtime_store::fsync_parent_dir(&entries)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(root.join(LOCK_FILE))?;
        lock.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => OpenError::Busy,
            fs::TryLockError::Error(error) => OpenError::Io(error),
        })?;
        Ok(Self {
            root: root.to_path_buf(),
            _lock: lock,
        })
    }

    // A missing file reads as None; an unreadable one is an error, never an empty journal.
    pub(super) fn read<T: DeserializeOwned>(&self, name: &str) -> io::Result<Option<T>> {
        match fs::read(self.root.join(name)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) fn write<T: Serialize>(&self, name: &str, value: &T) -> io::Result<()> {
        write_durable(&self.root, name, value)
    }

    pub(super) fn entries<T: DeserializeOwned>(
        &self,
        key: impl Fn(&T) -> &str,
    ) -> io::Result<Vec<T>> {
        let mut entries = Vec::new();
        for item in fs::read_dir(self.root.join(ENTRIES_DIR))? {
            let item = item?;
            let path = item.path();
            if path.extension().is_some_and(|e| e == "json") {
                if !item.file_type()?.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid journal entry type",
                    ));
                }
                let entry = serde_json::from_slice(&fs::read(&path)?).map_err(io::Error::other)?;
                if item.file_name() != entry_name(key(&entry)).as_str() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "journal entry filename does not match its action",
                    ));
                }
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    pub(super) fn write_entry<T: Serialize>(&self, key: &str, value: &T) -> io::Result<()> {
        write_durable(&self.root.join(ENTRIES_DIR), &entry_name(key), value)
    }
}

fn entry_name(key: &str) -> String {
    format!("{}.json", blake3::hash(key.as_bytes()).to_hex())
}

// The new content is synced before the rename and the directory after it, so a crash leaves
// either the old record or the new one.
fn write_durable<T: Serialize>(dir: &Path, name: &str, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let temp = dir.join(format!(".{name}.tmp"));
    let mut file = File::create(&temp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let target = dir.join(name);
    fs::rename(&temp, &target)?;
    runtime_store::fsync_parent_dir(&target)
}
