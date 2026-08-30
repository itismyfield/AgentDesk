use super::{ConfinedRuntimeRoot, FsError};
use std::path::Path;

#[derive(Debug)]
pub(super) enum DirHandle {
    #[cfg(test)]
    Inhabited,
}
pub(super) const MUTATION_SUPPORTED: bool = false;

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

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let (first_dir, second_dir) =
                (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let mut first = ConfinedRuntimeRoot::open(first_dir.path()).unwrap();
            let second = ConfinedRuntimeRoot::open(second_dir.path()).unwrap();
            let mut session = first.mutation_session();
            session.unsupported = true;
            let error = session.prepare_child(&second.directory, "..").unwrap_err();
            assert_eq!(error.kind(), FsErrorKind::UnsupportedPlatform);
        }
    }
}
