use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::tmux_common::{self as tc, TmuxSourceAuthority};
use crate::services::turn_orchestrator::{
    MarkerControl, MarkerEntry, MarkerNativeSource, MarkerOwner, MarkerPathSnapshot,
    MarkerRowExpectation, MarkerWitness, QueueIdentity,
};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HintPath {
    pub(crate) path: PathBuf,
    pub(super) parent: [u64; 2],
    pub(super) file: Option<[u64; 2]>,
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
                if !file.is_file() || file.nlink() != 1 {
                    return Err("runtime hint is not one regular entry".into());
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

    pub(super) fn unlink(&self) -> Result<(), String> {
        let directory = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(self.path.parent().ok_or("hint parent missing")?)
            .map_err(|error| error.to_string())?;
        let metadata = directory.metadata().map_err(|error| error.to_string())?;
        if [metadata.dev(), metadata.ino()] != self.parent {
            return Err("hint parent changed before unlink".into());
        }
        if let Some(expected) = self.file {
            let name = std::ffi::CString::new(
                self.path
                    .file_name()
                    .ok_or("hint basename missing")?
                    .as_bytes(),
            )
            .map_err(|_| "hint basename contains NUL")?;
            // Both operations stay relative to the observed directory even if its path is replaced.
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let handle = unsafe { std::fs::File::from_raw_fd(fd) };
            let file = handle.metadata().map_err(|error| error.to_string())?;
            let mut bytes = Vec::new();
            handle
                .take(10)
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            if !file.is_file()
                || file.nlink() != 1
                || [file.dev(), file.ino()] != expected
                || bytes != b"codex_tui"
            {
                return Err("hint entry changed before unlink".into());
            }
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        directory.sync_all().map_err(|error| error.to_string())
    }

    pub(crate) fn snapshot(&self) -> MarkerPathSnapshot {
        MarkerPathSnapshot {
            path: self.path.clone(),
            parent: self.parent,
            entry: self
                .file
                .map_or(MarkerEntry::Absent, |file| MarkerEntry::CodexTui { file }),
        }
    }
}

pub(crate) type NativeSource = MarkerNativeSource;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AbsenceStamp {
    pub(crate) owner: HostedOwner,
    pub(crate) expected: MarkerRowExpectation,
    pub(crate) front: QueueIdentity,
    pub(crate) epoch: String,
    pub(crate) paths: [HintPath; 2],
    pub(crate) nonce: Result<Option<String>, String>,
    pub(crate) source: Result<Option<NativeSource>, String>,
}

impl From<&HostedOwner> for MarkerOwner {
    fn from(owner: &HostedOwner) -> Self {
        Self {
            provider: owner.provider.clone(),
            discord_token_hash: owner.discord_token_hash.clone(),
            channel_id: owner.channel_id.clone(),
            logical_key: owner.logical_key.clone(),
            owner_node: owner.owner_node.clone(),
            runtime_root: owner.runtime_root.clone(),
        }
    }
}

impl From<&MarkerOwner> for HostedOwner {
    fn from(owner: &MarkerOwner) -> Self {
        Self {
            provider: owner.provider.clone(),
            discord_token_hash: owner.discord_token_hash.clone(),
            channel_id: owner.channel_id.clone(),
            logical_key: owner.logical_key.clone(),
            owner_node: owner.owner_node.clone(),
            runtime_root: owner.runtime_root.clone(),
        }
    }
}

impl AbsenceStamp {
    pub(crate) fn capture(
        owner: &HostedOwner,
        expected: MarkerRowExpectation,
        front: QueueIdentity,
        epoch: String,
    ) -> Result<Self, String> {
        tc::try_with_tmux_source_authority(&owner.logical_key, |authority| {
            Self::capture_under_source(owner, expected, front, epoch, authority)
        })
        .ok_or("marker Source is busy")?
    }

    pub(super) fn capture_under_source(
        owner: &HostedOwner,
        expected: MarkerRowExpectation,
        front: QueueIdentity,
        epoch: String,
        authority: &TmuxSourceAuthority<'_>,
    ) -> Result<Self, String> {
        use crate::services::tui_prompt_dedupe::{self as dedupe, RuntimeObservation};
        let expected_owner = match &expected {
            MarkerRowExpectation::Missing { owner, .. }
            | MarkerRowExpectation::Legacy { owner, .. } => owner,
        };
        if expected_owner != &owner.into() {
            return Err("marker owner expectation differs".into());
        }
        if dedupe::runtime_observation_under_source_authority(authority)
            != RuntimeObservation::Missing
        {
            return Err("marker Source binding remains".into());
        }
        let logical = &owner.logical_key;
        let paths = [
            PathBuf::from(tc::session_temp_path(
                logical,
                tc::TMUX_RUNTIME_KIND_TEMP_EXT,
            )),
            PathBuf::from(tc::legacy_tmp_session_path(
                logical,
                tc::TMUX_RUNTIME_KIND_TEMP_EXT,
            )),
        ];
        let nonce = match dedupe::binding_context::observe_spawn_nonce_marker(logical) {
            dedupe::binding_context::SpawnNonceMarker::Known(nonce) => Some(nonce),
            dedupe::binding_context::SpawnNonceMarker::Absent => None,
            dedupe::binding_context::SpawnNonceMarker::Unreadable => {
                return Err("marker nonce unreadable".into());
            }
        };
        Ok(Self {
            owner: owner.clone(),
            expected,
            front,
            epoch,
            paths: [HintPath::observe(&paths[0])?, HintPath::observe(&paths[1])?],
            nonce: Ok(nonce),
            source: Ok(native_source(logical)?),
        })
    }

    pub(crate) fn from_control(control: &MarkerControl) -> Result<Self, String> {
        let owner = match &control.expected {
            MarkerRowExpectation::Missing { owner, .. }
            | MarkerRowExpectation::Legacy { owner, .. } => owner,
        };
        let paths: [MarkerPathSnapshot; 2] = control
            .paths
            .clone()
            .try_into()
            .map_err(|_| "marker path count differs")?;
        Ok(Self {
            owner: owner.into(),
            expected: control.expected.clone(),
            front: control.identity.clone(),
            epoch: control.epoch.clone(),
            paths: paths.map(|path| HintPath {
                path: path.path,
                parent: path.parent,
                file: match path.entry {
                    MarkerEntry::Absent => None,
                    MarkerEntry::CodexTui { file } => Some(file),
                },
            }),
            nonce: Ok(control.witness.nonce.clone()),
            source: Ok(control.witness.source.clone()),
        })
    }

    pub(crate) fn witness(&self) -> Result<MarkerWitness, String> {
        Ok(MarkerWitness {
            nonce: self.nonce.clone()?,
            source: self.source.clone()?,
        })
    }

    pub(crate) fn path_snapshots(&self) -> Vec<MarkerPathSnapshot> {
        self.paths.iter().map(HintPath::snapshot).collect()
    }

    pub(crate) fn matches(&self, current: &Self) -> bool {
        self.nonce.is_ok() && self.source.is_ok() && self == current
    }

    pub(crate) fn permits_partial(&self, current: &Self) -> bool {
        let mut expected = self.clone();
        for (old, new) in expected.paths.iter_mut().zip(&current.paths) {
            if old.path != new.path
                || old.parent != new.parent
                || (new.file.is_some() && old.file != new.file)
            {
                return false;
            }
            old.file = new.file;
        }
        expected.matches(current)
    }
}

fn native_source(logical: &str) -> Result<Option<NativeSource>, String> {
    let mut found = None;
    for path in [
        tc::session_temp_path(logical, tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT),
        tc::legacy_tmp_session_path(logical, tc::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT),
    ] {
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let value: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
        let text = |v: &serde_json::Value| {
            v.as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or("marker Source text missing")
        };
        let proof = &value["codex_ownership"];
        let path = PathBuf::from(text(&value["rollout_path"])?);
        let file = std::fs::metadata(&path).map_err(|error| error.to_string())?;
        if !file.is_file() {
            return Err("marker Source is not regular".into());
        }
        let identity = NativeSource {
            path,
            session: text(&value["session_id"])?,
            nonce: text(&proof["execution_nonce"])?,
            proof_seq: proof["proof_seq"]
                .as_u64()
                .ok_or("marker Source proof missing")?,
            declared_file: [
                proof["dev"].as_u64().ok_or("marker Source dev missing")?,
                proof["ino"].as_u64().ok_or("marker Source ino missing")?,
            ],
            file: [file.dev(), file.ino()],
        };
        if identity.declared_file != identity.file
            || found.as_ref().is_some_and(|old| old != &identity)
        {
            return Err("marker Source execution differs".into());
        }
        found = Some(identity);
    }
    Ok(found)
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
