//! Test seam: runs a probe inside the Pending PG lookup to prove no synchronous guard is held.
use std::cell::RefCell;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::services::{tmux_common, tui_prompt_dedupe};

const WAIT: Duration = Duration::from_secs(10);

struct LookupProbe {
    logical: String,
    before: Box<dyn FnOnce()>,
}

thread_local! {
    static LOOKUP_PROBE: RefCell<Option<LookupProbe>> = const { RefCell::new(None) };
}

struct ScopedProbe;

impl Drop for ScopedProbe {
    fn drop(&mut self) {
        LOOKUP_PROBE.with_borrow_mut(|slot| *slot = None);
    }
}

pub(super) fn before_pending_lookup(logical: &str) {
    let probe = LOOKUP_PROBE.with_borrow_mut(|slot| {
        if slot.as_ref().is_some_and(|probe| probe.logical == logical) {
            slot.take()
        } else {
            None
        }
    });
    if let Some(probe) = probe {
        (probe.before)();
    }
}

pub(crate) fn check_unlocked<T>(logical: &str, submit: impl FnOnce() -> T) -> T {
    let (reading, mut release) = UnixStream::pair().unwrap();
    reading.set_nonblocking(true).unwrap();
    let (in_lookup, entered) = mpsc::channel();
    let probe_logical = logical.to_owned();
    let observer = std::thread::spawn(move || {
        crate::test_support::wait::recv_within(&entered, WAIT);
        let order = tui_prompt_dedupe::herdr_listing_order(&probe_logical);
        let listing_unlocked = order.try_lock().is_ok();
        let source_unlocked =
            tmux_common::try_with_tmux_source_authority(&probe_logical, |_| ()).is_some();
        release.write_all(&[1]).unwrap();
        (listing_unlocked, source_unlocked)
    });
    let before = Box::new(move || {
        tokio::runtime::Handle::current().block_on(async move {
            let mut reading = tokio::net::UnixStream::from_std(reading).unwrap();
            let mut signal = [0];
            let mut read = std::pin::pin!(reading.read_exact(&mut signal));
            let is_pending = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(read.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(is_pending, "lookup I/O must wait for the lock observer");
            in_lookup.send(()).unwrap();
            crate::test_support::wait::await_within(read, WAIT)
                .await
                .unwrap();
        });
    });
    LOOKUP_PROBE.with_borrow_mut(|slot| {
        assert!(slot.is_none(), "a lookup probe is already installed");
        *slot = Some(LookupProbe {
            logical: logical.to_owned(),
            before,
        });
    });
    let scope = ScopedProbe;
    let result = submit();
    drop(scope);
    let (listing_unlocked, source_unlocked) =
        crate::test_support::wait::join_within(observer, WAIT);
    assert!(
        listing_unlocked && source_unlocked,
        "Pending PG lookup held a synchronous guard: listing unlocked={listing_unlocked}, source unlocked={source_unlocked}"
    );
    result
}
