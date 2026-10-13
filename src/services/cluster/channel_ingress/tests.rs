use std::sync::Arc;

use chrono::TimeZone;

use super::*;
use crate::db::o_channel_homes::HeldHome;
use crate::services::cluster::channel_home::{self, HomeGate, HomeRefusal};

const CH: u64 = 5_713_000_000_000_001;
const A: &str = "node-a";
const B: &str = "node-b";
const STATES: [HomeState; 6] = [
    HomeState::Releasing,
    HomeState::Released,
    HomeState::Worker,
    HomeState::Reclaiming,
    HomeState::Reclaimed,
    HomeState::Orphaned,
];

use IngressHoldReason as Why;
use IngressObservation::{Hold, IgnoreForeign, LocalCandidate};

fn key() -> IngressKey {
    IngressKey::parse("codex", CH).unwrap()
}

fn renewed() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 13, 5, 0, 0).unwrap()
}

fn row(state: HomeState, holder: Option<&str>, target: Option<&str>, epoch: i64) -> ChannelHome {
    ChannelHome {
        channel_id: CH.to_string(),
        provider: "codex".into(),
        state,
        holder: holder.map(str::to_owned),
        target: target.map(str::to_owned),
        epoch,
        renewed_at: holder.map(|_| renewed()),
        updated_at: renewed(),
        detail: None,
    }
}

fn worker(holder: &str, epoch: i64) -> ChannelHome {
    row(HomeState::Worker, Some(holder), None, epoch)
}

/// A read that saw the row `age` after its renewal and began `elapsed` before `now`.
fn clock(now: Instant, age: Duration, elapsed: Duration) -> ReadClock {
    ReadClock {
        pg_now: Some(renewed() + chrono::Duration::from_std(age).unwrap()),
        started: Some(now - elapsed),
    }
}

fn fresh(now: Instant) -> ReadClock {
    clock(now, Duration::from_secs(1), Duration::from_millis(10))
}

fn owned(home_epoch: i64, gate_epoch: u64, intake: HomeIntake) -> HomeOwnership {
    HomeOwnership::Owned {
        home_epoch,
        gate_epoch,
        intake,
    }
}

fn sample(holder: &str, ownership: HomeOwnership, now: Instant) -> LocalLeaseSample {
    LocalLeaseSample {
        key: key(),
        holder: holder.into(),
        ownership,
        sampled_at: now,
        expires_at: Some(now + HOLD_FOR / 2),
    }
}

/// What this node's real gate reads now, as a sampler would take it.
fn sampled(gate: &HomeGate, now: Instant) -> LocalLeaseSample {
    LocalLeaseSample {
        key: key(),
        holder: gate.holder().into(),
        ownership: gate.ownership(),
        sampled_at: now,
        expires_at: gate.expiry(),
    }
}

fn held_gate(channel: u64, holder: &str, epoch: i64, state: HomeState) -> Arc<HomeGate> {
    let gate = Arc::new(HomeGate::new(&channel.to_string(), holder));
    let written = HeldHome::for_test(&channel.to_string(), holder, epoch, state);
    gate.confirm(&written, Instant::now()).unwrap();
    gate
}

fn at(
    local_id: &str,
    source: SourceEpoch,
    home: HomeRead<'_>,
    local: Option<&LocalLeaseSample>,
    now: Instant,
) -> IngressObservation {
    observe(&key(), local_id, source, home, local, now)
}

fn unstamped(
    local_id: &str,
    row: &ChannelHome,
    local: Option<&LocalLeaseSample>,
    now: Instant,
) -> IngressObservation {
    at(
        local_id,
        SourceEpoch::UnstampedSource,
        HomeRead::Row(row, fresh(now)),
        local,
        now,
    )
}

#[tokio::test(start_paused = true)]
async fn own_foreign_and_unknown_homes_stay_apart() {
    let now = Instant::now();
    let mine = sample(A, owned(4, 1, HomeIntake::Open), now);
    let source = SourceEpoch::UnstampedSource;
    assert_eq!(
        unstamped(A, &worker(A, 4), Some(&mine), now),
        LocalCandidate { home_epoch: 4 }
    );
    assert_eq!(
        unstamped(A, &worker(B, 4), Some(&mine), now),
        IgnoreForeign {
            holder: B.into(),
            home_epoch: 4
        }
    );
    let missing = at(A, source, HomeRead::Missing, Some(&mine), now);
    assert_eq!(missing, Hold(Why::NoHomeAuthority));
    let unreadable = at(A, source, HomeRead::Unreadable, Some(&mine), now);
    assert_eq!(unreadable, Hold(Why::AuthorityUnreadable));
    let stale = clock(now, HOLD_FOR, Duration::ZERO);
    let expired = at(
        A,
        source,
        HomeRead::Row(&worker(A, 4), stale),
        Some(&mine),
        now,
    );
    assert_eq!(expired, Hold(Why::RowExpired));
    assert_eq!(
        unstamped(" ", &worker(A, 4), Some(&mine), now),
        Hold(Why::NoLocalIdentity)
    );
}

#[tokio::test(start_paused = true)]
async fn own_worker_row_holds_without_an_open_unexpired_local_lease() {
    let now = Instant::now();
    let row = worker(A, 4);
    assert_eq!(unstamped(A, &row, None, now), Hold(Why::NoLocalLease));
    let never_renewed = HomeGate::new(&CH.to_string(), A);
    let lost = sampled(&never_renewed, now);
    assert_eq!(unstamped(A, &row, Some(&lost), now), Hold(Why::LocalLost));
    let draining = held_gate(CH, A, 4, HomeState::Worker);
    draining.close_intake();
    let closed = sampled(&draining, now);
    assert_eq!(
        unstamped(A, &row, Some(&closed), now),
        Hold(Why::LocalIntakeClosed)
    );
    let mut lapsing = sample(A, owned(4, 1, HomeIntake::Open), now - HOLD_FOR / 2);
    lapsing.expires_at = Some(now);
    assert_eq!(
        unstamped(A, &row, Some(&lapsing), now),
        Hold(Why::LocalExpired)
    );
    lapsing.expires_at = Some(now + Duration::from_nanos(1));
    assert_eq!(
        unstamped(A, &row, Some(&lapsing), now),
        LocalCandidate { home_epoch: 4 }
    );
    let mut undated = sample(A, owned(4, 1, HomeIntake::Open), now);
    undated.expires_at = None;
    assert_eq!(
        unstamped(A, &row, Some(&undated), now),
        Hold(Why::TimeUnknown)
    );
    let from_later = sample(
        A,
        owned(4, 1, HomeIntake::Open),
        now + Duration::from_millis(1),
    );
    assert_eq!(
        unstamped(A, &row, Some(&from_later), now),
        Hold(Why::TimeUnknown)
    );
    let mut other_holder = sample(A, owned(4, 1, HomeIntake::Open), now);
    other_holder.holder = B.into();
    assert_eq!(
        unstamped(A, &row, Some(&other_holder), now),
        Hold(Why::LocalSampleMismatch)
    );
    // Holder, epoch and intake match; only the sample's channel or provider differs.
    for other in [("codex", CH + 1), ("claude", CH)] {
        let mut elsewhere = sample(A, owned(4, 1, HomeIntake::Open), now);
        elsewhere.key = IngressKey::parse(other.0, other.1).unwrap();
        assert_eq!(
            unstamped(A, &row, Some(&elsewhere), now),
            Hold(Why::LocalSampleMismatch),
            "{other:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn only_a_worker_holder_takes_ingress_never_a_released_target() {
    let now = Instant::now();
    let mine = sample(A, owned(4, 1, HomeIntake::Open), now);
    let rows = [
        row(HomeState::Releasing, Some(A), Some(B), 4),
        row(HomeState::Released, None, Some(A), 4),
        row(HomeState::Reclaiming, Some(A), Some(B), 4),
        row(HomeState::Reclaimed, None, Some(A), 4),
        row(HomeState::Orphaned, None, None, 4),
    ];
    for row in &rows {
        assert_eq!(
            unstamped(A, row, Some(&mine), now),
            Hold(Why::HomeNotTakingIngress(row.state)),
            "{:?}",
            row.state
        );
    }
}

#[tokio::test(start_paused = true)]
async fn identity_is_the_provider_and_the_actual_channel() {
    let now = Instant::now();
    let mine = sample(A, owned(4, 1, HomeIntake::Open), now);
    let mut claude = worker(A, 4);
    claude.provider = "claude".into();
    assert_eq!(
        unstamped(A, &claude, Some(&mine), now),
        Hold(Why::ProviderMismatch)
    );
    let mut parent = worker(A, 4);
    parent.channel_id = (CH + 1).to_string();
    assert_eq!(
        unstamped(A, &parent, Some(&mine), now),
        Hold(Why::ChannelMismatch)
    );
    let mut malformed = Vec::new();
    for edit in [
        (|r: &mut ChannelHome| r.holder = None) as fn(&mut ChannelHome),
        |r| r.holder = Some("  ".into()),
        |r| r.target = Some(B.into()),
        |r| r.epoch = 0,
        |r| r.renewed_at = None,
        |r| r.provider = "gpt".into(),
        |r| r.provider = "Codex".into(),
        |r| r.state = HomeState::Released,
    ] {
        let mut row = worker(A, 4);
        edit(&mut row);
        malformed.push(unstamped(A, &row, Some(&mine), now));
    }
    assert!(
        malformed.iter().all(|seen| *seen == Hold(Why::InvalidHome)),
        "{malformed:?}"
    );
    assert_eq!(IngressKey::parse("codex", 0), None);
    assert_eq!(IngressKey::parse("gpt", CH), None);
    assert_eq!(IngressKey::parse(" CODEX ", CH), Some(key()));
}

#[tokio::test(start_paused = true)]
async fn a_routed_input_keeps_its_original_epoch_and_never_compares_the_gate_epoch() {
    let now = Instant::now();
    let row = worker(A, 100);
    let mine = sample(A, owned(100, 2, HomeIntake::Open), now);
    let routed = |epoch| {
        at(
            A,
            SourceEpoch::Routed(epoch),
            HomeRead::Row(&row, fresh(now)),
            Some(&mine),
            now,
        )
    };
    assert_eq!(
        unstamped(A, &row, Some(&mine), now),
        LocalCandidate { home_epoch: 100 }
    );
    assert_eq!(routed(Some(100)), LocalCandidate { home_epoch: 100 });
    assert_eq!(routed(Some(99)), Hold(Why::StaleSourceEpoch));
    // An outbox row forwarded under the gateway rules carries no epoch; the claim fence holds it.
    let legacy_epoch: Option<i64> = None;
    assert_eq!(routed(legacy_epoch), Hold(Why::RouteWithoutHomeEpoch));
    let gate = held_gate(CH, A, 100, HomeState::Worker);
    channel_home::register(Arc::clone(&gate));
    let fence = channel_home::intake_hold(&CH.to_string(), legacy_epoch);
    channel_home::unregister(&CH.to_string());
    assert!(fence.is_some(), "the existing claim fence holds it too");
    let older_gate = sample(A, owned(99, 100, HomeIntake::Open), now);
    assert_eq!(
        unstamped(A, &row, Some(&older_gate), now),
        Hold(Why::LocalEpochMismatch)
    );
    let missing = at(
        A,
        SourceEpoch::Routed(Some(100)),
        HomeRead::Missing,
        Some(&mine),
        now,
    );
    assert_eq!(missing, Hold(Why::NoHomeAuthority));
}

#[tokio::test(start_paused = true)]
async fn row_freshness_counts_the_whole_read_and_ends_at_hold_for() {
    let now = Instant::now();
    let mine = sample(A, owned(4, 1, HomeIntake::Open), now);
    let ms = Duration::from_millis;
    let read = |home: &ChannelHome, clock| {
        at(
            A,
            SourceEpoch::UnstampedSource,
            HomeRead::Row(home, clock),
            Some(&mine),
            now,
        )
    };
    let (own, foreign) = (worker(A, 4), worker(B, 4));
    let just_fresh = clock(now, HOLD_FOR - ms(1000), ms(999));
    assert_eq!(read(&own, just_fresh), LocalCandidate { home_epoch: 4 });
    let at_hold = clock(now, HOLD_FOR - ms(1000), ms(1000));
    assert_eq!(read(&own, at_hold), Hold(Why::RowExpired));
    assert_eq!(read(&foreign, at_hold), Hold(Why::RowExpired));
    let slow_reply = clock(now, Duration::from_secs(15), Duration::from_secs(6));
    assert_eq!(read(&own, slow_reply), Hold(Why::RowExpired));
    let unknown = [
        ReadClock {
            pg_now: None,
            ..fresh(now)
        },
        ReadClock {
            started: None,
            ..fresh(now)
        },
        ReadClock {
            pg_now: Some(renewed() - chrono::Duration::milliseconds(1)),
            ..fresh(now)
        },
        ReadClock {
            started: Some(now + ms(1)),
            ..fresh(now)
        },
    ];
    for clock in unknown {
        assert_eq!(read(&own, clock), Hold(Why::TimeUnknown), "{clock:?}");
        assert_eq!(read(&foreign, clock), Hold(Why::TimeUnknown), "{clock:?}");
    }
    let mut ancient = worker(A, 4);
    ancient.renewed_at = Some(DateTime::<Utc>::MIN_UTC);
    let far = ReadClock {
        pg_now: Some(DateTime::<Utc>::MAX_UTC),
        ..fresh(now)
    };
    assert!(matches!(read(&ancient, far), Hold(_)));
}

#[tokio::test(start_paused = true)]
async fn two_nodes_never_both_observe_a_candidate_for_one_row() {
    let now = Instant::now();
    let (mut scenarios, mut candidates) = (0, 0);
    for state in STATES {
        for holder in [Some(A), Some(B), Some("node-c"), None] {
            for target in [None, Some(A), Some(B)] {
                let row = row(state, holder, target, 7);
                // Each node may still hold a gate at the row's epoch, as after a split brain.
                for (a_open, b_open) in [(false, false), (true, false), (false, true), (true, true)]
                {
                    let a = a_open.then(|| sample(A, owned(7, 1, HomeIntake::Open), now));
                    let b = b_open.then(|| sample(B, owned(7, 3, HomeIntake::Open), now));
                    let seen = [
                        unstamped(A, &row, a.as_ref(), now),
                        unstamped(B, &row, b.as_ref(), now),
                    ];
                    let count = seen
                        .iter()
                        .filter(|seen| matches!(seen, LocalCandidate { .. }))
                        .count();
                    assert!(count <= 1, "{row:?} a={a_open} b={b_open}: {seen:?}");
                    scenarios += 1;
                    candidates += count;
                }
            }
        }
    }
    assert_eq!(scenarios, 6 * 4 * 3 * 4);
    // A holder's own open gate is a candidate on a well-formed worker row: holders A and B, two
    // gate pairings each.
    assert_eq!(candidates, 2 * 2);
}

#[tokio::test(start_paused = true)]
async fn an_earlier_candidate_admits_nothing_after_a_handoff() {
    let now = Instant::now();
    let a = held_gate(CH, A, 5, HomeState::Worker);
    let earlier = unstamped(A, &worker(A, 5), Some(&sampled(&a, now)), now);
    assert_eq!(earlier, LocalCandidate { home_epoch: 5 });
    a.close();
    let b = held_gate(CH, B, 7, HomeState::Worker);
    let current = worker(B, 7);
    assert_eq!(
        unstamped(A, &current, Some(&sampled(&a, now)), now),
        IgnoreForeign {
            holder: B.into(),
            home_epoch: 7
        }
    );
    let remembered = sample(A, owned(5, 1, HomeIntake::Open), now);
    assert!(matches!(
        unstamped(A, &current, Some(&remembered), now),
        IgnoreForeign { .. }
    ));
    assert_eq!(
        unstamped(B, &current, Some(&sampled(&b, now)), now),
        LocalCandidate { home_epoch: 7 }
    );
    assert_eq!(earlier, LocalCandidate { home_epoch: 5 });
    assert_eq!(
        (a.admit(|epoch| epoch), a.admit_post(|epoch| epoch)),
        (None, None)
    );
    assert!(a.admit_command("codex").is_none() && a.admit_recovery("codex").is_none());
    let replay = HeldHome::for_test(&CH.to_string(), A, 5, HomeState::Worker);
    assert!(a.confirm(&replay, Instant::now()).is_err());
    assert_eq!(b.admit(|epoch| epoch), Some(7));
}

#[derive(Debug, PartialEq)]
struct EntryPoints {
    unattributed: Result<bool, HomeRefusal>,
    commands: Vec<Result<bool, HomeRefusal>>,
    refusals: Vec<Option<HomeRefusal>>,
    turn_refusals: Vec<Option<crate::services::turn_host::HerdrRefusal>>,
    claim_holds: Vec<(bool, bool)>,
    open_gate: (
        Option<i64>,
        Option<u64>,
        HomeOwnership,
        Option<Instant>,
        usize,
    ),
    held_channels: Vec<String>,
    health: Option<serde_json::Value>,
}

/// Every existing home entry point reachable here, read for an open, a draining, a never-held and
/// an unregistered channel.
async fn entry_points(open: &HomeGate, channels: &[u64]) -> EntryPoints {
    let codex = crate::services::provider::ProviderKind::Codex;
    let mut turn_refusals = Vec::new();
    for channel in channels {
        let refusal =
            crate::services::turn_host::refusal_before_turn(None, &codex, *channel, || {
                std::future::ready(None)
            });
        turn_refusals.push(refusal.await);
    }
    let text = |channel: &u64| channel.to_string();
    EntryPoints {
        unattributed: channel_home::admit_unattributed().map(|permit| permit.is_some()),
        commands: channels
            .iter()
            .map(|channel| {
                channel_home::admit_command(&text(channel), "codex").map(|p| p.is_some())
            })
            .collect(),
        refusals: channels
            .iter()
            .map(|channel| channel_home::refusal(*channel))
            .collect(),
        turn_refusals,
        claim_holds: channels
            .iter()
            .map(|channel| {
                let hold = |epoch| channel_home::intake_hold(&text(channel), epoch).is_some();
                (hold(Some(3)), hold(None))
            })
            .collect(),
        open_gate: (
            open.admit(|epoch| epoch),
            open.admit_post(|epoch| epoch),
            open.ownership(),
            open.expiry(),
            open.commands_in_flight(),
        ),
        held_channels: channel_home::intake_held_channels(),
        health: channel_home::health(),
    }
}

#[tokio::test(start_paused = true)]
async fn observing_leaves_every_existing_home_entry_point_unchanged() {
    let channels = [CH, CH + 1, CH + 2, CH + 3];
    let open = held_gate(CH, A, 3, HomeState::Worker);
    let draining = held_gate(CH + 1, A, 3, HomeState::Releasing);
    let never = Arc::new(HomeGate::new(&(CH + 2).to_string(), A));
    for gate in [&open, &draining, &never] {
        channel_home::register(Arc::clone(gate));
    }
    let calls = observed_calls();
    let before = entry_points(&open, &channels).await;
    assert_eq!(observed_calls(), calls, "no entry point observes");
    let (not_held, closed) = (Err(HomeRefusal::NotHeld), Err(HomeRefusal::Draining));
    assert_eq!(before.commands, vec![Ok(true), closed, not_held, Ok(false)]);
    assert_eq!(before.unattributed, not_held);
    assert_eq!(
        before.claim_holds,
        vec![(false, true), (true, true), (true, true), (true, false)]
    );
    assert_eq!(before.open_gate.0, Some(3));
    assert_eq!(before.held_channels.len(), 2);

    let now = Instant::now();
    let samples = [
        sampled(&open, now),
        sampled(&draining, now),
        sampled(&never, now),
    ];
    let mut seen = Vec::new();
    for state in STATES {
        for holder in [Some(A), Some(B), None] {
            let row = row(state, holder, None, 3);
            for source in [
                SourceEpoch::UnstampedSource,
                SourceEpoch::Routed(Some(3)),
                SourceEpoch::Routed(None),
            ] {
                for local in samples.iter().map(Some).chain([None]) {
                    let home = HomeRead::Row(&row, fresh(now));
                    seen.push(at(A, source, home, local, now));
                }
            }
        }
    }
    assert_eq!(observed_calls(), calls + seen.len());
    assert!(seen.contains(&LocalCandidate { home_epoch: 3 }));

    let after = entry_points(&open, &channels).await;
    for channel in channels {
        channel_home::unregister(&channel.to_string());
    }
    assert_eq!(after, before);
}

/// The design's inventory table must name every non-test source file that reaches the home row,
/// the home gate, delegation availability or the command fence directly.
#[test]
fn home_authority_inventory_lists_every_direct_consumer() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let doc = std::fs::read_to_string(root.join("docs/design/i5713-channel-ingress-contract.md"))
        .expect("inventory doc");
    let listed: std::collections::BTreeSet<String> = doc
        .lines()
        .filter_map(|line| line.strip_prefix("| `src/")?.split_once('`'))
        .map(|(path, _)| format!("src/{path}"))
        .collect();
    const MARKS: [&str; 6] = [
        "o_channel_homes",
        "channel_home::",
        "channel_home_boot",
        "channel_home_drain",
        "home_fence",
        "home_availability",
    ];
    let mut pending = vec![root.join("src")];
    let mut consumers = std::collections::BTreeSet::new();
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let name = path.file_name().unwrap().to_string_lossy();
            if path.is_dir() {
                if name != "tests" {
                    pending.push(path);
                }
                continue;
            }
            let test_only = name == "tests.rs" || name.ends_with("_tests.rs");
            if !name.ends_with(".rs") || test_only {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            if MARKS.iter().any(|mark| text.contains(mark)) {
                consumers.insert(relative);
            }
        }
    }
    assert!(
        consumers.contains("src/db/o_channel_homes.rs"),
        "scan reached src"
    );
    let missing: Vec<_> = consumers.difference(&listed).collect();
    assert!(
        missing.is_empty(),
        "unclassified home consumers: {missing:?}"
    );
    let stale: Vec<_> = listed
        .iter()
        .filter(|path| !root.join(path).is_file())
        .collect();
    assert!(stale.is_empty(), "inventory rows naming no file: {stale:?}");
}
