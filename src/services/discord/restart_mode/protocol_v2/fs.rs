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
        MutationSession(self)
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
struct PreparedChild(ConfinedDir, DirectoryLocator);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempt<T> {
    NotAttempted,
    Succeeded(T),
    Failed(IoFact),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenAttemptFact {
    raw_fd: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MutationFacts {
    locator: DirectoryLocator,
    requested_mode: u32,
    mkdir: Attempt<()>,
    sync: Attempt<()>,
    observe: Attempt<OpenDirectoryFact>,
    open: Attempt<OpenAttemptFact>,
    fd_flags: Attempt<i32>,
    fstat: Attempt<OpenDirectoryFact>,
}

#[derive(Debug)]
enum DirectoryMutation {
    Rejected(FsError),
    Attempted(MutationFacts, Result<ConfinedDir, FsError>),
}

impl DirectoryMutation {
    fn preflight<T>(result: Result<T, FsError>) -> Result<T, Self> {
        result.map_err(Self::Rejected)
    }
}

struct MutationSession<'root>(&'root mut ConfinedRuntimeRoot);

impl MutationSession<'_> {
    fn open_or_create_child(&mut self, parent: &ConfinedDir, value: &str) -> DirectoryMutation {
        let prepared = match DirectoryMutation::preflight(self.prepare_child(parent, value)) {
            Ok(prepared) => prepared,
            Err(rejection) => return rejection,
        };
        platform::open_or_create_child(prepared)
    }

    fn prepare_child(
        &mut self,
        parent: &ConfinedDir,
        value: &str,
    ) -> Result<PreparedChild, FsError> {
        let locator = prepare_locator(
            platform::MUTATION_SUPPORTED,
            &self.0.directory.seal,
            (&parent.seal, parent.open.identity),
            value,
        )?;
        Ok(PreparedChild(parent.clone(), locator))
    }
}

fn prepare_locator(
    supported: bool,
    root: &Arc<LineageSeal>,
    parent: (&Arc<LineageSeal>, DirectoryIdentity),
    value: &str,
) -> Result<DirectoryLocator, FsError> {
    if !supported {
        return Err(FsError::unsupported());
    }
    if !Arc::ptr_eq(root, parent.0) {
        return Err(FsError::cross_lineage());
    }
    Ok(DirectoryLocator::Child {
        parent: parent.1,
        component: validate_component(value)?,
    })
}

fn validate_component(value: &str) -> Result<CString, FsError> {
    #[cfg(test)]
    PREFLIGHT_ACTIVITY.with(|activity| activity.set(activity.get() | 1));
    let fact = match value {
        "" => InvalidComponentFact::Empty,
        "." => InvalidComponentFact::Current,
        ".." => InvalidComponentFact::Parent,
        _ if value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte)) =>
        {
            #[cfg(test)]
            PREFLIGHT_ACTIVITY.with(|activity| activity.set(activity.get() | 2));
            return Ok(CString::new(value).expect("validated component"));
        }
        _ => InvalidComponentFact::Character,
    };
    Err(FsError::invalid_component(fact))
}

#[cfg(test)]
thread_local! { static PREFLIGHT_ACTIVITY: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
fn take_preflight_activity() -> u8 {
    PREFLIGHT_ACTIVITY.with(|activity| activity.replace(0))
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
    Mkdir,
    SyncParent,
    Observe,
    Open,
    GetFdFlags,
    Fstat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InvalidComponentFact {
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
    MissingCloseOnExec {
        fd_flags: i32,
    },
    IdentityMismatch {
        observed: DirectoryIdentity,
        opened: DirectoryIdentity,
    },
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

    fn identity_mismatch(observed: DirectoryIdentity, opened: DirectoryIdentity) -> Self {
        Self {
            kind: FsErrorKind::IdentityMismatch { observed, opened },
        }
    }

    fn unsupported() -> Self {
        Self {
            kind: FsErrorKind::UnsupportedPlatform,
        }
    }
}
