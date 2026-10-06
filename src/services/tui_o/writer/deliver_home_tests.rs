//! A delegated channel's POSTs go through its registered home gate, whatever gate its writer holds.

use std::sync::mpsc;

use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::cluster::channel_home::{HOLD_FOR, register_for_test as register};

fn prepared_epoch(writer: &mut Writer, serial: u64) -> Option<u64> {
    writer
        .store()
        .ledger()
        .piece(serial)
        .map(|piece| piece.epoch)
}

// Only the home gate admits, whatever the gateway gate says: owed pieces still post while draining,
// nothing after a lapsed hold or the final close.
#[tokio::test(start_paused = true)]
async fn a_delegated_channel_posts_under_its_home_through_the_drain_and_none_after_close() {
    let harness = Harness::new();
    let mut writer = harness.writer();
    register(CHANNEL + 1, Some(HomeState::Worker));
    assert_eq!(writer.deliver(&piece("m0", "a")).await, Step::NoGateway);
    let epoch = harness.gate.acquired();
    assert_eq!(writer.deliver(&piece("m0", "a")).await, Step::Done);
    assert_eq!(prepared_epoch(&mut writer, 0), Some(epoch), "unregistered");

    register(CHANNEL, None);
    assert_eq!(writer.deliver(&piece("m1", "b")).await, Step::NoGateway);
    let home = register(CHANNEL, Some(HomeState::Worker));
    harness.gate.lost();
    assert_eq!(writer.deliver(&piece("m1", "b")).await, Step::Done);
    let home_epoch = prepared_epoch(&mut writer, 1);
    assert_eq!(home_epoch, Some(1), "the home gate's epoch");
    home.close_intake();
    assert_eq!(writer.deliver(&piece("m2", "c")).await, Step::Done);
    // No renewal for H: nothing closed the inner gate, yet the hold has lapsed.
    tokio::time::advance(HOLD_FOR).await;
    assert_eq!(writer.deliver(&piece("m3", "d")).await, Step::NoGateway);
    let home = register(CHANNEL, Some(HomeState::Releasing));
    assert_eq!(writer.deliver(&piece("m3", "d")).await, Step::Done);
    home.close();
    harness.gate.acquired();
    assert_eq!(writer.deliver(&piece("m4", "e")).await, Step::NoGateway);
    assert_eq!(harness.port.posts(), ["a", "b", "c", "d"]);
    let serials = writer.store().ledger().next_serial();
    assert_eq!(serials, 4, "nothing prepared after close");
}

// A POST being admitted holds the home gate: the final close waits until it is handed off, and
// nothing is admitted after the close.
#[tokio::test(start_paused = true)]
async fn the_final_close_waits_for_an_admission_in_progress() {
    let harness = Harness::new();
    let mut writer = harness.writer();
    let home = register(CHANNEL, Some(HomeState::Releasing));
    let (entered, admitting) = mpsc::channel();
    let closing = std::sync::Arc::clone(&home);
    let closed = Arc::new(AtomicBool::new(false));
    let closer = {
        let closed = Arc::clone(&closed);
        std::thread::spawn(move || {
            admitting.recv().unwrap();
            closing.close();
            closed.store(true, Ordering::SeqCst);
        })
    };
    let overtaken = Arc::new(AtomicBool::new(true));
    let seen = Arc::clone(&overtaken);
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        entered.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        seen.store(closed.load(Ordering::SeqCst), Ordering::SeqCst);
    }));
    assert_eq!(writer.deliver(&piece("m1", "owed")).await, Step::Done);
    closer.join().unwrap();
    assert!(
        !overtaken.load(Ordering::SeqCst),
        "the close overtook an admission"
    );
    *harness.port.on_post.lock().unwrap() = None;
    assert_eq!(writer.deliver(&piece("m2", "late")).await, Step::NoGateway);
    assert_eq!(harness.port.posts(), ["owed"]);
}
