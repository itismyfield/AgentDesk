use super::{
    BoundedRead, ConfinedDir, ConfinedRuntimeRoot, DirectoryIdentity, DirectoryLocator,
    DirectoryMutation, FsError, PinnedStage, PlatformStageCreation, PreparedChild, PreparedRegular,
    PreparedSeal, PreparedStage, RegularFile, SealedLinkFacts, StageCleanup,
};
use std::path::Path;

#[derive(Debug)]
pub(super) enum DirHandle {}
pub(super) type FileHandle = ();
pub(super) const MUTATION_SUPPORTED: bool = false;
pub(super) const REGULAR_READ_SUPPORTED: bool = false;
pub(super) const SEALED_STAGE_SUPPORTED: bool = false;

pub(in super::super) fn open_runtime_root(_: &Path) -> Result<ConfinedRuntimeRoot, FsError> {
    Err(FsError::unsupported())
}
pub(super) fn open_or_create_child(_: PreparedChild) -> DirectoryMutation {
    DirectoryMutation::rejected(FsError::unsupported())
}
pub(super) fn open_regular(_: PreparedRegular) -> Result<RegularFile, FsError> {
    Err(FsError::unsupported())
}
pub(super) fn read_bounded(_: FileHandle, _: usize) -> Result<BoundedRead, FsError> {
    Err(FsError::unsupported())
}
pub(super) fn create_stage(_: PreparedStage) -> Result<PlatformStageCreation, FsError> {
    #[cfg(test)]
    UNSUPPORTED_STAGE_CALLERS.with(|callers| callers.set(callers.get() + 1));
    Err(FsError::unsupported())
}
pub(super) fn seal_stage(_: PreparedSeal, _: &[u8]) -> Result<PinnedStage, FsError> {
    #[cfg(test)]
    UNSUPPORTED_STAGE_CALLERS.with(|callers| callers.set(callers.get() + 1));
    Err(FsError::unsupported())
}
pub(super) fn link_stage(
    _: &ConfinedDir,
    _: &DirectoryLocator,
    _: &ConfinedDir,
    _: &DirectoryLocator,
    _: DirectoryIdentity,
) -> Result<SealedLinkFacts, FsError> {
    #[cfg(test)]
    UNSUPPORTED_STAGE_CALLERS.with(|callers| callers.set(callers.get() + 1));
    Err(FsError::unsupported())
}
pub(super) fn cleanup_stage(
    _: &ConfinedDir,
    _: &DirectoryLocator,
    _: super::OpenDirectoryFact,
) -> StageCleanup {
    #[cfg(test)]
    UNSUPPORTED_STAGE_CALLERS.with(|callers| callers.set(callers.get() + 1));
    StageCleanup::Rejected(FsError::unsupported())
}

#[cfg(test)]
thread_local! { static UNSUPPORTED_STAGE_CALLERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod high_risk_recovery {
    use super::*;
    use crate::services::discord::restart_mode::protocol_v2::fs::FsErrorKind;

    #[test]
    fn unsupported_precedes_validation_for_every_facade_operation() {
        let facades = [open_runtime_root as fn(&Path) -> Result<ConfinedRuntimeRoot, FsError>];
        for facade in facades {
            for invalid in [Path::new(""), Path::new(".."), Path::new("child/../escape")] {
                assert_eq!(
                    facade(invalid).unwrap_err().kind(),
                    FsErrorKind::UnsupportedPlatform
                );
            }
        }

        let root = std::sync::Arc::new(super::super::LineageSeal);
        let foreign = std::sync::Arc::new(super::super::LineageSeal);
        let identity = super::super::DirectoryIdentity {
            device: 0,
            inode: 0,
        };
        super::super::take_preflight_activity();
        UNSUPPORTED_STAGE_CALLERS.with(|callers| callers.set(0));
        let error =
            super::super::prepare_locator(MUTATION_SUPPORTED, &root, (&foreign, identity), "..")
                .unwrap_err();
        assert!(matches!(
            DirectoryMutation::rejected(error),
            DirectoryMutation::Rejected(error) if error.kind() == FsErrorKind::UnsupportedPlatform
        ));
        assert_eq!(
            super::super::prepare_locator(
                REGULAR_READ_SUPPORTED,
                &root,
                (&foreign, identity),
                ".."
            )
            .unwrap_err()
            .kind(),
            FsErrorKind::UnsupportedPlatform
        );
        assert_eq!(
            super::super::prepare_locator(
                SEALED_STAGE_SUPPORTED,
                &root,
                (&foreign, identity),
                ".."
            )
            .unwrap_err()
            .kind(),
            FsErrorKind::UnsupportedPlatform
        );
        assert_eq!(
            read_bounded((), usize::MAX).unwrap_err().kind(),
            FsErrorKind::UnsupportedPlatform
        );
        assert_eq!(UNSUPPORTED_STAGE_CALLERS.with(std::cell::Cell::get), 0);
        assert_eq!(super::super::take_preflight_activity(), 0);
    }
}
