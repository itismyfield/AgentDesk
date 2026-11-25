//! The boot owner ends each AGY execution it finds once: route rows and unfinished journals alike.

use std::num::NonZeroU64;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;
use crate::services::discord::inflight::agy_end_authority_tests::{row, write};
use crate::services::discord::inflight::{AgyCustodyField, InflightTurnState};
use crate::services::process::{ProcessIdentity, configure_child_process_group};
use crate::services::tui_o::store::agy_delivery::{Stage, read_saved};
use crate::services::tui_o::store::agy_spool::{AgyReservation, AgySpool};
use crate::services::tui_o::store::{OStore, StoreConfig};
use crate::services::tui_o::writer::agy_delivery::{Evidence, Piece, Post, Transport};
use crate::test_support::wait::{WAIT_CAP, join_within, poll_within};

const FAILED: &str = "-# Antigravity 실행 실패";

#[derive(Default)]
struct Wire {
    posts: Mutex<Vec<String>>,
    next: AtomicU64,
    /// One channel whose POST waits for the barrier; `stalled` tells that it started.
    stall: Mutex<Option<(u64, tokio::sync::oneshot::Receiver<()>)>>,
    stalled: AtomicBool,
}

impl Transport for Wire {
    fn bot(&self) -> Option<NonZeroU64> {
        NonZeroU64::new(9)
    }

    async fn post(&self, channel: u64, piece: &Piece) -> Post {
        let stall = {
            let mut stall = self.stall.lock().unwrap();
            match stall.take() {
                Some((at, barrier)) if at == channel => Some(barrier),
                other => {
                    *stall = other;
                    None
                }
            }
        };
        if let Some(barrier) = stall {
            self.stalled.store(true, Ordering::SeqCst);
            barrier.await.unwrap();
        }
        self.posts.lock().unwrap().push(piece.payload.clone());
        let id = 5_000 + self.next.fetch_add(1, Ordering::Relaxed);
        Post::Created(NonZeroU64::new(id).unwrap())
    }

    async fn history_before(
        &self,
        _channel: u64,
        _before: Option<NonZeroU64>,
    ) -> Result<Vec<Evidence>, String> {
        Ok(Vec::new())
    }
}

struct Lease;

impl DeliveryLease for Lease {
    type Held = ();
    fn try_acquire(&self, _channel: u64, _serial: u64) -> Option<()> {
        Some(())
    }
}

struct Boot {
    _root: crate::config::TestRuntimeRootGuard,
    root: PathBuf,
    gate: OwnershipGate,
    shared: Arc<SharedData>,
    wire: Wire,
    holds: HoldSet,
}

impl Boot {
    fn new() -> Self {
        let guard = crate::config::TestRuntimeRootGuard::new();
        let root = crate::config::runtime_root().unwrap();
        OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
            .unwrap()
            .unwrap();
        let gate = OwnershipGate::default();
        gate.acquired();
        Self {
            _root: guard,
            root,
            gate,
            shared: crate::services::discord::make_shared_data_for_tests(),
            wire: Wire::default(),
            holds: HoldSet::default(),
        }
    }

    /// A route row as a 4a-2 admission saves it; returns the row and its path.
    fn admitted(&self, channel: u64) -> (InflightTurnState, PathBuf) {
        let state = row(channel, true);
        let inflight = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
        let path = write(&inflight, &state);
        (state, path)
    }

    async fn recover(&self) -> BTreeSet<String> {
        self.recover_only(None).await
    }

    async fn recover_only(&self, only: Option<&BTreeSet<String>>) -> BTreeSet<String> {
        let (port, projection) = (&self.wire, &self.shared);
        recover(
            &self.root,
            &self.gate,
            port,
            &Lease,
            projection,
            &self.holds,
            only,
        )
        .await
    }

    /// The route's gated producer as it stands after `started`: a live child in its own group.
    fn producer(&self, state: &InflightTurnState) -> std::process::Child {
        let Some(AgyCustodyField::Route(route)) = state.agy_custody.clone() else {
            panic!("route fixture");
        };
        let reservation = AgyReservation {
            destination: route.destination,
            logical_channel: route.logical_channel,
            message: route.message,
            nonce: route.nonce,
            session_key: "discord:agy",
            writer_generation: route.writer_generation,
            resume: None,
        };
        let mut spool = AgySpool::reserve(&self.root, reservation).unwrap();
        spool.launch().unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);
        configure_child_process_group(&mut command);
        let child = command.spawn().unwrap();
        spool
            .started(child.id(), &ProcessIdentity::capture(child.id()))
            .unwrap();
        child
    }

    fn posts(&self) -> Vec<String> {
        self.wire.posts.lock().unwrap().clone()
    }

    fn saved(&self, key: TurnKey) -> Saved {
        read_saved(&self.root, key.destination.get(), key.id.get())
            .unwrap()
            .unwrap()
    }
}

fn key(state: &InflightTurnState) -> TurnKey {
    TurnKey {
        destination: NonZeroU64::new(state.channel_id).unwrap(),
        id: NonZeroU64::new(state.user_msg_id).unwrap(),
        nonce: state.turn_nonce.as_deref().unwrap().parse().unwrap(),
    }
}

// D1: a route with no spool slot is abandoned once under the boot lease; a later boot reposts nothing.
#[tokio::test(flavor = "current_thread")]
async fn owner_ends_a_no_slot_route_once() {
    let boot = Boot::new();
    let (state, path) = boot.admitted(6_327_400_001);
    assert!(boot.recover().await.is_empty());
    assert_eq!(boot.posts(), [FAILED]);
    assert!(!path.exists(), "the owner ends its own route row");
    let saved = boot.saved(key(&state));
    assert_eq!(
        (saved.ended, saved.revision),
        (Some(Stage::Complete), NonZeroU64::MIN)
    );
    assert!(boot.recover().await.is_empty());
    assert_eq!(boot.posts().len(), 1);
}

// C0: a slot left before its first checkpoint never ran: not started, and set aside.
#[tokio::test(flavor = "current_thread")]
async fn no_checkpoint_slot_is_not_started_and_set_aside() {
    let boot = Boot::new();
    let (state, _) = boot.admitted(6_327_400_002);
    let slot = boot.root.join(format!(
        "o_store/{}/agy/v1/{}",
        state.channel_id, state.user_msg_id
    ));
    std::fs::create_dir_all(&slot).unwrap();
    std::fs::write(slot.join("0000.jsonl"), b"").unwrap();
    assert!(boot.recover().await.is_empty());
    assert_eq!(boot.posts(), [FAILED]);
    assert!(
        !slot.exists(),
        "the empty slot no longer holds its destination"
    );
    let not_started = Plan::abandoned(key(&state), Abandon::NotStarted).fingerprint;
    assert_eq!(boot.saved(key(&state)).fingerprint, not_started);
}

// C3′: a started producer still alive after the restart is ended by its group, then lost, once.
#[tokio::test(flavor = "current_thread")]
async fn boot_ends_a_live_orphan_group_once() {
    let boot = Boot::new();
    let (state, path) = boot.admitted(6_327_400_003);
    let child = boot.producer(&state);
    // Reaped as init would reap it once this process is no longer its parent.
    let reaper = std::thread::spawn(move || {
        let mut child = child;
        child.wait()
    });
    assert!(boot.recover().await.is_empty());
    let ended = join_within(reaper, WAIT_CAP);
    assert!(!ended.unwrap().success());
    assert_eq!(boot.posts(), [FAILED]);
    assert!(!path.exists());
    let lost = Plan::abandoned(key(&state), Abandon::ProducerLost).fingerprint;
    assert_eq!(boot.saved(key(&state)).fingerprint, lost);
}

// P2-2: a journal a restart left after ClearInflight (the row already gone) is found at boot and
// finished without a repost: at Inflight, and with Release committed.
#[tokio::test(flavor = "current_thread")]
async fn journal_left_after_clear_inflight_finishes_without_reposting() {
    for (offset, steps) in [(0, 3), (1, 4)] {
        let boot = Boot::new();
        let state = row(6_327_400_010 + offset, true);
        let key = key(&state);
        let plan = Plan::abandoned(key, Abandon::NoSlot);
        {
            let mut store = AgyJournalRoot::open(&boot.root, key.destination).unwrap();
            let epoch = boot.gate.current().epoch().unwrap();
            let lease = TurnLease {
                key,
                revision: NonZeroU64::MIN,
            };
            let mut replay = Replay::open(&mut store, &boot.gate, epoch, lease, &plan).unwrap();
            for _ in 0..steps {
                replay
                    .advance(&boot.wire, &Lease, &boot.shared)
                    .await
                    .unwrap();
            }
        }
        assert_eq!(boot.posts(), [FAILED], "{steps}");
        assert!(boot.recover().await.is_empty(), "{steps}");
        assert_eq!(boot.posts().len(), 1, "{steps}: no repost");
        assert_eq!(boot.saved(key).ended, Some(Stage::Complete), "{steps}");
    }
}

// P2-3: one key's success leaves the provider held while another key stays unresolved.
#[test]
fn boot_hold_stays_until_every_key_resolves() {
    let holds = HoldSet::default();
    let held = || !crate::services::discord::health::boot_hold::health_reasons().is_empty();
    holds.record("a", Some("agy custody row is held".into()));
    holds.record("b", Some("could not verify the recorded process".into()));
    assert!(held());
    holds.record("b", None);
    assert!(held(), "a is still unresolved");
    holds.record("a", None);
    assert!(!held());
}

fn held() -> bool {
    !crate::services::discord::health::boot_hold::health_reasons().is_empty()
}

// P1-1: a retry disposes of no key it did not leave open; this process's live producer survives.
#[tokio::test(flavor = "current_thread")]
async fn a_retry_never_touches_a_key_it_did_not_leave_open() {
    let boot = Boot::new();
    let (mut held_row, _) = boot.admitted(6_327_400_031);
    if let Some(AgyCustodyField::Route(route)) = &mut held_row.agy_custody {
        route.nonce = uuid::Uuid::new_v4();
    }
    let inflight = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    write(&inflight, &held_row);
    let open = boot.recover().await;
    assert_eq!(open, BTreeSet::from(["row:6327400031".to_owned()]));
    let (fresh, path) = boot.admitted(6_327_400_032);
    let mut child = boot.producer(&fresh);
    let pid = child.id();
    assert_eq!(boot.recover_only(Some(&open)).await, open);
    let alive = crate::services::process::process_group_probe(pid);
    assert_eq!(alive, crate::services::process::ProcessGroupProbe::Live);
    let slot = format!("o_store/{}/agy/v1/{}", fresh.channel_id, fresh.user_msg_id);
    assert!(boot.root.join(slot).is_dir() && path.exists());
    let journal = read_saved(&boot.root, fresh.channel_id, fresh.user_msg_id).unwrap();
    assert!(journal.is_none() && boot.posts().is_empty());
    child.kill().unwrap();
    child.wait().unwrap();
}

// P1-2: a planned restart's marker never strands the owner's own row; the turn still completes.
#[tokio::test(flavor = "current_thread")]
async fn owner_ends_its_route_row_across_a_planned_restart() {
    let boot = Boot::new();
    let (state, path) = boot.admitted(6_327_400_033);
    let mark = crate::services::discord::inflight::mark_restart_mode_blocking;
    mark(
        ProviderKind::Antigravity,
        crate::services::discord::InflightRestartMode::DrainRestart,
    )
    .await;
    let marked: InflightTurnState = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(
        marked.restart_mode.is_some(),
        "the fixture row carries the marker"
    );
    assert!(boot.recover().await.is_empty());
    assert!(!path.exists() && !held());
    assert_eq!(boot.saved(key(&state)).ended, Some(Stage::Complete));
    assert!(boot.recover().await.is_empty());
    assert_eq!(boot.posts(), [FAILED]);
}

// P2-1: every key found is held before any transport; one key's end never hides the next.
#[tokio::test(flavor = "current_thread")]
async fn boot_hold_covers_every_key_until_the_last_one_ends() {
    let boot = Boot::new();
    boot.admitted(6_327_400_041);
    let (late, _) = boot.admitted(6_327_400_042);
    let (release, barrier) = tokio::sync::oneshot::channel();
    *boot.wire.stall.lock().unwrap() = Some((late.channel_id, barrier));
    let observe = async {
        let stalled = async || match boot.wire.stalled.load(Ordering::SeqCst) {
            true => Ok(()),
            false => Err("the second key has not posted".to_owned()),
        };
        poll_within(stalled, WAIT_CAP, "second key's POST")
            .await
            .unwrap();
        assert_eq!(boot.posts(), [FAILED], "the first key ended");
        assert!(held(), "the second key is still unresolved");
        release.send(()).unwrap();
    };
    let (open, ()) = tokio::join!(boot.recover(), observe);
    assert!(open.is_empty() && !held());
}

// P2-2: a failed discovery keeps the pass open, and once a scan completes its debt is ended.
#[tokio::test(flavor = "current_thread")]
async fn a_discovery_failure_hands_its_debt_on_once_the_scan_completes() {
    let boot = Boot::new();
    let state = row(6_327_400_043, true);
    let key = key(&state);
    let plan = Plan::abandoned(key, Abandon::NoSlot);
    {
        let mut store = AgyJournalRoot::open(&boot.root, key.destination).unwrap();
        let epoch = boot.gate.current().epoch().unwrap();
        let lease = TurnLease {
            key,
            revision: NonZeroU64::MIN,
        };
        let mut replay = Replay::open(&mut store, &boot.gate, epoch, lease, &plan).unwrap();
        for _ in 0..4 {
            replay
                .advance(&boot.wire, &Lease, &boot.shared)
                .await
                .unwrap();
        }
    }
    let journals = boot
        .root
        .join(format!("o_store/{}/agy-delivery", state.channel_id));
    let aside = journals.with_file_name("agy-delivery.aside");
    std::fs::rename(&journals, &aside).unwrap();
    std::fs::write(&journals, b"not a directory").unwrap();
    let open = boot.recover().await;
    assert!(open.contains("discovery"), "{open:?}");
    std::fs::remove_file(&journals).unwrap();
    std::fs::rename(&aside, &journals).unwrap();
    assert!(boot.recover_only(Some(&open)).await.is_empty());
    assert_eq!(boot.saved(key).ended, Some(Stage::Complete));
    assert_eq!(boot.posts(), [FAILED], "no repost");
}

// P2-3: the bot identity is fetched only for delivery work, and fetched again after a failure.
#[tokio::test(flavor = "current_thread")]
async fn bot_identity_is_fetched_only_for_work_and_again_after_a_failure() {
    let boot = Boot::new();
    let (state, path) = boot.admitted(6_327_400_044);
    let calls = std::cell::Cell::new(0);
    let identity = || {
        calls.set(calls.get() + 1);
        std::future::ready((calls.get() > 1).then_some(&boot.wire))
    };
    let jobs = judge(&boot.root, None);
    let (gate, shared) = (&boot.gate, &boot.shared);
    drive(
        &boot.root,
        gate,
        &Lease,
        shared,
        jobs,
        identity,
        Duration::ZERO,
    )
    .await;
    assert_eq!(calls.get(), 2);
    assert!(!path.exists());
    assert_eq!(boot.saved(key(&state)).ended, Some(Stage::Complete));
    let none = std::cell::Cell::new(0);
    let empty = || {
        none.set(none.get() + 1);
        std::future::ready(None::<&Wire>)
    };
    drive(
        &boot.root,
        gate,
        &Lease,
        shared,
        BTreeMap::new(),
        empty,
        Duration::ZERO,
    )
    .await;
    assert_eq!(none.get(), 0, "an empty boot fetches nothing");
}
