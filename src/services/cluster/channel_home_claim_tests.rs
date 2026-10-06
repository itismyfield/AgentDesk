//! A delegated channel's placement claims its pending adoption only while its home takes intake,
//! with the gate held from that check through the claim, so a drain's close comes before or after.

use std::sync::mpsc;
use std::time::Duration;

use tokio::time::Instant;

use super::*;
use crate::db::o_channel_homes::HomeState;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute, test_probe};
use crate::services::tui_o::cutover::test_override;

const RACED: u64 = 4_380_701;
const CLOSED: u64 = 4_380_702;
const PLAIN: u64 = 4_380_703;

fn candidate(channel: u64) -> Candidate {
    let found = |boot: Option<&crate::services::tui_o::channel_policy::BootChannels>| {
        boot.and_then(|boot| boot.candidate(channel)).cloned()
    };
    test_override::with_channels(found).expect("a pending adoption")
}

fn open_gate(channel: u64) -> Arc<HomeGate> {
    let gate = Arc::new(HomeGate::new(&channel.to_string(), "mini"));
    register(Arc::clone(&gate));
    let renewal = HeldHome::for_test(&channel.to_string(), "mini", 3, HomeState::Worker);
    gate.confirm(&renewal, Instant::now()).expect("opens");
    gate
}

// A close that comes first holds the placement and leaves the adoption pending; a close sent
// while a placement is between its check and its claim waits until that claim is made.
#[test]
fn a_drain_close_never_lands_between_the_placement_check_and_the_adoption_claim() {
    let _ready = test_probe::answer_with(|_| true);
    let _pending = test_override::force_candidates(&[
        (RACED, ClaudeTui),
        (CLOSED, ClaudeTui),
        (PLAIN, ClaudeTui),
    ]);

    let closed = open_gate(CLOSED);
    closed.close_intake();
    closed.close();
    let held = intake_route::route_for_placement("claude", CLOSED);
    assert!(matches!(held, IntakeRoute::Hold(_)), "{held:?}");
    assert_eq!(candidate(CLOSED).peek(), Adoption::Pending);

    // The adoption's own lock stands in for a claim that takes a moment to be made.
    let raced = open_gate(RACED);
    let adoption = candidate(RACED);
    let (locked_tx, locked) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let holder = std::thread::spawn(move || {
        let _claiming = adoption.lock();
        locked_tx.send(()).unwrap();
        closed_rx.recv_timeout(Duration::from_secs(2)).is_ok()
    });
    locked.recv().unwrap();
    let closer = Arc::clone(&raced);
    let drain = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        closer.close_intake();
        closer.close();
        let _ = closed_tx.send(());
    });
    let placed = intake_route::route_for_placement("claude", RACED);
    assert!(matches!(placed, IntakeRoute::Hold(_)), "{placed:?}");
    let closed_while_claiming = holder.join().unwrap();
    drain.join().unwrap();
    assert!(!closed_while_claiming, "the close waited for the claim");
    let claimed = candidate(RACED).peek();
    assert_eq!(claimed, Adoption::Released, "intake was open at its check");
    let after = intake_route::route_for_placement("claude", RACED);
    assert!(matches!(after, IntakeRoute::Hold(_)), "{after:?}");

    // A channel without a gate is placed as before: its pending adoption goes to Legacy.
    assert_eq!(
        intake_route::route_for_placement("claude", PLAIN),
        IntakeRoute::Unselected
    );
    assert_eq!(candidate(PLAIN).peek(), Adoption::Released);

    for channel in [RACED, CLOSED] {
        unregister(&channel.to_string());
    }
}
