use super::*;
use std::cell::RefCell;

thread_local! {
    static FAILURE: RefCell<Option<(PathBuf, Stage, i32)>> = const { RefCell::new(None) };
}

pub(crate) struct FaultGuard;

impl FaultGuard {
    pub(crate) fn new(path: &Path, stage: Stage, errno: i32) -> Self {
        FAILURE.with(|failure| {
            assert!(
                failure
                    .replace(Some((path.to_path_buf(), stage, errno)))
                    .is_none()
            );
        });
        Self
    }
}

impl Drop for FaultGuard {
    fn drop(&mut self) {
        FAILURE.with(|failure| failure.replace(None));
    }
}

pub(super) fn inject(path: &Path, stage: Stage) -> io::Result<()> {
    FAILURE.with(|failure| match &*failure.borrow() {
        Some((target, failed, errno)) if path == target && stage == *failed => {
            Err(io::Error::from_raw_os_error(*errno))
        }
        _ => Ok(()),
    })
}

#[test]
fn preparation_and_failed_stages_keep_original_bytes_and_raw_errno() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, "old").unwrap();
    for stage in [
        Stage::CreateDir,
        Stage::CreateTemp,
        Stage::Write,
        Stage::FileSync,
        Stage::Rename,
    ] {
        let fault = FaultGuard::new(&path, stage, 28);
        let error = match prepare(&path, "new") {
            Err(error) => error,
            Ok(prepared) => prepared.publish().unwrap_err(),
        };
        assert_eq!(error.stage, stage);
        assert_eq!(error.error.raw_os_error(), Some(28));
        assert_eq!(fs::read_to_string(&path).unwrap(), "old");
        drop(fault);
    }
    let prepared = prepare(&path, "new").unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "old");
    prepared.publish().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
}

#[test]
fn real_rename_fault_is_at_the_adapter_syscall() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    fs::write(&path, "old").unwrap();
    let fault = rename_fault::Armed::new(&path);
    let error = prepare(&path, "new").unwrap().publish().unwrap_err();
    assert_eq!(error.stage, Stage::Rename);
    assert!(error.error.raw_os_error().is_some());
    assert_eq!(fault.evidence(), (1, vec![error.error.to_string()]));
    assert_eq!(fs::read_to_string(&path).unwrap(), "old");
}
