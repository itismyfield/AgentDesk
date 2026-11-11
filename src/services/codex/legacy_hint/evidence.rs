use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::turn_orchestrator::QueueIdentity;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HintPath {
    pub(crate) path: PathBuf,
    parent: [u64; 2],
    file: Option<[u64; 2]>,
}

impl HintPath {
    pub(crate) fn observe(path: &Path) -> Result<Self, String> {
        let parent = std::fs::metadata(path.parent().ok_or("hint parent missing")?)
            .map_err(|error| error.to_string())?;
        if !parent.is_dir() {
            return Err("hint parent is not a directory".into());
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(handle) => {
                let file = handle.metadata().map_err(|error| error.to_string())?;
                let mut bytes = Vec::new();
                if !file.is_file() {
                    return Err("runtime hint is not a regular file".into());
                }
                handle
                    .take(10)
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                if bytes != b"codex_tui" {
                    return Err("foreign runtime hint".into());
                }
                Some([file.dev(), file.ino()])
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        Ok(Self {
            path: path.to_owned(),
            parent: [parent.dev(), parent.ino()],
            file,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeSource {
    pub(crate) path: PathBuf,
    pub(crate) session: String,
    pub(crate) nonce: String,
    pub(crate) proof_seq: u64,
    pub(crate) declared_file: [u64; 2],
    pub(crate) file: [u64; 2],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AbsenceStamp {
    pub(crate) owner: HostedOwner,
    pub(crate) row: i64,
    pub(crate) exact_key: String,
    pub(crate) front: QueueIdentity,
    pub(crate) epoch: String,
    pub(crate) paths: [HintPath; 2],
    pub(crate) nonce: Result<Option<String>, String>,
    pub(crate) source: Result<Option<NativeSource>, String>,
}

impl AbsenceStamp {
    pub(crate) fn matches(&self, current: &Self) -> bool {
        self.nonce.is_ok() && self.source.is_ok() && self == current
    }
}

#[path = "evidence_tests.rs"]
mod tests;
