//! The injected-input table and ring: bounds, ages, the flock, and a corrupt ring.

use super::*;

/// A provider no other test writes, so cap and age checks see only their own entries.
fn isolated(name: &str) -> ProviderKind {
    ProviderKind::Unsupported(format!("inject-{name}"))
}

fn ring_ids(provider: &ProviderKind) -> Vec<u64> {
    let (entries, _) = load(&ring_path(provider).unwrap());
    entries.iter().map(|entry| entry.message_id).collect()
}

/// Past the per-provider cap the oldest terminal leaves the table, and the provider file still
/// answers a scan for it.
#[test]
fn the_table_drops_its_oldest_terminal_past_the_cap_and_the_ring_still_holds_it() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = isolated("cap");
    let (channel, now) = (ChannelId::new(5_845_401), Instant::now());
    let outcome = InjectionOutcome::Observed;
    let base = 5_845_402_000;
    for id in base + 1..=base + MEMORY_CAP as u64 + 1 {
        note_terminal(&provider, channel, Some(MessageId::new(id)), outcome, now);
    }
    let held = |id: u64| terminal(Some(&provider), MessageId::new(base + id), now).is_some();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let oldest = MessageId::new(base + 1);
    record_terminal(&provider, channel, oldest, outcome, now_ms).unwrap();
    let scanned = scan_view_at(&provider, now_ms, now)
        .terminal
        .get(&oldest.get())
        .copied();
    let observed = (held(1), held(2), held(MEMORY_CAP as u64 + 1), scanned);
    assert_eq!(observed, (false, true, true, Some(outcome)));
}

/// A table terminal answers until its age reaches the table TTL.
#[test]
fn a_table_terminal_answers_until_its_ttl() {
    let provider = isolated("memory-ttl");
    let (channel, at) = (ChannelId::new(5_845_411), Instant::now());
    let message = MessageId::new(5_845_412);
    note_terminal(
        &provider,
        channel,
        Some(message),
        InjectionOutcome::Unconfirmed,
        at,
    );
    let read = |later: Duration| terminal(Some(&provider), message, at + later);
    let edge = MEMORY_TTL - Duration::from_secs(1);
    assert_eq!(
        (read(edge), read(MEMORY_TTL)),
        (Some(InjectionOutcome::Unconfirmed), None)
    );
}

/// A ring terminal answers a scan until its age reaches the ring TTL, by the clock the caller
/// passes in.
#[test]
fn a_ring_terminal_answers_a_scan_until_its_ttl() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = isolated("disk-ttl");
    let (channel, message) = (ChannelId::new(5_845_421), MessageId::new(5_845_422));
    let at_ms = chrono::Utc::now().timestamp_millis();
    record_terminal(
        &provider,
        channel,
        message,
        InjectionOutcome::Observed,
        at_ms,
    )
    .unwrap();
    let scan = |now_ms: i64| scan_view_at(&provider, now_ms, Instant::now());
    let inside = scan(at_ms + DISK_TTL_MS - 1)
        .terminal
        .contains_key(&message.get());
    let outside = scan(at_ms + DISK_TTL_MS)
        .terminal
        .contains_key(&message.get());
    assert_eq!((inside, outside), (true, false));
}

/// A full ring drops its oldest entry for a new one, and a message recorded again keeps one
/// entry with its latest outcome.
#[test]
fn a_full_ring_drops_its_oldest_entry_and_a_rerecorded_message_keeps_one() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = isolated("disk-cap");
    let channel = ChannelId::new(5_845_431);
    let now_ms = chrono::Utc::now().timestamp_millis();
    let entries = (1..=DISK_CAP as u64)
        .map(|id| DiskEntry {
            message_id: id,
            channel_id: channel.get(),
            outcome: InjectionOutcome::Observed,
            at_epoch_ms: now_ms,
        })
        .collect();
    let path = ring_path(&provider).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string(&DiskRing { entries }).unwrap()).unwrap();
    let newest = MessageId::new(DISK_CAP as u64 + 1);
    record_terminal(
        &provider,
        channel,
        newest,
        InjectionOutcome::Observed,
        now_ms,
    )
    .unwrap();
    // Not the oldest left, so the cap drop cannot hide a second copy.
    let again = MessageId::new(3);
    record_terminal(
        &provider,
        channel,
        again,
        InjectionOutcome::Unconfirmed,
        now_ms,
    )
    .unwrap();
    let ids = ring_ids(&provider);
    let view = scan_view_at(&provider, now_ms, Instant::now());
    let observed = (
        ids.len(),
        ids.contains(&1),
        ids.iter().filter(|id| **id == again.get()).count(),
        view.terminal.get(&again.get()).copied(),
        ids.contains(&newest.get()),
    );
    let expected = (
        DISK_CAP,
        false,
        1,
        Some(InjectionOutcome::Unconfirmed),
        true,
    );
    assert_eq!(observed, expected);
}

/// A writer held between its read and its write keeps a second writer, seen reaching the flock,
/// from finishing until it lands, so neither entry is lost. Unix only: off Unix the record lock
/// takes no flock.
#[cfg(unix)]
#[test]
fn a_writer_between_read_and_write_holds_the_second_writer_off() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = isolated("flock");
    let channel = ChannelId::new(5_845_441);
    let (first, second) = (MessageId::new(5_845_442), MessageId::new(5_845_443));
    let now_ms = chrono::Utc::now().timestamp_millis();
    let path = ring_path(&provider).unwrap();
    let (reached, go) = test_support::park_before_write(&path);
    let write = |message: MessageId| {
        let provider = provider.clone();
        std::thread::spawn(move || {
            record_terminal(
                &provider,
                channel,
                message,
                InjectionOutcome::Observed,
                now_ms,
            )
        })
    };
    let a = write(first);
    reached
        .recv_timeout(Duration::from_secs(10))
        .expect("first writer parked");
    let at_lock = test_support::watch_lock(&path);
    let b = write(second);
    at_lock
        .recv_timeout(Duration::from_secs(10))
        .expect("second writer at the flock");
    // Unlocked, the second writer would land on its own well within this bound.
    let deadline = Instant::now() + Duration::from_millis(500);
    while !b.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let b_finished_first = b.is_finished();
    go.send(()).unwrap();
    a.join().unwrap().unwrap();
    b.join().unwrap().unwrap();
    let mut ids = ring_ids(&provider);
    ids.sort_unstable();
    assert_eq!(
        (b_finished_first, ids),
        (false, vec![first.get(), second.get()])
    );
}

/// An unreadable ring is replaced by one holding only the new entry.
#[test]
fn an_unreadable_ring_is_replaced_by_the_new_entry() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = isolated("corrupt");
    let path = ring_path(&provider).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not json").unwrap();
    let (channel, message) = (ChannelId::new(5_845_451), MessageId::new(5_845_452));
    let now_ms = chrono::Utc::now().timestamp_millis();
    record_terminal(
        &provider,
        channel,
        message,
        InjectionOutcome::Observed,
        now_ms,
    )
    .unwrap();
    assert_eq!(ring_ids(&provider), [message.get()]);
}

/// A parent's claim keeps a second claim and a thread's promotion off the message and reads as in
/// progress until its guard drops; the drop itself removes the claim.
#[test]
fn a_claim_holds_its_message_until_its_guard_drops_and_the_drop_removes_it() {
    let provider = isolated("claim");
    let (message, now) = (MessageId::new(5_845_461), Instant::now());
    let guard = claim_source(&provider, message, now).expect("the first claim");
    let held = (
        claim_source(&provider, message, now).is_none(),
        promote_thread(&provider, message, now),
        in_progress(Some(&provider), message, now),
    );
    drop(guard);
    let raw = test_support::source_entry(&provider, message.get());
    let after = (in_progress(Some(&provider), message, now), raw);
    let again = claim_source(&provider, message, now).is_some();
    assert_eq!(
        (held, after, again),
        ((true, false, true), (false, None), true)
    );
}

/// A claim whose owner vanished without its guard's cleanup blocks nothing: its lease is dead.
#[test]
fn a_claim_left_with_a_dead_lease_blocks_nothing() {
    let provider = isolated("dead-lease");
    let (message, now) = (MessageId::new(5_845_471), Instant::now());
    test_support::plant_dead_lease(&provider, message.get());
    let progress = in_progress(Some(&provider), message, now);
    let promoted = promote_thread(&provider, message, now);
    let other = MessageId::new(5_845_472);
    test_support::plant_dead_lease(&provider, other.get());
    let claimed = claim_source(&provider, other, now).is_some();
    assert_eq!((progress, promoted, claimed), (false, true, true));
}

/// A guard's drop removes only its own claim: a terminal noted while it held the message stays,
/// and a claim made after that terminal aged out is not removed by the older guard.
#[test]
fn a_dropped_guard_keeps_a_terminal_and_a_later_claim() {
    let provider = isolated("identity");
    let channel = ChannelId::new(5_845_480);
    let (ended, now) = (MessageId::new(5_845_481), Instant::now());
    let guard = claim_source(&provider, ended, now).expect("claim");
    note_terminal(
        &provider,
        channel,
        Some(ended),
        InjectionOutcome::Observed,
        now,
    );
    drop(guard);
    let kept = terminal(Some(&provider), ended, now);
    let replaced = MessageId::new(5_845_482);
    let older = claim_source(&provider, replaced, now).expect("claim");
    note_terminal(
        &provider,
        channel,
        Some(replaced),
        InjectionOutcome::Observed,
        now,
    );
    let later = now + MEMORY_TTL;
    let newer = claim_source(&provider, replaced, later).expect("a claim after the terminal aged");
    drop(older);
    let still = in_progress(Some(&provider), replaced, later);
    drop(newer);
    assert_eq!((kept, still), (Some(InjectionOutcome::Observed), true));
}

/// A thread's intake keeps a parent's claim off the message for the dedup window only, and never
/// reads as an injection in progress, so the thread's own mailbox takes it.
#[test]
fn a_thread_intake_blocks_a_parent_claim_until_it_expires() {
    let provider = isolated("thread-intake");
    let (message, now) = (MessageId::new(5_845_491), Instant::now());
    let promoted = promote_thread(&provider, message, now);
    let blocked = claim_source(&provider, message, now).is_none();
    let progress = in_progress(Some(&provider), message, now);
    let later = now + THREAD_INTAKE_TTL;
    let claimed = claim_source(&provider, message, later).is_some();
    assert_eq!(
        (promoted, blocked, progress, claimed),
        (true, true, false, true)
    );
}

/// A message noted again keeps one order slot, so repeats cannot grow the table's order.
#[test]
fn a_renoted_terminal_keeps_one_order_slot() {
    let provider = isolated("renote");
    let (channel, now) = (ChannelId::new(5_845_500), Instant::now());
    let message = MessageId::new(5_845_501);
    for outcome in [
        InjectionOutcome::Observed,
        InjectionOutcome::Unconfirmed,
        InjectionOutcome::Observed,
    ] {
        note_terminal(&provider, channel, Some(message), outcome, now);
    }
    let slots = test_support::order_slots(&provider, message.get());
    assert_eq!(
        (slots, terminal(Some(&provider), message, now)),
        (1, Some(InjectionOutcome::Observed))
    );
}
