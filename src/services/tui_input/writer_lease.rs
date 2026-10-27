//! Physical targets an owner transaction holds right now. A content write from a thread without
//! that owner's scope fails closed; a target no owner holds keeps main's behavior.
use std::collections::BTreeMap;
use std::sync::{
    Mutex, PoisonError,
    atomic::{AtomicU64, Ordering},
};

use crate::services::claude_tui::owner_input;

static HELD: Mutex<BTreeMap<String, u64>> = Mutex::new(BTreeMap::new());

pub(crate) fn key(host: &str, physical: &str) -> String {
    format!("{host}:{physical}")
}

/// One owner's holds; dropping it releases every target it took.
pub(crate) struct Holder {
    id: u64,
    keys: Mutex<Vec<String>>,
}

impl Default for Holder {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            keys: Mutex::default(),
        }
    }
}

impl Holder {
    pub(crate) fn hold(&self, key: &str) -> Result<(), String> {
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        match held.get(key) {
            Some(id) if *id == self.id => return Ok(()),
            Some(_) => return Err(format!("{key} is held by another owner")),
            None => {}
        }
        held.insert(key.to_string(), self.id);
        let mut keys = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        keys.push(key.to_string());
        Ok(())
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
        let keys = self.keys.get_mut().unwrap_or_else(PoisonError::into_inner);
        for key in keys.drain(..) {
            if held.get(&key) == Some(&self.id) {
                held.remove(&key);
            }
        }
    }
}

/// Whether a content write to `physical` may run, and whether an owner scope runs it. An owner
/// writes only its own target; a thread without one is refused while any owner holds it.
pub(crate) fn admits(host: &str, physical: &str) -> Result<bool, String> {
    let key = key(host, physical);
    match owner_input::hold_content(host, physical, &key) {
        Some(held) => held.map(|()| true),
        None if HELD
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(&key) =>
        {
            Err(format!("{key} is held by an owner on another thread"))
        }
        None => Ok(false),
    }
}

/// One content write behind `admits`; an owner's write also passes its ready and start.
pub(crate) fn content<R>(
    host: &str,
    physical: &str,
    send: impl FnOnce() -> Result<R, String>,
) -> Result<R, String> {
    if admits(host, physical)? {
        owner_input::payload(send)
    } else {
        send()
    }
}

#[cfg(all(test, unix))]
#[path = "writer_lease_tests.rs"]
mod tests;
