//! One-shot fault at the actual reader boundary; never compiled in production.
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static FAULT: Mutex<Option<(PathBuf, u64)>> = Mutex::new(None);

pub(crate) struct ReadFault(PathBuf, u64);

pub(crate) fn after_offset(path: &Path, offset: u64) -> ReadFault {
    let mut fault = FAULT.lock().unwrap();
    assert!(fault.is_none(), "only one scoped reader fault may be armed");
    *fault = Some((path.to_owned(), offset));
    ReadFault(path.to_owned(), offset)
}

impl Drop for ReadFault {
    fn drop(&mut self) {
        let mut fault = FAULT.lock().unwrap();
        if fault.as_ref() == Some(&(self.0.clone(), self.1)) {
            *fault = None;
        }
    }
}

pub(super) fn read<T>(
    path: &str,
    offset: u64,
    read: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut fault = FAULT.lock().unwrap();
    if fault
        .as_ref()
        .is_some_and(|(expected, floor)| expected == Path::new(path) && offset >= *floor)
    {
        *fault = None;
        return Err(std::io::Error::other("scoped transcript read failure"));
    }
    drop(fault);
    read()
}
