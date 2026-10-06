//! A delegated channel's placement claims its pending adoption only while its home takes intake,
//! with the gate held from that check through the claim, so a drain's close comes before or after.

use std::sync::mpsc;
use std::sync::{Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::time::Instant;
use tracing_subscriber::layer::{Context, SubscriberExt};

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

/// The channel and message of an adoption's log line.
#[derive(Default)]
struct Fields {
    channel: Option<u64>,
    message: String,
}

impl tracing::field::Visit for Fields {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == "channel" {
            self.channel = Some(value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}

/// What a claim of the raced channel saw while it was being made.
struct DuringClaim {
    gate_held: bool,
    closed: bool,
    closer: JoinHandle<()>,
}

/// Runs inside the claim of `gate`'s channel, on the claiming thread, from the claim's own log
/// line: it reads whether the gate lock is held, then starts a drain close and waits for it.
struct ClaimProbe {
    gate: Arc<HomeGate>,
    seen: Arc<Mutex<Option<DuringClaim>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ClaimProbe {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let raced = fields.channel == Some(RACED);
        if !raced || !fields.message.contains("Legacy took the channel") {
            return;
        }
        // The lock a close takes; the claiming thread holds it when the claim is under the gate.
        let gate_held = self.gate.local.try_lock().is_err();
        let (started_tx, started) = mpsc::channel();
        let (closed_tx, closed) = mpsc::channel();
        let gate = Arc::clone(&self.gate);
        let closer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            gate.close_intake();
            gate.close();
            let _ = closed_tx.send(());
        });
        started.recv().unwrap();
        let closed = closed.recv_timeout(Duration::from_millis(100)).is_ok();
        let during = DuringClaim {
            gate_held,
            closed,
            closer,
        };
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = Some(during);
    }
}

// A close that comes first holds the placement and leaves the adoption pending; a close started
// while a placement is making its claim waits until that claim is made and the gate let go.
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

    let raced = open_gate(RACED);
    let seen = Arc::new(Mutex::new(None));
    let probe = ClaimProbe {
        gate: Arc::clone(&raced),
        seen: Arc::clone(&seen),
    };
    crate::logging::test_capture::pin_callsite_interest();
    let subscriber = tracing_subscriber::registry().with(probe);
    let placed = tracing::subscriber::with_default(subscriber, || {
        intake_route::route_for_placement("claude", RACED)
    });
    let during = seen.lock().unwrap().take().expect("the claim was made");
    during.closer.join().unwrap();
    assert!(during.gate_held, "the claim runs under the gate lock");
    assert!(!during.closed, "the close waited for the claim");
    assert!(matches!(placed, IntakeRoute::Hold(_)), "{placed:?}");
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
