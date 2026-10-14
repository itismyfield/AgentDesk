//! The real automatic take over inputs that carry a busy-retry record.

use super::*;

const SIX_MINUTES_MS: u64 = 6 * 60 * 1_000;

fn now_ms() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    u64::try_from(now.unwrap().as_millis()).unwrap()
}

fn write_record(f: &Fixture, id: u64, json: &str) {
    let root = discord::runtime_store::discord_busy_followup_retries_root().unwrap();
    let dir = root
        .join(f.provider.as_str())
        .join(f.channel.get().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{id}.json")), json).unwrap();
}

/// Takes once and gives a taken input's dispatch back; returns what was taken.
async fn take_once(f: &Fixture) -> Option<u64> {
    let taken = f.take().await;
    let (Some(input), Some(lease)) = (taken.intervention, taken.dispatch_lease) else {
        return None;
    };
    let id = input.message_id;
    discord::mailbox_abandon_unclaimed_dispatch_after_success(
        &f.shared,
        &f.provider,
        f.channel,
        id,
        lease,
    )
    .await;
    Some(id.get())
}

/// A retry that waited in the queue keeps its count but is not capped by age, so it still runs
/// before later input; only a spent count, a latched cap or an unreadable record changes that.
#[tokio::test]
async fn the_automatic_take_runs_a_waiting_retry_and_passes_only_spent_ones() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let legacy = |count: u32, first: u64| {
        format!(
            r#"{{"notice_message_id":9,"busy_retry_count":{count},"first_busy_retry_at_ms":{first}}}"#
        )
    };
    let latched = format!(
        r#"{{"notice_message_id":9,"busy_retry_count":2,"first_busy_retry_at_ms":{},"active_retry_ms":300000,"capped":true,"notice_bound":true}}"#,
        now_ms()
    );
    // (record of the head A, failures recorded after it, which input the take runs)
    let cases = [
        (legacy(1, now_ms() - SIX_MINUTES_MS), 0, Some(1)),
        (legacy(6, now_ms()), 0, Some(2)),
        (legacy(5, now_ms()), 1, Some(2)),
        (latched, 0, Some(2)),
        ("{".to_string(), 0, None),
    ];
    for (index, (record, failures, expected)) in cases.into_iter().enumerate() {
        let f = Fixture::new(8_068_970 + 10 * index as u64).await;
        let (a, b) = (f.channel.get() + 1, f.channel.get() + 2);
        write_record(&f, a, &record);
        for _ in 0..failures {
            discord::busy_followup_retry_store::record_busy_retry(
                &f.provider,
                f.channel.get(),
                a,
                9,
            )
            .unwrap();
        }
        f.enqueue(item(a)).await;
        f.enqueue(item(b)).await;
        let expected = expected.map(|offset| f.channel.get() + offset);
        assert_eq!(take_once(&f).await, expected, "case {index}");
        let left: Vec<u64> = [a, b]
            .into_iter()
            .filter(|id| Some(*id) != expected)
            .collect();
        assert_eq!(f.ids().await, left, "case {index}");
        let snapshot = discord::mailbox_snapshot(&f.shared, f.channel).await;
        let sources = snapshot
            .intervention_queue
            .iter()
            .map(|i| i.source_message_ids.clone());
        let own: Vec<Vec<MessageId>> = left.iter().map(|id| vec![MessageId::new(*id)]).collect();
        assert_eq!(sources.collect::<Vec<_>>(), own, "case {index}");
        f.shared.mailboxes.remove_fixture_for_test(f.channel);
    }
}

/// A merged input whose source spent its budget is parked even though its own id is unspent, and
/// a later input is neither merged into it nor refused as a duplicate of its text.
#[tokio::test]
async fn a_spent_source_parks_its_merged_input_and_later_input_stays_separate() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for (index, merge) in [false, true].into_iter().enumerate() {
        let f = Fixture::new(8_068_990 + 10 * index as u64).await;
        let (primary, source, later_id) = (
            f.channel.get() + 1,
            f.channel.get() + 2,
            f.channel.get() + 3,
        );
        let store = |id, notice| {
            discord::busy_followup_retry_store::bind_notice_if_absent(
                &f.provider,
                f.channel.get(),
                id,
                notice,
            )
            .unwrap();
        };
        store(primary, 9);
        store(source, 8);
        for _ in 0..discord::busy_followup_retry_store::MAX_BUSY_RETRY_COUNT {
            discord::busy_followup_retry_store::record_busy_retry(
                &f.provider,
                f.channel.get(),
                source,
                8,
            )
            .unwrap();
        }
        let mut merged = item(primary);
        merged.source_message_ids = vec![MessageId::new(primary), MessageId::new(source)];
        merged.merge_consecutive = merge;
        let mut later = item(later_id);
        later.merge_consecutive = merge;
        if !merge {
            later.text = merged.text.clone();
        }
        f.enqueue(merged).await;
        f.enqueue(later).await;
        assert_eq!(f.ids().await, [primary, later_id], "merge={merge}");
        assert_eq!(take_once(&f).await, Some(later_id), "merge={merge}");
        assert_eq!(f.ids().await, [primary], "merge={merge}");
        f.shared.mailboxes.remove_fixture_for_test(f.channel);
    }
}
