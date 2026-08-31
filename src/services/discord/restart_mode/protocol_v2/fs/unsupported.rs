use super::{
    BoundedRead, ConfinedDir, ConfinedRuntimeRoot, DirectoryIdentity, DirectoryLocator,
    DirectoryMutation, FsError, PinnedStage, PlatformStageCreation, PreparedChild, PreparedRegular,
    PreparedSeal, PreparedStage, RegularFile, SealedLinkFacts, StageCleanup,
};
use std::path::Path;

#[cfg(not(test))]
#[derive(Debug)]
pub(super) enum DirHandle {}
#[cfg(test)]
pub(super) type DirHandle = ();
pub(super) type FileHandle = ();
pub(super) type MutationLock = ();
pub(super) const MUTATION_SUPPORTED: bool = false;
pub(super) const REGULAR_READ_SUPPORTED: bool = false;
pub(super) const SEALED_STAGE_SUPPORTED: bool = false;

pub(in super::super) fn open_runtime_root(_: &Path) -> Result<ConfinedRuntimeRoot, FsError> {
    Err(FsError::unsupported())
}
pub(super) fn lock_mutation(_: &ConfinedRuntimeRoot) -> Result<MutationLock, FsError> {
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
    Err(FsError::unsupported())
}
pub(super) fn seal_stage(_: PreparedSeal, _: &[u8]) -> Result<PinnedStage, FsError> {
    Err(FsError::unsupported())
}
pub(super) fn link_stage(
    _: &ConfinedDir,
    _: &DirectoryLocator,
    _: &ConfinedDir,
    _: &DirectoryLocator,
    _: DirectoryIdentity,
) -> Result<SealedLinkFacts, FsError> {
    Err(FsError::unsupported())
}
pub(super) fn cleanup_stage(
    _: &ConfinedDir,
    _: &DirectoryLocator,
    _: super::OpenDirectoryFact,
) -> StageCleanup {
    StageCleanup::Rejected(FsError::unsupported())
}

#[cfg(test)]
mod high_risk_recovery {
    use super::*;
    use crate::services::discord::restart_mode::protocol_v2::fs::FsErrorKind;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn dummy_dir_handle() -> super::super::platform::DirHandle {
        std::fs::File::open(".").unwrap().into()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn dummy_dir_handle() -> super::super::platform::DirHandle {}
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn dummy_file_handle() -> super::super::platform::FileHandle {
        std::fs::File::open("/dev/null").unwrap().into()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn dummy_file_handle() -> super::super::platform::FileHandle {}

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
        let locator = DirectoryLocator::Child {
            parent: identity,
            component: std::ffi::CString::new("..").unwrap(),
        };
        let open = super::super::OpenDirectoryFact {
            identity,
            file_type: super::super::DirectoryTypeFact { mode: 0 },
        };
        let dir = ConfinedDir {
            fd: std::sync::Arc::new(dummy_dir_handle()),
            open,
            seal: root.clone(),
            locator: locator.clone(),
        };
        assert_eq!(
            create_stage(PreparedStage(dir.clone(), locator.clone()))
                .unwrap_err()
                .kind(),
            FsErrorKind::UnsupportedPlatform
        );
        assert_eq!(
            seal_stage(
                PreparedSeal {
                    parent: dir.clone(),
                    locator: locator.clone(),
                    writer: dummy_file_handle(),
                    writer_open: open
                },
                b"x"
            )
            .unwrap_err()
            .kind(),
            FsErrorKind::UnsupportedPlatform
        );
        assert_eq!(
            link_stage(&dir, &locator, &dir, &locator, identity)
                .unwrap_err()
                .kind(),
            FsErrorKind::UnsupportedPlatform
        );
        assert!(
            matches!(cleanup_stage(&dir, &locator, open), StageCleanup::Rejected(error) if error.kind() == FsErrorKind::UnsupportedPlatform)
        );
        let runtime = ConfinedRuntimeRoot {
            lineage: super::super::RootLineage {
                anchor: identity,
                root: identity,
            },
            directory: dir,
        };
        assert_eq!(
            lock_mutation(&runtime).unwrap_err().kind(),
            FsErrorKind::UnsupportedPlatform
        );
        assert_eq!(super::super::take_preflight_activity(), 0);
    }
}
