use super::{ConfinedDir, ConfinedRuntimeRoot, DirectoryMutation, FsError};
use std::path::Path;

#[derive(Debug)]
pub(super) enum DirHandle {
    #[cfg(test)]
    Inhabited,
}

pub(super) fn unsupported_mutation(_: &ConfinedDir, _: &str) -> DirectoryMutation {
    DirectoryMutation::rejected(FsError::unsupported())
}

pub(in super::super) fn open_runtime_root(_: &Path) -> Result<ConfinedRuntimeRoot, FsError> {
    Err(FsError::unsupported())
}

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

        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let mut first = ConfinedRuntimeRoot::open(first_dir.path()).unwrap();
        let second = ConfinedRuntimeRoot::open(second_dir.path()).unwrap();
        let parent = second.directory.clone();
        super::super::take_validation_count();
        super::super::unix::take_mutation_trace();
        let result = first
            .unsupported_mutation_session()
            .open_or_create_child(&parent, "..");
        assert_eq!(result.error.unwrap().kind(), FsErrorKind::UnsupportedPlatform);
        assert_eq!(super::super::take_validation_count(), 0);
        assert!(super::super::unix::take_mutation_trace().is_empty());
    }
}
