use super::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

struct Delay {
    pid: u32,
    gate: Option<Arc<Gate>>,
    observed: Arc<AtomicBool>,
}
thread_local! { static DELAY: RefCell<Option<Delay>> = const { RefCell::new(None) }; }
pub(crate) struct Guard(Arc<AtomicBool>);
pub(crate) fn arm() -> Guard {
    let observed = Arc::new(AtomicBool::new(false));
    DELAY.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(Delay {
            pid: 0,
            gate: None,
            observed: observed.clone(),
        });
    });
    Guard(observed)
}
impl Guard {
    pub(crate) fn verify(&self) {
        assert!(
            self.0.load(Ordering::Acquire),
            "consumer never crossed the observed-exit delay"
        );
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        DELAY.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}
pub(crate) fn register_pid(pid: u32) {
    DELAY.with(|slot| {
        if let Some(delay) = slot.borrow_mut().as_mut() {
            delay.pid = pid;
        }
    });
}
pub(super) fn register_gate(gate: Arc<Gate>) {
    DELAY.with(|slot| {
        if let Some(delay) = slot.borrow_mut().as_mut() {
            delay.gate = Some(gate);
        }
    });
}
fn published(gate: &Gate) -> std::sync::MutexGuard<'_, State> {
    let state = gate.state.lock().unwrap();
    let (state, _timeout) = gate
        .changed
        .wait_timeout_while(state, Duration::from_secs(5), |s| s.producers != 0)
        .unwrap();
    assert_eq!(
        state.producers, 0,
        "finite fixture reader never published EOF"
    );
    assert!(
        state.published >= 3,
        "normal terminal and EOF must be published"
    );
    state
}
pub(super) fn before_receive(gate: &Arc<Gate>) {
    let pid = DELAY.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|d| d.gate.as_ref().is_some_and(|g| Arc::ptr_eq(g, gate)))
            .map(|d| d.pid)
    });
    if let Some(pid) = pid {
        if gate.state.lock().unwrap().received == 0 {
            assert!(pid > 1);
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe {
                    libc::waitid(
                        libc::P_PID,
                        pid as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOWAIT,
                    )
                },
                0
            );
            drop(published(gate));
        }
    }
}
pub(crate) fn after_exit() {
    let delay = DELAY.with(|slot| slot.borrow_mut().take());
    if let Some(delay) = delay {
        let gate = delay.gate.expect("stdout publication observer missing");
        let state = published(&gate);
        assert!(
            state.published - state.received >= 2,
            "terminal and EOF must still be queued at exit observation"
        );
        eprintln!(
            "R3_PUBLICATION_PROOF pid={} published={} received={} pending={} actual_exit_observed=true",
            delay.pid,
            state.published,
            state.received,
            state.published - state.received
        );
        drop(state);
        std::thread::sleep(super::super::EXIT_DRAIN + Duration::from_millis(50));
        delay.observed.store(true, Ordering::Release);
    }
}
