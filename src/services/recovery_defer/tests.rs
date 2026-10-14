use super::*;
use std::cell::Cell;
use std::sync::{Arc, Barrier};

fn sec(value: u64) -> Duration {
    Duration::from_secs(value)
}
fn key(source: Source, id: u64) -> Key {
    Key {
        source,
        namespace: id % 7,
        identity: id.into(),
    }
}
fn page<'a>(state: &mut Scheduler, source: Source, values: &'a [u64], now: u64) -> Page<'a, u64> {
    state.page(source, sec(now), |after| {
        values
            .iter()
            .filter(move |id| **id > after)
            .map(|id| (*id, id))
    })
}
fn seed(state: &mut Scheduler, source: Source, values: &[u64], now: u64) {
    loop {
        let p = page(state, source, values, now);
        if p.len() == 0 {
            break;
        }
        // A page can outlive a round. Partial pages resume after the last examined ordinal.
        state.begin_round(sec(now));
        let before = state.cursors.visibility(sec(now)).0;
        for v in p.iter() {
            state.visit(v, key(source, *v.value), sec(now));
        }
        if state.cursors.visibility(sec(now)).0 == before {
            break;
        }
        state.round_at = None;
    }
}

#[test]
fn bounds_and_running_eviction() {
    let mut state = Scheduler::new();
    let values: Vec<_> = (1..=400).collect();
    seed(&mut state, Source::Row, &values[..5], 0);
    state.begin_round(sec(30));
    let mut permits = Vec::new();
    for id in 1..=4 {
        permits.push(
            state
                .probe_cached(key(Source::Row, id), sec(30))
                .expect("initial permit"),
        );
    }
    state.begin_round(sec(31));
    assert!(
        state.probe_cached(key(Source::Row, 5), sec(31)).is_none(),
        "fifth concurrent probe"
    );
    seed(&mut state, Source::Row, &values, 31);
    seed(&mut state, Source::Report, &values, 31);
    seed(&mut state, Source::Inventory, &values, 31);
    let p = page(&mut state, Source::Inventory, &values, 331);
    let visible = state.visibility(sec(331));
    assert_eq!(visible.cached, 256);
    assert_eq!(visible.in_flight, 4);
    assert_eq!(visible.page, 64);
    assert_eq!(visible.cached + visible.in_flight + visible.page, 324);
    assert!(visible.saturated);
    let second = page(&mut state, Source::Report, &values, 331);
    assert_eq!(second.len(), 0, "only one page across all sources");
    for permit in permits {
        assert!(state.complete_unmeasured(permit, sec(331)));
    }
    drop(p);
    assert_eq!(state.visibility(sec(331)).page, 0);
}

#[test]
fn due_boundaries_and_backoff() {
    for (source, first) in [
        (Source::Row, 30),
        (Source::Report, 30),
        (Source::Inventory, 60),
    ] {
        let mut state = Scheduler::new();
        seed(&mut state, source, &[1], 0);
        state.begin_round(sec(first - 1));
        assert!(state.probe_cached(key(source, 1), sec(first - 1)).is_none());
        let mut now = first;
        let mut previous_serial = None;
        for delay in [60, 120, 300, 300] {
            state.begin_round(sec(now));
            let permit = state
                .probe_cached(key(source, 1), sec(now))
                .expect("due permit");
            if let Some(serial) = previous_serial {
                assert!(
                    !state.complete_unmeasured(Permit { serial }, sec(now)),
                    "stale completion must protect next permit"
                );
                assert_eq!(state.visibility(sec(now)).in_flight, 1);
            }
            let serial = permit.serial;
            previous_serial = Some(serial);
            assert!(
                state.probe_cached(key(source, 1), sec(now)).is_none(),
                "in-progress dedup"
            );
            assert!(state.complete_unmeasured(permit, sec(now)));
            assert!(
                !state.complete_unmeasured(Permit { serial }, sec(now)),
                "duplicate completion"
            );
            state.begin_round(sec(now + delay - 1));
            assert!(
                !state
                    .probe_cached(key(source, 1), sec(now + delay - 1))
                    .is_some(),
                "early backoff"
            );
            now += delay;
        }
    }
}

#[test]
fn round_budgets_do_not_reset_on_intake_or_cache_miss() {
    let mut state = Scheduler::new();
    seed(&mut state, Source::Row, &(1..=20).collect::<Vec<_>>(), 0);
    state.begin_round(sec(30));
    for id in 1..=4 {
        let permit = state
            .probe_cached(key(Source::Row, id), sec(30))
            .expect("round permit");
        state.complete_unmeasured(permit, sec(30));
        state.begin_round(sec(30));
    }
    assert!(
        state.probe_cached(key(Source::Row, 5), sec(30)).is_none(),
        "per-round probe limit"
    );
    state.begin_round(sec(31));
    for _ in 0..16 {
        assert!(
            !state
                .probe_cached(key(Source::Report, 999), sec(31))
                .is_some()
        );
    }
    assert!(
        state.probe_cached(key(Source::Row, 5), sec(31)).is_none(),
        "per-round candidate limit"
    );
    state.begin_round(sec(30));
    assert!(
        state.probe_cached(key(Source::Row, 5), sec(30)).is_none(),
        "time reversal cannot reset round"
    );
    state.begin_round(sec(32));
    assert!(state.probe_cached(key(Source::Row, 5), sec(32)).is_some());
}

#[test]
fn late_intake_keeps_its_first_due_after_epoch_restart() {
    for (source, delay) in [
        (Source::Row, 30),
        (Source::Report, 30),
        (Source::Inventory, 60),
    ] {
        let mut state = Scheduler::new();
        seed(&mut state, source, &[1], 0);
        let p = page(&mut state, source, &[1, 2], 300);
        state.begin_round(sec(300));
        for visit in p.iter() {
            if let Some(permit) = state.visit(visit, key(source, *visit.value), sec(300)) {
                state.complete_unmeasured(permit, sec(300));
            }
        }
        drop(p);
        let p = page(&mut state, source, &[1, 2, 3], 350);
        state.begin_round(sec(350));
        assert_eq!(p.len(), 1);
        assert!(
            state
                .visit(p.iter().next().unwrap(), key(source, 3), sec(350))
                .is_none(),
            "late intake first due"
        );
        state.begin_round(sec(350 + delay - 1));
        assert!(
            !state
                .probe_cached(key(source, 3), sec(350 + delay - 1))
                .is_some()
        );
        state.begin_round(sec(350 + delay));
        assert!(
            state
                .probe_cached(key(source, 3), sec(350 + delay))
                .is_some()
        );
    }
}

#[test]
fn cursors_revisit_all_sources_and_resume_partial_pages() {
    let mut state = Scheduler::new();
    let values: Vec<_> = (1..=700).collect();
    let mut visited = [Vec::new(), Vec::new(), Vec::new()];
    for tick in 0..600 {
        let source = state.next_source();
        assert_eq!(
            source,
            [Source::Row, Source::Report, Source::Inventory][tick % 3]
        );
        let p = page(&mut state, source, &values, tick as u64);
        state.begin_round(sec(tick as u64));
        for v in p.iter().take(16) {
            if let Some(permit) = state.visit(v, key(source, *v.value), sec(tick as u64)) {
                state.complete_unmeasured(permit, sec(tick as u64));
            }
            visited[tick % 3].push(*v.value);
        }
    }
    for seen in visited {
        assert!(seen.len() >= 1400, "finite inventory must cycle");
        assert_eq!(&seen[..700], values.as_slice());
        assert_eq!(&seen[700..1400], values.as_slice());
    }
}

#[test]
fn eviction_epoch_and_stale_page() {
    let mut state = Scheduler::new();
    let values: Vec<_> = (1..=300).collect();
    let old = page(&mut state, Source::Row, &values[..1], 0);
    state.begin_round(sec(0));
    let visit = old.iter().next().unwrap();
    assert!(state.visit(visit, key(Source::Row, 1), sec(0)).is_none());
    // Evict the first hint using a different source while the borrowed ticket stays alive.
    for id in 2..=300 {
        state.insert_hint(Hint {
            key: key(Source::Report, id),
            due: sec(0),
            failures: 0,
        });
    }
    state.begin_round(sec(30));
    assert!(
        state.visit(visit, key(Source::Row, 1), sec(30)).is_none(),
        "eviction cannot reuse a ticket"
    );
    drop(old);
    assert_eq!(page(&mut state, Source::Row, &values[..1], 30).len(), 0);
    assert_eq!(page(&mut state, Source::Row, &values[..1], 299).len(), 0);
    let next = page(&mut state, Source::Row, &values[..1], 300);
    state.begin_round(sec(300));
    assert!(
        state
            .visit(next.iter().next().unwrap(), key(Source::Row, 1), sec(300))
            .is_none(),
        "new epoch first due"
    );
    state.begin_round(sec(330));
    assert!(state.probe_cached(key(Source::Row, 1), sec(330)).is_some());
}

#[test]
fn carrier_survives_eviction_and_restart_and_stream_is_bounded() {
    let raw: Vec<String> = (1..=900)
        .map(|id| format!("{id}:attempt=old;mtime=123;marker=keep"))
        .collect();
    let original = raw.clone();
    let calls = Cell::new(0);
    let mut state = Scheduler::new();
    let p = state.page(Source::Report, sec(0), |after| {
        raw.iter()
            .enumerate()
            .skip(after as usize)
            .map(|(i, value)| {
                calls.set(calls.get() + 1);
                (i as u64 + 1, value)
            })
    });
    assert_eq!(p.len(), 64);
    assert_eq!(
        calls.get(),
        64,
        "stream cannot materialize the full inventory"
    );
    drop(p);
    for id in 1..=900 {
        state.insert_hint(Hint {
            key: key(Source::Report, id),
            due: sec(30),
            failures: 0,
        });
    }
    assert_eq!(state.visibility(sec(0)).cached, 256);
    drop(state);
    let mut restarted = Scheduler::new();
    let discovered = restarted.page(Source::Report, sec(900), |after| {
        raw.iter()
            .enumerate()
            .skip(after as usize)
            .map(|(i, value)| (i as u64 + 1, value))
    });
    assert_eq!(discovered.iter().next().unwrap().value, &original[0]);
    assert_eq!(
        raw, original,
        "attempt/mtime/marker bytes survive hint eviction and process replacement"
    );
}

#[test]
fn bounded_visibility() {
    let mut state = Scheduler::new();
    seed(&mut state, Source::Inventory, &[1, 2], 0);
    let before = state.visibility(sec(299));
    assert_eq!(before.cursors, [0, 0, 2]);
    assert_eq!(before.scan_age, [None, None, Some(sec(299))]);
    assert_eq!(before.revisit_lag, [None; 3]);
    let p = page(&mut state, Source::Inventory, &[1, 2], 300);
    let after = state.visibility(sec(301));
    assert_eq!(after.revisit_lag, [None, None, Some(sec(300))]);
    assert_eq!(after.scan_age[2], Some(sec(1)));
    assert_eq!(after.page, 2);
    drop(p);
    assert_eq!(state.visibility(sec(301)).page, 0);
}

#[test]
fn shared_process_state_serializes_concurrent_permits() {
    let before = invocation_count();
    with_scheduler(|state| {
        *state = Scheduler::new();
        seed(state, Source::Row, &[1, 2, 3, 4, 5], 0);
    });
    let barrier = Arc::new(Barrier::new(5));
    let handles: Vec<_> = (1..=5)
        .map(|id| {
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                with_scheduler(|state| {
                    state.begin_round(sec(30));
                    state.probe_cached(key(Source::Row, id), sec(30)).is_some()
                })
            })
        })
        .collect();
    let admitted = handles
        .into_iter()
        .map(|handle| handle.join().expect("probe thread"))
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(admitted, 4);
    with_scheduler(|state| assert_eq!(state.visibility(sec(30)).in_flight, 4));
    assert_eq!(invocation_count(), before + 2);
}
