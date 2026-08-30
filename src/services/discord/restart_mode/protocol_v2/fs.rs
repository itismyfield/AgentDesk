use std::{ffi::CString, path::Path, sync::Arc};

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
    seal: Arc<LineageSeal>,
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

    fn mutation_session(&mut self) -> MutationSession<'_> {
        MutationSession {
            root: self,
            #[cfg(test)]
            unsupported: false,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ConfinedDir {
    fd: Arc<platform::DirHandle>,
    open: OpenDirectoryFact,
    seal: Arc<LineageSeal>,
    locator: DirectoryLocator,
}

#[derive(Debug)]
struct LineageSeal;

#[derive(Clone, Debug, PartialEq, Eq)]
enum DirectoryLocator {
    Anchor {
        component: CString,
    },
    Child {
        parent: DirectoryIdentity,
        component: CString,
    },
}

#[derive(Debug)]
struct PreparedChild {
    parent: ConfinedDir,
    locator: DirectoryLocator,
}

struct MutationSession<'root> {
    root: &'root mut ConfinedRuntimeRoot,
    #[cfg(test)]
    unsupported: bool,
}

impl MutationSession<'_> {
    fn prepare_child(
        &mut self,
        parent: &ConfinedDir,
        value: &str,
    ) -> Result<PreparedChild, FsError> {
        if !platform::MUTATION_SUPPORTED {
            return Err(FsError::unsupported());
        }
        #[cfg(test)]
        if self.unsupported {
            return Err(FsError::unsupported());
        }
        if !Arc::ptr_eq(&parent.seal, &self.root.seal) {
            return Err(FsError::cross_lineage());
        }
        let component = validate_component(value)?;
        Ok(PreparedChild {
            parent: parent.clone(),
            locator: DirectoryLocator::Child {
                parent: parent.open.identity,
                component,
            },
        })
    }
}

fn validate_component(value: &str) -> Result<CString, FsError> {
    let invalid = match value {
        "" => Some(InvalidComponentFact::Empty),
        "." => Some(InvalidComponentFact::Current),
        ".." => Some(InvalidComponentFact::Parent),
        _ if value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)) =>
        {
            None
        }
        _ => Some(InvalidComponentFact::Character),
    };
    invalid.map_or_else(
        || Ok(CString::new(value).expect("validated component")),
        |fact| Err(FsError::invalid_component(fact)),
    )
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
enum InvalidComponentFact {
    Empty,
    Current,
    Parent,
    Character,
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
    InvalidComponent(InvalidComponentFact),
    CrossLineage,
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

    fn invalid_component(fact: InvalidComponentFact) -> Self {
        Self {
            kind: FsErrorKind::InvalidComponent(fact),
        }
    }

    fn cross_lineage() -> Self {
        Self {
            kind: FsErrorKind::CrossLineage,
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
