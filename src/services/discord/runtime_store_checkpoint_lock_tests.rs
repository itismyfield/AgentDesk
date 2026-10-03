use super::*;
use std::cell::RefCell;
use std::sync::mpsc;

thread_local! {
    static AFTER_READ: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

pub(super) fn after_read() {
    AFTER_READ.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

// Pause the low writer after its read, then order the high writer according to the
// actual lock. Removing the lock lets the low writer overwrite the high checkpoint.
#[test]
fn checkpoint_lock_keeps_max_across_a_paused_low_writer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 4_162_001;
    let path = last_message_root()
        .unwrap()
        .join("claude")
        .join(format!("{channel}.txt"));
    let (read_tx, read_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let low = std::thread::spawn(move || {
        AFTER_READ.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                read_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }));
        });
        save_last_message_id("claude", channel, 90_001);
    });
    read_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap();
    let observer = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(last_message_id_lock_path(&path))
        .unwrap();
    let held = match observer.try_lock() {
        Err(fs::TryLockError::WouldBlock) => true,
        Ok(()) => {
            observer.unlock().unwrap();
            false
        }
        Err(error) => panic!("checkpoint lock probe: {error}"),
    };
    if held {
        release_tx.send(()).unwrap();
        low.join().unwrap();
        save_last_message_id("claude", channel, 90_002);
    } else {
        save_last_message_id("claude", channel, 90_002);
        release_tx.send(()).unwrap();
        low.join().unwrap();
    }
    assert_eq!(
        fs::read_to_string(path).unwrap().trim(),
        "90002",
        "a delayed low writer must not regress the durable checkpoint"
    );
}
