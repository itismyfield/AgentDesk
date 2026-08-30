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
            backend: TestMutationBackend::Platform,
        }
    }

    #[cfg(test)]
    fn unsupported_mutation_session(&mut self) -> MutationSession<'_> {
        MutationSession {
            root: self,
            backend: TestMutationBackend::Unsupported,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct ConfinedDir {
    fd: Arc<platform::DirHandle>,
    open: OpenDirectoryFact,
    lineage: Arc<LineageSeal>,
    locator: DirectoryLocator,
}

#[derive(Debug)]
struct LineageSeal;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DirectoryLocator {
    parent: DirectoryIdentity,
    component: Arc<CString>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attempt<T> {
    NotAttempted,
    Succeeded(T),
    Failed(IoFact),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MutationFacts {
    locator: DirectoryLocator,
    requested_mode: u32,
    mkdir: Attempt<()>,
    parent_sync: Attempt<()>,
    observed: Attempt<OpenDirectoryFact>,
    open: Attempt<()>,
    fd_flags: Attempt<i32>,
    fstat: Attempt<OpenDirectoryFact>,
}

#[derive(Debug)]
struct DirectoryMutation {
    directory: Option<ConfinedDir>,
    facts: Option<MutationFacts>,
    error: Option<FsError>,
}

impl DirectoryMutation {
    fn rejected(error: FsError) -> Self {
        Self {
            directory: None,
            facts: None,
            error: Some(error),
        }
    }
}

struct MutationComponent(Arc<CString>);

impl MutationComponent {
    fn parse(value: &str) -> Result<Self, FsError> {
        #[cfg(test)]
        VALIDATION_COUNT.with(|count| count.set(count.get() + 1));
        let bytes = value.as_bytes();
        let kind = if bytes.is_empty() {
            Some(InvalidComponentFact::Empty)
        } else if value == "." {
            Some(InvalidComponentFact::Current)
        } else if value == ".." {
            Some(InvalidComponentFact::Parent)
        } else if bytes
            .iter()
            .any(|byte| !byte.is_ascii_alphanumeric() && !b"._-".contains(byte))
        {
            Some(InvalidComponentFact::Character)
        } else {
            None
        };
        kind.map_or_else(
            || Ok(Self(Arc::new(CString::new(bytes).expect("validated component")))),
            |fact| Err(FsError::invalid_component(fact)),
        )
    }
}

struct MutationSession<'root> {
    root: &'root mut ConfinedRuntimeRoot,
    #[cfg(test)]
    backend: TestMutationBackend,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum TestMutationBackend {
    Platform,
    Unsupported,
}

impl MutationSession<'_> {
    fn root_dir(&self) -> ConfinedDir {
        self.root.directory.clone()
    }

    fn open_or_create_child(
        &mut self,
        parent: &ConfinedDir,
        component: &str,
    ) -> DirectoryMutation {
        #[cfg(test)]
        if matches!(self.backend, TestMutationBackend::Unsupported) {
            return unsupported::unsupported_mutation(parent, component);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            return platform::unsupported_mutation(parent, component);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if !Arc::ptr_eq(&parent.lineage, &self.root.seal) {
                return DirectoryMutation::rejected(FsError::cross_lineage());
            }
            let component = match MutationComponent::parse(component) {
                Ok(component) => component,
                Err(error) => return DirectoryMutation::rejected(error),
            };
            platform::open_or_create_child(parent, component)
        }
    }
}

#[cfg(test)]
thread_local! {
    static VALIDATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_validation_count() -> usize {
    VALIDATION_COUNT.with(|count| count.replace(0))
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
