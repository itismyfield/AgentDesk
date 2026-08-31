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

    fn open_regular(&self, parent: &ConfinedDir, value: &str) -> Result<RegularFile, FsError> {
        let locator = prepare_locator(
            platform::REGULAR_READ_SUPPORTED,
            &self.directory.seal,
            (&parent.seal, parent.open.identity),
            value,
        )?;
        platform::open_regular(PreparedRegular(parent.clone(), locator))
    }
}

#[derive(Debug)]
struct RegularFile {
    fd: platform::FileHandle,
    open: OpenDirectoryFact,
    locator: DirectoryLocator,
}

#[derive(Debug, PartialEq, Eq)]
enum BoundedRead {
    Complete(Vec<u8>),
    Oversize,
}

impl RegularFile {
    fn read_bounded(self, limit: usize) -> Result<BoundedRead, FsError> {
        platform::read_bounded(self.fd, limit)
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

#[derive(Debug)]
struct PreparedRegular(ConfinedDir, DirectoryLocator);

#[derive(Debug)]
struct PreparedStage(ConfinedDir, DirectoryLocator);
#[rustfmt::skip] #[derive(Debug)] struct PreparedSeal { parent: ConfinedDir, locator: DirectoryLocator, writer: platform::FileHandle, writer_open: OpenDirectoryFact }
#[rustfmt::skip] #[derive(Debug)] enum PlatformStageCreation { Writer(platform::FileHandle, OpenDirectoryFact), Collision(platform::FileHandle, OpenDirectoryFact) }
#[derive(Debug)]
struct PinnedStage(platform::FileHandle, OpenDirectoryFact);
#[rustfmt::skip] pub(super) enum StageCreation<'session, 'root> { Writer(StageWriter<'session, 'root>), Collision(CollisionStageToken<'session, 'root>) }
#[rustfmt::skip] pub(super) struct StageWriter<'session, 'root> { session: &'session mut MutationSession<'root>, parent: ConfinedDir, locator: DirectoryLocator, writer: platform::FileHandle, writer_open: OpenDirectoryFact }
pub(super) struct SyncedStage<'session, 'root> {
    token: StageToken<'session, 'root>,
}
#[rustfmt::skip] struct StageToken<'session, 'root> { session: &'session mut MutationSession<'root>, parent: ConfinedDir, locator: DirectoryLocator, sealed_fd: platform::FileHandle, sealed: OpenDirectoryFact, target: Option<BoundTarget> }
#[rustfmt::skip] struct BoundTarget { parent: ConfinedDir, locator: DirectoryLocator }
#[rustfmt::skip] pub(super) enum LinkOutcome<'session, 'root> { Durable(DurableLinkedStage<'session, 'root>), Collision(CollisionStageToken<'session, 'root>), Prelink(PrelinkStageToken<'session, 'root>), Indeterminate(LinkOutcomeIndeterminate<'session, 'root>) }
#[rustfmt::skip] pub(super) struct DurableLinkedStage<'session, 'root> { token: StageToken<'session, 'root>, facts: SealedLinkFacts, evidence: SealedLinkDisposition }
#[rustfmt::skip] pub(super) struct CollisionStageToken<'session, 'root> { token: StageToken<'session, 'root>, facts: Option<SealedLinkFacts> }
#[rustfmt::skip] pub(super) struct PrelinkStageToken<'session, 'root> { token: StageToken<'session, 'root>, error: FsError }
#[rustfmt::skip] pub(super) struct LinkOutcomeIndeterminate<'session, 'root> { token: StageToken<'session, 'root>, facts: SealedLinkFacts }
#[rustfmt::skip] #[derive(Clone, Copy, Debug, PartialEq, Eq)] struct SealedLinkFacts { sealed: DirectoryIdentity, link: Attempt<()>, target: Attempt<Option<OpenDirectoryFact>>, parent_sync: Attempt<()> }
#[rustfmt::skip] #[derive(Clone, Copy, Debug, PartialEq, Eq)] enum SealedLinkDisposition { LinkedNormally, ObservedAfterReportedError, CleanupEligible, Indeterminate }
#[rustfmt::skip] #[derive(Clone, Debug, PartialEq, Eq)] pub(super) struct CleanupFacts { unlink: Attempt<()>, observe: Attempt<Option<OpenDirectoryFact>>, parent_sync: Attempt<()> }
#[rustfmt::skip] #[derive(Clone, Debug, PartialEq, Eq)] pub(super) enum StageCleanup { Rejected(FsError), Attempted(CleanupFacts) }
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MaintenanceCleanup(StageCleanup);

struct PostVerificationCleanupGrant(());

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
    fn rejected(error: FsError) -> Self {
        Self::Rejected(error)
    }
}

struct MutationSession<'root>(&'root mut ConfinedRuntimeRoot);

impl<'root> MutationSession<'root> {
    fn open_or_create_child(&mut self, parent: &ConfinedDir, value: &str) -> DirectoryMutation {
        let prepared = match self.prepare_child(parent, value) {
            Ok(prepared) => prepared,
            Err(error) => return DirectoryMutation::rejected(error),
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

    fn create_stage<'session>(
        &'session mut self,
        parent: &ConfinedDir,
        value: &str,
    ) -> Result<StageCreation<'session, 'root>, FsError> {
        let locator = prepare_locator(
            platform::SEALED_STAGE_SUPPORTED,
            &self.0.directory.seal,
            (&parent.seal, parent.open.identity),
            value,
        )?;
        let created = platform::create_stage(PreparedStage(parent.clone(), locator.clone()))?;
        Ok(match created {
            PlatformStageCreation::Writer(writer, writer_open) => {
                StageCreation::Writer(StageWriter {
                    session: self,
                    parent: parent.clone(),
                    locator,
                    writer,
                    writer_open,
                })
            }
            PlatformStageCreation::Collision(sealed_fd, sealed) => {
                StageCreation::Collision(CollisionStageToken {
                    token: StageToken {
                        session: self,
                        parent: parent.clone(),
                        locator,
                        sealed_fd,
                        sealed,
                        target: None,
                    },
                    facts: None,
                })
            }
        })
    }
}

impl<'session, 'root> StageWriter<'session, 'root> {
    fn seal(self, bytes: &[u8]) -> Result<SyncedStage<'session, 'root>, FsError> {
        let Self {
            session,
            parent,
            locator,
            writer,
            writer_open,
        } = self;
        let PinnedStage(sealed_fd, sealed) = platform::seal_stage(
            PreparedSeal {
                parent: parent.clone(),
                locator: locator.clone(),
                writer,
                writer_open,
            },
            bytes,
        )?;
        Ok(SyncedStage {
            token: StageToken {
                session,
                parent,
                locator,
                sealed_fd,
                sealed,
                target: None,
            },
        })
    }
}

impl<'session, 'root> SyncedStage<'session, 'root> {
    fn link(self, target_parent: &ConfinedDir, value: &str) -> LinkOutcome<'session, 'root> {
        let mut token = self.token;
        let target_locator = match prepare_locator(
            platform::SEALED_STAGE_SUPPORTED,
            &token.session.0.directory.seal,
            (&target_parent.seal, target_parent.open.identity),
            value,
        ) {
            Ok(locator) => locator,
            Err(error) => {
                return LinkOutcome::Prelink(PrelinkStageToken { token, error });
            }
        };
        if token.parent.open.identity == target_parent.open.identity {
            let error =
                FsError::directory_alias(token.parent.open.identity, target_parent.open.identity);
            return LinkOutcome::Prelink(PrelinkStageToken { token, error });
        }
        token.target = Some(BoundTarget {
            parent: target_parent.clone(),
            locator: target_locator,
        });
        let target = token.target.as_ref().expect("bound target");
        let facts = match platform::link_stage(
            &token.parent,
            &token.locator,
            &target.parent,
            &target.locator,
            token.sealed.identity,
        ) {
            Ok(facts) => facts,
            Err(error) => {
                return LinkOutcome::Prelink(PrelinkStageToken { token, error });
            }
        };
        match reduce_sealed_link(facts) {
            evidence @ (SealedLinkDisposition::LinkedNormally
            | SealedLinkDisposition::ObservedAfterReportedError) => {
                LinkOutcome::Durable(DurableLinkedStage {
                    token,
                    facts,
                    evidence,
                })
            }
            SealedLinkDisposition::CleanupEligible => LinkOutcome::Collision(CollisionStageToken {
                token,
                facts: Some(facts),
            }),
            SealedLinkDisposition::Indeterminate => {
                LinkOutcome::Indeterminate(LinkOutcomeIndeterminate { token, facts })
            }
        }
    }
}

impl CollisionStageToken<'_, '_> {
    fn cleanup(self) -> StageCleanup {
        cleanup_token(self.token)
    }
}

impl PrelinkStageToken<'_, '_> {
    fn cleanup(self) -> StageCleanup {
        cleanup_token(self.token)
    }
}

impl DurableLinkedStage<'_, '_> {
    fn cleanup(self, _: PostVerificationCleanupGrant) -> MaintenanceCleanup {
        MaintenanceCleanup(cleanup_token(self.token))
    }
}

fn reduce_sealed_link(facts: SealedLinkFacts) -> SealedLinkDisposition {
    match (facts.link, facts.target, facts.parent_sync) {
        (Attempt::Succeeded(()), Attempt::Succeeded(Some(target)), Attempt::Succeeded(()))
            if target.identity == facts.sealed =>
        {
            SealedLinkDisposition::LinkedNormally
        }
        (Attempt::Failed(_), Attempt::Succeeded(Some(target)), Attempt::Succeeded(()))
            if target.identity == facts.sealed =>
        {
            SealedLinkDisposition::ObservedAfterReportedError
        }
        (Attempt::Failed(_), Attempt::Succeeded(Some(target)), sync)
            if target.identity != facts.sealed && !matches!(sync, Attempt::NotAttempted) =>
        {
            SealedLinkDisposition::CleanupEligible
        }
        _ => SealedLinkDisposition::Indeterminate,
    }
}

fn cleanup_token(token: StageToken<'_, '_>) -> StageCleanup {
    if !Arc::ptr_eq(&token.session.0.directory.seal, &token.parent.seal) {
        return StageCleanup::Rejected(FsError::cross_lineage());
    }
    let DirectoryLocator::Child { parent, .. } = &token.locator else {
        unreachable!("stage token must name a child")
    };
    if *parent != token.parent.open.identity {
        return StageCleanup::Rejected(FsError::identity_mismatch(
            *parent,
            token.parent.open.identity,
        ));
    }
    if let Some(target) = &token.target
        && target.parent.open.identity == token.parent.open.identity
    {
        return StageCleanup::Rejected(FsError::directory_alias(
            token.parent.open.identity,
            target.parent.open.identity,
        ));
    }
    platform::cleanup_stage(&token.parent, &token.locator, token.sealed)
}

#[cfg(test)]
fn post_verification_cleanup_grant() -> PostVerificationCleanupGrant {
    PostVerificationCleanupGrant(())
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
    Read,
    Write,
    SyncFile,
    Link,
    Unlink,
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
    NotRegular(DirectoryTypeFact),
    MissingCloseOnExec {
        fd_flags: i32,
    },
    IdentityMismatch {
        observed: DirectoryIdentity,
        opened: DirectoryIdentity,
    },
    DirectoryAlias {
        source: DirectoryIdentity,
        target: DirectoryIdentity,
    },
    ReadLimitOverflow {
        limit: usize,
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

    fn not_regular(mode: u32) -> Self {
        Self {
            kind: FsErrorKind::NotRegular(DirectoryTypeFact { mode }),
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

    fn directory_alias(source: DirectoryIdentity, target: DirectoryIdentity) -> Self {
        Self {
            kind: FsErrorKind::DirectoryAlias { source, target },
        }
    }

    fn read_limit_overflow(limit: usize) -> Self {
        Self {
            kind: FsErrorKind::ReadLimitOverflow { limit },
        }
    }

    fn unsupported() -> Self {
        Self {
            kind: FsErrorKind::UnsupportedPlatform,
        }
    }
}
