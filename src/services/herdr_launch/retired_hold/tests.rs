use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use crate::services::herdr_launch::retired_hold::{RetiredHoldOutcome, release};

#[derive(Default)]
struct SyncState {
    fail: bool,
    calls: usize,
}

static SYNCS: LazyLock<Mutex<HashMap<PathBuf, SyncState>>> = LazyLock::new(Mutex::default);

pub(crate) struct SyncFault(PathBuf);

impl SyncFault {
    pub(crate) fn new(path: &Path) -> Self {
        assert!(
            SYNCS
                .lock()
                .unwrap()
                .insert(path.into(), SyncState::default())
                .is_none()
        );
        Self(path.into())
    }

    pub(crate) fn fail(&self, fail: bool) {
        SYNCS.lock().unwrap().get_mut(&self.0).unwrap().fail = fail;
    }

    pub(crate) fn calls(&self) -> usize {
        SYNCS.lock().unwrap().get(&self.0).unwrap().calls
    }
}

impl Drop for SyncFault {
    fn drop(&mut self) {
        SYNCS.lock().unwrap().remove(&self.0);
    }
}

pub(crate) fn before_sync(path: &Path) -> io::Result<()> {
    if let Some(state) = SYNCS.lock().unwrap().get_mut(path) {
        state.calls += 1;
        if state.fail {
            return Err(io::Error::other(
                "injected retired hold directory fsync failure",
            ));
        }
    }
    Ok(())
}

pub(crate) fn hold_path(nonce: &str) -> PathBuf {
    crate::config::runtime_root()
        .unwrap()
        .join("runtime/herdr_input_holds")
        .join(nonce)
}

#[test]
fn retired_hold_absent_still_requires_directory_fsync() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let path = hold_path("absent-n1");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let fault = SyncFault::new(&path);
    fault.fail(true);
    assert!(
        matches!(release("absent-n1"), RetiredHoldOutcome::NotDurable(_)),
        "an absent hold does not prove its unlink durable"
    );
    assert_eq!(fault.calls(), 1);
    fault.fail(false);
    if cfg!(windows) {
        assert!(matches!(
            release("absent-n1"),
            RetiredHoldOutcome::NotDurable(_)
        ));
    } else {
        assert_eq!(release("absent-n1"), RetiredHoldOutcome::Durable);
    }
    assert_eq!(fault.calls(), 2);
}

#[test]
fn retired_hold_unlinked_but_unsynced_is_retryable() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let path = hold_path("unlinked-n1");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "N1 hold").unwrap();
    let unrelated = path.with_file_name("n2");
    std::fs::write(&unrelated, "N2 hold").unwrap();
    let fault = SyncFault::new(&path);
    fault.fail(true);
    assert!(matches!(
        release("unlinked-n1"),
        RetiredHoldOutcome::NotDurable(_)
    ));
    assert!(!path.exists());
    assert_eq!(fault.calls(), 1);
    fault.fail(false);
    if cfg!(windows) {
        assert!(matches!(
            release("unlinked-n1"),
            RetiredHoldOutcome::NotDurable(_)
        ));
    } else {
        assert_eq!(release("unlinked-n1"), RetiredHoldOutcome::Durable);
    }
    assert_eq!(
        fault.calls(),
        2,
        "retry must sync even after unlink succeeded"
    );
    assert_eq!(std::fs::read_to_string(unrelated).unwrap(), "N2 hold");
}

#[test]
fn retired_hold_missing_directory_is_confirmed_absent_and_bad_nonce_is_kept() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    assert_eq!(release("initial-n1"), RetiredHoldOutcome::Durable);
    assert!(matches!(release("../n2"), RetiredHoldOutcome::Failed(_)));
}

#[cfg(unix)]
#[test]
fn retired_hold_metadata_error_is_preserved_without_cleanup() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let path = hold_path("unreadable-n1");
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("herdr_input_holds", dir).unwrap();
    assert!(std::fs::symlink_metadata(&path).is_err());
    let fault = SyncFault::new(&path);
    assert!(matches!(
        release("unreadable-n1"),
        RetiredHoldOutcome::Failed(_)
    ));
    assert_eq!(fault.calls(), 0);
    assert!(std::fs::symlink_metadata(dir).unwrap().is_symlink());
}

#[cfg(unix)]
#[test]
fn retired_hold_dangling_directory_is_not_confirmed_absent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let path = hold_path("dangling-n1");
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("missing-holds", dir).unwrap();
    let fault = SyncFault::new(&path);
    assert!(matches!(
        release("dangling-n1"),
        RetiredHoldOutcome::NotDurable(_)
    ));
    assert_eq!(fault.calls(), 1);
    assert!(std::fs::symlink_metadata(dir).unwrap().is_symlink());
}

#[cfg(unix)]
#[test]
fn retired_hold_permission_error_keeps_the_hold() {
    use std::os::unix::fs::PermissionsExt;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let path = hold_path("permission-n1");
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(&path, "N1 hold").unwrap();
    let fault = SyncFault::new(&path);
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0)).unwrap();
    let result = release("permission-n1");
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(result, RetiredHoldOutcome::Failed(_)),
        "{result:?}"
    );
    assert_eq!(fault.calls(), 0);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "N1 hold");
}
