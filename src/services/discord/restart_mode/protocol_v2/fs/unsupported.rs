use super::{ConfinedRuntimeRoot, FsError};
use std::path::Path;

#[derive(Debug)]
pub(super) enum DirHandle {}

pub(in super::super) fn open_runtime_root(_: &Path) -> Result<ConfinedRuntimeRoot, FsError> {
    Err(FsError::unsupported())
}

#[cfg(test)]
mod high_risk_recovery {
    use super::*;
    use crate::services::discord::restart_mode::protocol_v2::fs::FsErrorKind;

    #[test]
    fn unsupported_precedes_validation_for_every_facade_operation() {
        assert!(super::super::mutation_preflight_semantic_stub());
        let facades = [open_runtime_root as fn(&Path) -> Result<ConfinedRuntimeRoot, FsError>];
        for facade in facades {
            for invalid in [Path::new(""), Path::new(".."), Path::new("child/../escape")] {
                assert_eq!(
                    facade(invalid).unwrap_err().kind(),
                    FsErrorKind::UnsupportedPlatform
                );
            }
        }
    }
}
