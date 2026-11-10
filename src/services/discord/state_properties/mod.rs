//! Model-based and property tests for AgentDesk's stateful paths.
//!
//! Compiled under `cfg(test)` and under the `state_properties` feature so the
//! suite can run without also monomorphizing every other unit test.

mod campaign;
mod db_access;
mod delivery_lease;
mod kanban;
mod mailbox;
mod queue_prefix;
mod session_host;
mod turn_lifecycle;

use std::sync::atomic::{AtomicU64, Ordering};

fn next_id(counter: &AtomicU64, start: u64) -> u64 {
    let id = counter.fetch_add(1, Ordering::Relaxed);
    if id == 0 { start } else { id }
}

fn fresh_channel() -> u64 {
    static CHANNELS: AtomicU64 = AtomicU64::new(9_100_000_001);
    next_id(&CHANNELS, 9_100_000_001)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
}

/// Point queue and inflight files at a private root for one property case.
struct IsolatedRoot {
    _dir: tempfile::TempDir,
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl IsolatedRoot {
    fn acquire() -> Self {
        let dir = tempfile::tempdir().expect("state property runtime root");
        let key = "AGENTDESK_ROOT_DIR";
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, dir.path()) };
        Self {
            _dir: dir,
            key,
            previous,
        }
    }
}

impl Drop for IsolatedRoot {
    fn drop(&mut self) {
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

pub(super) fn note_panic(name: &str, payload: Box<dyn std::any::Any + Send>) -> String {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic");
    format!("{name}: {message}")
}

pub(super) fn exercise(name: &str, test: fn()) -> Option<String> {
    std::panic::catch_unwind(test)
        .err()
        .map(|payload| note_panic(name, payload))
}

pub fn run_all() {
    let mut failures = Vec::new();
    failures.extend(delivery_lease::run());
    failures.extend(turn_lifecycle::run());
    failures.extend(mailbox::run());
    failures.extend(kanban::run());
    failures.extend(campaign::run());
    failures.extend(session_host::run());
    failures.extend(queue_prefix::run());
    failures.extend(runtime().block_on(db_access::run()));
    if !failures.is_empty() {
        panic!(
            "{} state invariant failure(s):\n{}",
            failures.len(),
            failures.join("\n---\n")
        );
    }
}
