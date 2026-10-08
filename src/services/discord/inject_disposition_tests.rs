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

/// A writer held between its read and its write keeps a second writer out until it lands, so
/// neither entry is lost.
#[test]
fn a_writer_between_read_and_write_holds_the_second_writer_off() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = isolated("flock");
    let channel = ChannelId::new(5_845_441);
    let (first, second) = (MessageId::new(5_845_442), MessageId::new(5_845_443));
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (reached, go) = test_support::park_before_write(&ring_path(&provider).unwrap());
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
    let b = write(second);
    std::thread::sleep(Duration::from_millis(300));
    let b_waited = !b.is_finished();
    go.send(()).unwrap();
    a.join().unwrap().unwrap();
    b.join().unwrap().unwrap();
    let mut ids = ring_ids(&provider);
    ids.sort_unstable();
    assert_eq!((b_waited, ids), (true, vec![first.get(), second.get()]));
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
