use super::{File, Read, Take, io};
use std::cell::RefCell;

type AfterRead = Box<dyn FnOnce(&[u8]) -> io::Result<()>>;

thread_local! {
    static AFTER_READ: RefCell<Option<AfterRead>> = const { RefCell::new(None) };
}

pub(in crate::services::discord) fn after_next_read(
    inspect: impl FnOnce(&[u8]) -> io::Result<()> + 'static,
) {
    AFTER_READ.with(|hook| assert!(hook.replace(Some(Box::new(inspect))).is_none()));
}

pub(super) fn read(reader: &mut Take<&File>, bytes: &mut Vec<u8>) -> io::Result<usize> {
    let count = reader.read_to_end(bytes)?;
    if let Some(inspect) = AFTER_READ.with(RefCell::take) {
        inspect(bytes)?;
    }
    Ok(count)
}
