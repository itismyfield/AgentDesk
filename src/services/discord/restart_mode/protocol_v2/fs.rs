use std::path::Path;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
mod unsupported;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use unix as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use unsupported as platform;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    pub(super) fn device(self) -> u64 {
        self.device
    }

    pub(super) fn inode(self) -> u64 {
        self.inode
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RootLineage {
    anchor: DirectoryIdentity,
    root: DirectoryIdentity,
}

impl RootLineage {
    pub(super) fn anchor(self) -> DirectoryIdentity {
        self.anchor
    }

    pub(super) fn root(self) -> DirectoryIdentity {
        self.root
    }
}

#[derive(Debug)]
pub(super) struct ConfinedRuntimeRoot {
    lineage: RootLineage,
    directory: ConfinedDir,
}

impl ConfinedRuntimeRoot {
    pub(super) fn open(path: &Path) -> Result<Self, FsError> {
        platform::open_runtime_root(path)
    }

    pub(super) fn lineage(&self) -> RootLineage {
        self.lineage
    }

    pub(super) fn identity(&self) -> DirectoryIdentity {
        self.directory.open.identity
    }
}

#[derive(Debug)]
pub(super) struct ConfinedDir {
    fd: platform::DirHandle,
    open: OpenDirectoryFact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenDirectoryFact {
    identity: DirectoryIdentity,
    file_type: DirectoryTypeFact,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DirectoryTypeFact {
    mode: u32,
}

impl DirectoryTypeFact {
    pub(super) fn mode(self) -> u32 {
        self.mode
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IoFact {
    operation: FsOperation,
    raw_errno: i32,
}

impl IoFact {
    pub(super) fn operation(self) -> FsOperation {
        self.operation
    }

    pub(super) fn raw_errno(self) -> i32 {
        self.raw_errno
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FsOperation {
    Open,
    GetFdFlags,
    Fstat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InvalidRootFact {
    Empty,
    ParentDir,
    Nul,
    Prefix,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FsErrorKind {
    InvalidRoot(InvalidRootFact),
    Io(IoFact),
    NotDirectory(DirectoryTypeFact),
    MissingCloseOnExec { fd_flags: i32 },
    UnsupportedPlatform,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FsError {
    kind: FsErrorKind,
}

impl FsError {
    pub(super) fn kind(self) -> FsErrorKind {
        self.kind
    }

    fn invalid(fact: InvalidRootFact) -> Self {
        Self {
            kind: FsErrorKind::InvalidRoot(fact),
        }
    }

    fn io(operation: FsOperation, raw_errno: i32) -> Self {
        Self {
            kind: FsErrorKind::Io(IoFact {
                operation,
                raw_errno,
            }),
        }
    }

    fn not_directory(mode: u32) -> Self {
        Self {
            kind: FsErrorKind::NotDirectory(DirectoryTypeFact { mode }),
        }
    }

    fn missing_close_on_exec(fd_flags: i32) -> Self {
        Self {
            kind: FsErrorKind::MissingCloseOnExec { fd_flags },
        }
    }

    fn unsupported() -> Self {
        Self {
            kind: FsErrorKind::UnsupportedPlatform,
        }
    }
}

#[cfg(test)]
fn mutation_preflight_semantic_stub() -> bool {
    false
}
