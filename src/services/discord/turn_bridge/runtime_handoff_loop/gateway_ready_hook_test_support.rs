thread_local! {
    static CONNECTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn connected() -> bool {
    CONNECTED.with(std::cell::Cell::get)
}

/// Restores the previous state when dropped.
pub(in crate::services::discord) struct Guard(bool);

pub(in crate::services::discord) fn connect() -> Guard {
    Guard(CONNECTED.with(|cell| cell.replace(true)))
}

impl Drop for Guard {
    fn drop(&mut self) {
        CONNECTED.with(|cell| cell.set(self.0));
    }
}
