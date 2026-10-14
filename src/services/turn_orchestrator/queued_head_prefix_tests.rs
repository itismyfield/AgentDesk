//! The actor's choice of the first ready input: parked inputs ahead of it move behind it in the
//! reserving write, and a stale, unreadable or untypable choice reserves nothing.

use super::*;

fn retry_record(channel: ChannelId, id: u64) -> PathBuf {
    let queues = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let retries = queues
        .parent()
        .unwrap()
        .join("discord_busy_followup_retries");
    retries.join(format!("claude/{}/{id}.json", channel.get()))
}

fn write_record(channel: ChannelId, id: u64, content: &str) {
    let record = retry_record(channel, id);
    std::fs::create_dir_all(record.parent().unwrap()).unwrap();
    std::fs::write(record, content).unwrap();
}

/// Spends the input's retry budget, as the store's own latch records it.
fn park(channel: ChannelId, id: u64) {
    let budget = serde_json::json!({ "notice_message_id": 9, "busy_retry_count": 3,
        "first_busy_retry_at_ms": 1, "active_retry_ms": 0, "capped": true, "notice_bound": true });
    write_record(channel, id, &budget.to_string());
}

async fn seed(channel: ChannelId, items: &[Intervention]) -> ChannelMailboxHandle {
    let handle = ChannelMailboxRegistry::default().handle(channel);
    for item in items {
        let mut item = item.clone();
        item.merge_consecutive = false;
        assert!(handle.enqueue(item, ctx()).await.enqueued);
    }
    handle
}

async fn ids(handle: &ChannelMailboxHandle) -> Vec<u64> {
    let queue = handle.snapshot().await.intervention_queue;
    queue.iter().map(|item| item.message_id.get()).collect()
}

fn disk_ids(channel: ChannelId) -> Vec<u64> {
    let disk = disk(channel);
    let items = if disk.is_array() {
        &disk
    } else {
        &disk["items"]
    };
    let id = |item: &serde_json::Value| item["message_id"].as_u64().unwrap();
    items.as_array().unwrap().iter().map(id).collect()
}

async fn claim(handle: &ChannelMailboxHandle, expected: &Intervention) -> &'static str {
    label(
        &handle
            .claim_queued_head(QueueIdentity::of(expected), ctx())
            .await,
    )
}

/// Parked inputs ahead of the first ready one move behind it, in their order, in the same write
/// that records the reservation; a second claim moves nothing, and a proven NotSent leaves the
/// input where the reservation put it.
#[test]
fn parked_inputs_ahead_move_behind_the_first_ready_one_in_the_reserving_write() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_601);
        let items = [
            human(61, 7, "parked one"),
            human(62, 7, "parked two"),
            human(63, 7, "ready head"),
            human(64, 7, "ready tail"),
        ];
        let handle = seed(channel, &items).await;
        park(channel, 61);
        park(channel, 62);
        assert_eq!(
            claim(&handle, &items[0]).await,
            "contended",
            "a parked input"
        );
        assert_eq!(
            claim(&handle, &items[3]).await,
            "contended",
            "not the first ready"
        );
        assert!(disk(channel).is_array(), "a refused claim wrote nothing");

        let ticket = reserve(&handle, &items[2]).await;
        assert_eq!(disk_ids(channel), [63, 61, 62, 64]);
        assert_eq!(disk(channel)["head_injection"]["phase"], "reserved");
        assert_eq!(disk(channel)["head_injection"]["identity"]["primary"], 63);
        assert_eq!(claim(&handle, &items[2]).await, "contended");
        assert_eq!(claim(&handle, &items[3]).await, "contended");
        assert_eq!(
            disk_ids(channel),
            [63, 61, 62, 64],
            "a held head moves nothing"
        );

        assert_eq!(
            settle(&handle, ticket, HeadOutcome::NotSent).await,
            "committed"
        );
        assert_eq!(ids(&handle).await, [63, 61, 62, 64]);
        assert!(disk(channel)["head_injection"].is_null());
        submit(&handle, &items[2]).await;
        assert_eq!(ids(&handle).await, [61, 62, 64]);
        submit(&handle, &items[3]).await;
        assert_eq!(
            ids(&handle).await,
            [61, 62],
            "parked inputs keep their order"
        );
        let texts: Vec<_> = queued(&handle).await.into_iter().map(|row| row.2).collect();
        assert_eq!(texts, ["parked one", "parked two"]);
        let terminal = &disk(channel)["terminal_identities"];
        assert_eq!(
            (&terminal[0]["primary"], &terminal[1]["primary"]),
            (&63.into(), &64.into())
        );
    });
}

/// The actor reads retry state again: a choice made stale by a parked input turning ready, or by
/// the chosen input turning parked or unreadable, reserves nothing and writes nothing.
#[test]
fn a_choice_gone_stale_before_the_claim_reserves_nothing() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_602);
        let items = [human(71, 7, "parked first"), human(72, 7, "chosen")];
        let handle = seed(channel, &items).await;
        park(channel, 71);
        // The caller chose 72 behind a parked 71; 71 becomes ready before the claim.
        std::fs::remove_file(retry_record(channel, 71)).unwrap();
        assert_eq!(claim(&handle, &items[1]).await, "contended");
        assert!(disk(channel).is_array());
        assert_eq!(ids(&handle).await, [71, 72]);

        // The caller chose 71 at the front; it is parked before the claim.
        park(channel, 71);
        assert_eq!(claim(&handle, &items[0]).await, "contended");
        // Or its retry record becomes unreadable: nothing at or behind it is taken.
        write_record(channel, 71, "{");
        assert_eq!(claim(&handle, &items[0]).await, "not_eligible");
        assert_eq!(claim(&handle, &items[1]).await, "not_eligible");
        assert!(disk(channel).is_array());
        assert_eq!(ids(&handle).await, [71, 72]);
    });
}

/// The first ready input decides alone. One that cannot be typed is not overtaken by a plain one
/// behind it, and an unreadable input holds back what follows it but not what precedes it.
#[test]
fn nothing_behind_an_untypable_or_unreadable_input_overtakes_it() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_603);
        let items = [
            human(81, 7, "parked"),
            human(82, 7, "!command"),
            human(83, 7, "plain tail"),
        ];
        let handle = seed(channel, &items).await;
        park(channel, 81);
        assert_eq!(claim(&handle, &items[1]).await, "not_eligible");
        assert_eq!(claim(&handle, &items[2]).await, "contended");
        assert!(disk(channel).is_array());
        assert_eq!(ids(&handle).await, [81, 82, 83]);

        let behind = ChannelId::new(6_813_604);
        let pair = [human(84, 7, "head"), human(85, 7, "unreadable tail")];
        let handle = seed(behind, &pair).await;
        write_record(behind, 85, "{");
        let _held = reserve(&handle, &pair[0]).await;

        let ahead = ChannelId::new(6_813_605);
        let pair = [human(86, 7, "unreadable head"), human(87, 7, "tail")];
        let handle = seed(ahead, &pair).await;
        write_record(ahead, 86, "{");
        assert_eq!(claim(&handle, &pair[1]).await, "not_eligible");
        assert!(disk(ahead).is_array());
    });
}

/// A reservation whose write fails restores the queue order and every record it held: the
/// submitted sources stay, no head is left in memory, and the next claim reserves as if the
/// failed one never ran.
#[test]
fn a_prefix_reservation_that_cannot_be_saved_restores_the_queue_and_its_records() {
    let _root = root();
    block_on(async {
        let channel = ChannelId::new(6_813_607);
        let items = [
            human(91, 7, "submitted earlier"),
            human(92, 7, "parked"),
            human(93, 7, "ready head"),
            human(94, 7, "ready tail"),
        ];
        let handle = seed(channel, &items).await;
        submit(&handle, &items[0]).await;
        park(channel, 92);
        let before = std::fs::read(queue_path(channel)).unwrap();
        assert_eq!(disk(channel)["terminal_identities"][0]["primary"], 91);

        fail_queue_saves(channel, 1);
        assert_eq!(claim(&handle, &items[2]).await, "persistence_failed");
        assert_eq!(ids(&handle).await, [92, 93, 94], "the move was undone");
        assert_eq!(std::fs::read(queue_path(channel)).unwrap(), before);

        let ticket = reserve(&handle, &items[2]).await;
        assert_eq!(disk_ids(channel), [93, 92, 94]);
        assert_eq!(disk(channel)["terminal_identities"][0]["primary"], 91);
        let HeadSettle::Committed(Some(ticket)) = handle
            .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
            .await
        else {
            panic!("MayHaveSent was not committed");
        };
        assert_eq!(
            settle(&handle, ticket, HeadOutcome::Unconfirmed).await,
            "unknown"
        );
        // Unknown is a barrier: the tail is never claimed in its place, here or after a restart.
        assert_eq!(claim(&handle, &items[3]).await, "blocked_unknown");
        let restart = ChannelMailboxRegistry::default().handle(channel);
        assert_eq!(claim(&restart, &items[3]).await, "blocked_unknown");
        assert_eq!(taken(&restart).await, None);
        assert_eq!(disk_ids(channel), [93, 92, 94]);
        assert_eq!(disk(channel)["head_injection"]["phase"], "unknown");
        assert_eq!(disk(channel)["terminal_identities"][0]["primary"], 91);
    });
}
