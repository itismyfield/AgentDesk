//! A delegated channel's writer host is gated by its registered home gate instead of the gateway's.

use super::*;
use crate::db::o_channel_homes::{HeldHome, HomeState};
use crate::services::cluster::channel_home::{HOLD_FOR, register_for_test as register};

fn facts_read(io: &TestIo) -> usize {
    let calls = io.calls();
    calls
        .iter()
        .filter(|call| **call == ("facts", CHANNEL))
        .count()
}

// Hosted without the PG lease, the host reads no facts until its home is held, whatever the gateway
// gate says, and readiness ends once the hold lapses even before anything closed its gate.
#[tokio::test(start_paused = true)]
async fn a_delegated_channel_waits_on_and_is_ready_by_its_home_gate() {
    let (harness, _) = fresh(startup);
    harness.gate.acquired();
    let home = register(CHANNEL, None);
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let tasks = hosted(&harness, &io, false, &ready);
    assert_eq!(tasks.len(), 1, "hosted without the PG gateway lease");
    polls(3).await;
    assert_eq!(facts_read(&io), 0, "nothing read before the home is held");
    assert_eq!(harness.store.read_era().unwrap(), None);
    assert!(!ready.accepts(CHANNEL));

    let held = HeldHome::for_test(&CHANNEL.to_string(), "mini", 1, HomeState::Worker);
    home.confirm(&held, tokio::time::Instant::now()).unwrap();
    polls(3).await;
    assert_eq!(facts_read(&io), 1);
    assert_eq!(io.alarms.halted(), []);
    assert!(ready.is_ready(CHANNEL) && ready.accepts(CHANNEL));
    harness.gate.lost();
    assert!(ready.accepts(CHANNEL), "the gateway gate does not decide");
    tokio::time::advance(HOLD_FOR).await;
    assert!(!ready.accepts(CHANNEL), "a lapsed hold takes no work");
    abort(tasks);
}

// A channel with no home registered here stays held without the lease, as before.
#[tokio::test(start_paused = true)]
async fn an_unregistered_channel_still_needs_the_pg_gateway_lease() {
    let (harness, _) = fresh(startup);
    harness.gate.acquired();
    register(OTHER, Some(HomeState::Worker));
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    assert!(hosted(&harness, &io, false, &ready).is_empty());
    polls(3).await;
    assert_eq!(io.calls(), []);
    let halted = io.alarms.halted();
    assert!(matches!(halted.as_slice(), [(CHANNEL, detail)] if detail.contains("no PG gateway")));
}
