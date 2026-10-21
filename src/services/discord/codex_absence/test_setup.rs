//! Installs transport instrumentation on the thread that invokes the real driver.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

type Setup = Box<dyn FnOnce(&str) -> Box<dyn Any> + Send>;

struct Entry {
    owner: Arc<()>,
    setup: Setup,
}

static SETUPS: LazyLock<Mutex<HashMap<u64, Entry>>> = LazyLock::new(Default::default);

pub(crate) struct ResetGuard {
    channel: u64,
    owner: Arc<()>,
}

impl Drop for ResetGuard {
    fn drop(&mut self) {
        let removed = {
            let mut setups = SETUPS.lock().unwrap_or_else(|error| error.into_inner());
            if setups
                .get(&self.channel)
                .is_some_and(|entry| Arc::ptr_eq(&entry.owner, &self.owner))
            {
                setups.remove(&self.channel)
            } else {
                None
            }
        };
        drop(removed);
    }
}

pub(crate) fn install(channel: u64, setup: Setup) -> ResetGuard {
    let owner = Arc::new(());
    let mut setups = SETUPS.lock().unwrap_or_else(|error| error.into_inner());
    assert!(
        !setups.contains_key(&channel),
        "driver setup already installed for {channel}"
    );
    setups.insert(
        channel,
        Entry {
            owner: Arc::clone(&owner),
            setup,
        },
    );
    ResetGuard { channel, owner }
}

pub(crate) fn enter(channel: u64, prompt: &str) -> Option<Box<dyn Any>> {
    let setup = SETUPS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&channel)?
        .setup;
    Some(setup(prompt))
}

#[test]
fn setup_constructs_and_drops_a_non_send_guard_on_the_worker_once() {
    struct WorkerOwned {
        owner: std::thread::ThreadId,
        dropped: Arc<Mutex<Vec<std::thread::ThreadId>>>,
        _not_send: std::rc::Rc<()>,
    }
    impl Drop for WorkerOwned {
        fn drop(&mut self) {
            assert_eq!(std::thread::current().id(), self.owner);
            self.dropped.lock().unwrap().push(self.owner);
        }
    }
    let channel = 7_163_208;
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let on_worker = Arc::clone(&dropped);
    let _setup = install(
        channel,
        Box::new(move |prompt| {
            assert_eq!(prompt, "authored message");
            Box::new(WorkerOwned {
                owner: std::thread::current().id(),
                dropped: on_worker,
                _not_send: std::rc::Rc::new(()),
            })
        }),
    );
    let worker = std::thread::spawn(move || {
        assert!(enter(channel + 1, "ignored").is_none());
        let guard = enter(channel, "authored message").expect("matching worker setup");
        assert!(enter(channel, "authored message").is_none());
        assert_eq!(
            guard.downcast_ref::<WorkerOwned>().unwrap().owner,
            std::thread::current().id()
        );
        drop(guard);
        std::thread::current().id()
    })
    .join()
    .unwrap();
    assert_ne!(worker, std::thread::current().id());
    assert_eq!(*dropped.lock().unwrap(), vec![worker]);
}
