use super::*;
use crate::services::turn_orchestrator::pending_queue_persistence::{post_save_fault, save_fault};
use crate::services::turn_orchestrator::queued_head::{
    HeadOutcome, HeadSettle, HeadTicket, QueueIdentity, QueuedHeadClaim,
    downgrade_channel_snapshot, snapshot_json,
};
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, InterventionMode, SourceMessageQueuedGeneration,
    SourceMessageTextSegment,
};
use poise::serenity_prelude::{MessageId, UserId};
use serde_json::{Value, json};

const TOKEN: &str = "b3b1a-exit-debt";
const EPISODE: &str = "episode-b3b1a";

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(future)
}

fn human(id: u64, author: u64, text: &str, generation: u64) -> Intervention {
    let source = SourceMessageQueuedGeneration::user_instruction(MessageId::new(id), generation);
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: generation,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: vec![source],
        source_text_segments: Vec::new(),
        text: text.to_string(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: true,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn exit(item: &Intervention, kind: QueueExitKind) -> QueueExitEvent {
    QueueExitEvent {
        intervention: item.clone(),
        kind,
    }
}

fn path(channel: ChannelId) -> std::path::PathBuf {
    pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap()
}

fn bytes(channel: ChannelId) -> Option<Vec<u8>> {
    fs::read(path(channel)).ok()
}

fn disk(channel: ChannelId) -> Value {
    serde_json::from_slice(&bytes(channel).expect("queue file")).unwrap()
}

fn value<T: serde::Serialize>(item: &T) -> Value {
    serde_json::to_value(item).unwrap()
}

fn read(channel: ChannelId) -> Vec<Value> {
    let debts = read_queue_exit_debts_checked(&ProviderKind::Claude, TOKEN, channel).unwrap();
    debts.iter().map(value).collect()
}

/// Everything feedback reads from an exit: identity, every source's generation, segments, text.
fn payload(event: &QueueExitEvent) -> Value {
    let item = &event.intervention;
    let generations = item.source_message_queued_generations.iter();
    let segments = item.source_text_segments.iter();
    json!({
        "kind": format!("{:?}", event.kind),
        "message_id": item.message_id.get(),
        "author": item.author_id.get(),
        "generation": item.queued_generation,
        "sources": item.source_message_ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
        "generations": generations
            .map(|g| (g.message_id.get(), g.queued_generation, g.preserve_on_cancel))
            .collect::<Vec<_>>(),
        "segments": segments
            .map(|s| (s.message_id.get(), s.text.clone()))
            .collect::<Vec<_>>(),
        "text": item.text,
    })
}

/// A state whose queue the real writer has already published.
fn seeded(channel: ChannelId, queue: Vec<Intervention>) -> ChannelMailboxState {
    let mut state = ChannelMailboxState::default();
    state.intervention_queue = queue;
    persist_queue(&mut state, channel, &ctx()).unwrap();
    state
}

fn ids(state: &ChannelMailboxState) -> Vec<u64> {
    let queue = state.intervention_queue.iter();
    queue.map(|item| item.message_id.get()).collect()
}

async fn reserve(handle: &ChannelMailboxHandle, head: &Intervention) -> HeadTicket {
    let identity = QueueIdentity::of(head);
    match handle.claim_queued_head(identity, ctx()).await {
        QueuedHeadClaim::Reserved(ticket) => ticket,
        _ => panic!("head {} was not reserved", head.message_id),
    }
}

async fn settle(handle: &ChannelMailboxHandle, ticket: HeadTicket, outcome: HeadOutcome) -> bool {
    let settled = handle.settle_queued_head(ticket, outcome, ctx()).await;
    matches!(settled, HeadSettle::Committed(_))
}

/// A debt-free snapshot keeps its exact bytes, and a merged intervention and an item an overflow
/// evicted behind a reserved head both come back with the same id, episode, kind and payload.
#[test]
fn debt_codec_round_trips_merged_and_reserved_overflow_exits() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let identity = QueueIdentity {
        primary: 7,
        queued_generation: 5,
        sources: vec![
            crate::services::turn_orchestrator::queued_head::SourceIdentity {
                message_id: 7,
                queued_generation: 5,
            },
        ],
    };
    let mut meta = SnapshotMeta {
        terminal_identities: vec![identity],
        ..SnapshotMeta::default()
    };
    assert_eq!(snapshot_json(Vec::new(), None).unwrap(), "[]");
    let v1 = "{\n  \"version\": 1,\n  \"items\": [],\n  \"head_injection\": null,\n  \"terminal_identities\": [\n    {\n      \"primary\": 7,\n      \"queued_generation\": 5,\n      \"sources\": [\n        {\n          \"message_id\": 7,\n          \"queued_generation\": 5\n        }\n      ]\n    }\n  ]\n}";
    assert_eq!(snapshot_json(Vec::new(), Some(&meta)).unwrap(), v1);

    let channel = ChannelId::new(5_340_101);
    let merged = block_on(async {
        let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(5_340_100));
        assert!(
            handle
                .enqueue(human(11, 7, "first", 3), ctx())
                .await
                .enqueued
        );
        assert!(
            handle
                .enqueue(human(12, 7, "second", 4), ctx())
                .await
                .enqueued
        );
        handle.snapshot().await.intervention_queue.remove(0)
    });
    assert_eq!(
        merged.source_message_ids.len(),
        2,
        "premise: the actor merged"
    );
    let mut queue = vec![human(20, 1, "reserved head", 5)];
    queue.extend((21..=50).map(|id| human(id, id, "tail", 5)));
    let overflow = super::super::super::queue_enqueue::enqueue_behind(
        &mut queue,
        human(51, 51, "late", 5),
        None,
        None,
        true,
    );
    let evicted = &overflow.queue_exit_events[0];
    assert_eq!(
        evicted.intervention.message_id.get(),
        21,
        "premise: evicted behind the head"
    );

    let events = [exit(&merged, QueueExitKind::Superseded), evicted.clone()];
    let debts: Vec<_> = events
        .iter()
        .map(|e| QueueExitDebt::new(EPISODE, e, channel))
        .collect();
    let mut state = seeded(channel, vec![merged.clone()]);
    commit_queue_exit(&mut state, channel, &ctx(), Vec::new(), debts.clone()).unwrap();

    let restored = read_queue_exit_debts_checked(&ProviderKind::Claude, TOKEN, channel).unwrap();
    assert_eq!(restored.len(), 2);
    for ((debt, event), stored) in debts.iter().zip(&events).zip(&restored) {
        assert_eq!(stored.event_id, debt.event_id);
        assert_eq!(stored.episode, EPISODE);
        assert_eq!(stored.state, QueueExitDebtState::Unacked);
        assert_eq!(payload(&stored.event()), payload(event));
    }
    assert_eq!(restored[1].kind, QueueExitDebtKind::Overflow);
    meta.queue_exit_debts = restored;
    let with_debt = snapshot_json(Vec::new(), Some(&meta)).unwrap();
    assert!(with_debt.starts_with(&v1[..v1.len() - 2]) && with_debt.contains("queue_exit_debts"));
}

/// A new reader finds the same debt after the writer is gone, and reading twice consumes nothing.
#[test]
fn a_debt_is_rebuilt_after_a_restart_and_reads_consume_nothing() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_340_201);
    let (x, y) = (human(31, 7, "cancelled", 5), human(32, 8, "kept", 5));
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), channel);
    let mut state = seeded(channel, vec![x.clone(), y.clone()]);
    commit_queue_exit(&mut state, channel, &ctx(), vec![y], vec![debt.clone()]).unwrap();
    drop(state);

    let committed = bytes(channel);
    let first = read(channel);
    assert_eq!(first, [value(&debt)]);
    assert_eq!(read(channel), first);
    let scanned = scan_queue_exit_debts_checked(&ProviderKind::Claude, TOKEN)
        .ok()
        .unwrap();
    assert_eq!(scanned.len(), 1);
    assert_eq!(bytes(channel), committed, "reading is not consuming");
    let restored = &scanned[0].1[0];
    assert_eq!(
        payload(&restored.event()),
        payload(&exit(&x, QueueExitKind::Cancelled))
    );
}

/// The last item leaving with a debt keeps the file, and the namespace scan finds the channel
/// that the item recovery skips.
#[test]
fn an_emptied_queue_keeps_its_debt_file_and_the_scan_finds_it() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (channel, plain) = (ChannelId::new(5_340_301), ChannelId::new(5_340_302));
    let x = human(41, 7, "last", 5);
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Superseded), channel);
    let mut state = seeded(channel, vec![x]);
    seeded(plain, vec![human(42, 8, "plain", 5)]);
    commit_queue_exit(&mut state, channel, &ctx(), Vec::new(), vec![debt.clone()]).unwrap();

    assert_eq!(disk(channel)["items"], json!([]));
    let recovery = super::super::load_pending_queues_checked(&ProviderKind::Claude, TOKEN);
    assert!(
        !recovery.queues.contains_key(&channel),
        "premise: items-only recovery skips it"
    );
    let scanned = scan_queue_exit_debts_checked(&ProviderKind::Claude, TOKEN)
        .ok()
        .unwrap();
    let found: Vec<_> = scanned
        .iter()
        .map(|(c, d)| (*c, d.iter().map(value).collect()))
        .collect();
    assert_eq!(found, [(channel, vec![value(&debt)])]);
}

/// A save that fails before the file is touched leaves the previous queue on disk and in memory.
#[test]
fn a_failed_save_leaves_the_previous_queue_and_no_debt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_340_401);
    let (x, y) = (human(51, 7, "x", 5), human(52, 8, "y", 5));
    let mut state = seeded(channel, vec![x.clone(), y.clone()]);
    let before = bytes(channel);
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), channel);
    save_fault::fail_next(channel);
    let failed = commit_queue_exit(&mut state, channel, &ctx(), vec![y.clone()], vec![debt]);

    assert!(matches!(failed, Err(QueueExitCommitError::NotCommitted(_))));
    assert_eq!(bytes(channel), before);
    assert!(read(channel).is_empty());
    assert_eq!(ids(&state), [51, 52]);
    assert!(state.queued_head.meta.queue_exit_debts.is_empty());
    let retry = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), channel);
    commit_queue_exit(&mut state, channel, &ctx(), vec![y], vec![retry]).unwrap();
    assert_eq!(read(channel).len(), 1);
}

/// An error returned after the snapshot was published keeps that debt, and memory follows the
/// file; a file holding neither snapshot is a fail-closed error that rewrites nothing.
#[test]
fn an_error_after_publication_keeps_the_published_debt() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_340_501);
    let (x, y) = (human(61, 7, "x", 5), human(62, 8, "y", 5));
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Superseded), channel);
    let mut state = seeded(channel, vec![x.clone(), y.clone()]);
    post_save_fault::fail_next(channel);
    commit_queue_exit(&mut state, channel, &ctx(), vec![y], vec![debt.clone()]).unwrap();

    assert_eq!(read(channel), [value(&debt)]);
    assert_eq!(ids(&state), [62]);
    assert_eq!(state.queued_head.meta.queue_exit_debts.len(), 1);
    state.intervention_queue.push(human(63, 9, "z", 5));
    persist_queue(&mut state, channel, &ctx()).unwrap();
    assert_eq!(read(channel), [value(&debt)]);

    let torn = ChannelId::new(5_340_502);
    let mut state = seeded(torn, vec![x.clone()]);
    fs::write(path(torn), b"{\"version\":1,\"items\":[").unwrap();
    let other = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Superseded), torn);
    let failed = commit_queue_exit(&mut state, torn, &ctx(), Vec::new(), vec![other]);
    assert!(matches!(
        failed,
        Err(QueueExitCommitError::Indeterminate(_))
    ));
    assert_eq!(bytes(torn).unwrap(), b"{\"version\":1,\"items\":[");
    assert_eq!(ids(&state), [61]);
    assert!(state.queued_head.meta.queue_exit_debts.is_empty());
}

/// A head reservation and its settlement write around a debt without dropping it.
#[test]
fn a_debt_survives_a_head_claim_and_settlement() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_340_601);
    let (x, head) = (human(71, 7, "superseded", 5), human(72, 8, "head", 5));
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Superseded), channel);
    let mut state = seeded(channel, vec![x, head.clone()]);
    commit_queue_exit(
        &mut state,
        channel,
        &ctx(),
        vec![head.clone()],
        vec![debt.clone()],
    )
    .unwrap();
    drop(state);

    block_on(async {
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(
            handle
                .enqueue(human(73, 9, "behind", 5), ctx())
                .await
                .enqueued
        );
        let ticket = reserve(&handle, &head).await;
        assert_eq!(disk(channel)["head_injection"]["phase"], "reserved");
        assert_eq!(disk(channel)["queue_exit_debts"], json!([value(&debt)]));
        let HeadSettle::Committed(Some(ticket)) = handle
            .settle_queued_head(ticket, HeadOutcome::MayHaveSent, ctx())
            .await
        else {
            panic!("MayHaveSent was not committed");
        };
        assert!(settle(&handle, ticket, HeadOutcome::Injected).await);
        assert!(disk(channel)["head_injection"].is_null());
        assert_eq!(disk(channel)["terminal_identities"][0]["primary"], 72);

        let restart = ChannelMailboxRegistry::default().handle(channel);
        assert!(
            restart
                .enqueue(human(74, 10, "after", 5), ctx())
                .await
                .enqueued
        );
    });
    assert_eq!(disk(channel)["terminal_identities"][0]["primary"], 72);
    assert_eq!(read(channel), [value(&debt)]);
}

/// A malformed snapshot, a debt naming another channel, a failed listing and a failed entry are
/// each an error; the channels that could be read are still reported.
#[test]
fn unreadable_debts_are_errors_not_absent_debts() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (good, torn, copy) = (
        ChannelId::new(5_340_701),
        ChannelId::new(5_340_702),
        ChannelId::new(5_340_703),
    );
    let x = human(81, 7, "x", 5);
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), good);
    let mut state = seeded(good, vec![x]);
    commit_queue_exit(&mut state, good, &ctx(), Vec::new(), vec![debt.clone()]).unwrap();
    fs::write(path(torn), b"{\"version\":1,\"items\":[").unwrap();
    let provider = ProviderKind::Claude;

    assert!(read_queue_exit_debts_checked(&provider, TOKEN, torn).is_err());
    let scan = scan_queue_exit_debts_checked(&provider, TOKEN)
        .err()
        .unwrap();
    assert_eq!(scan.errors.len(), 1, "{:?}", scan.errors);
    assert!(scan.errors[0].contains(&torn.get().to_string()));
    assert_eq!(
        scan.found.len(),
        1,
        "the readable channel is still reported"
    );
    fs::remove_file(path(torn)).unwrap();

    fs::copy(path(good), path(copy)).unwrap();
    let foreign = read_queue_exit_debts_checked(&provider, TOKEN, copy)
        .err()
        .unwrap();
    assert!(foreign.contains("another channel"), "{foreign}");
    fs::remove_file(path(copy)).unwrap();

    let blocked = path(good).parent().unwrap().with_file_name("listing-fails");
    fs::write(&blocked, b"a file where the token directory belongs").unwrap();
    let listing = scan_queue_exit_debts_checked(&provider, "listing-fails")
        .err()
        .unwrap();
    assert!(
        listing.errors[0].starts_with("read_dir"),
        "{:?}",
        listing.errors
    );
    assert!(
        scan_queue_exit_debts_checked(&provider, "absent")
            .ok()
            .unwrap()
            .is_empty()
    );

    let entries = [
        Err(std::io::Error::other("injected entry error")),
        Ok(path(good)),
    ];
    let entry = scan_entries(entries.into_iter()).err().unwrap();
    assert!(entry.errors[0].contains("injected entry error"));
    let found: Vec<_> = entry
        .found
        .iter()
        .map(|(c, d)| (*c, d.iter().map(value).collect()))
        .collect();
    assert_eq!(found, [(good, vec![value(&debt)])]);
}

/// An unacknowledged debt keeps a file with no items from force purge, downgrade, a write that
/// drops it, and the actor's clear and purge.
#[test]
fn debt_only_files_refuse_purge_downgrade_and_debt_dropping_writes() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_340_801);
    let x = human(91, 7, "x", 5);
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), channel);
    let mut state = seeded(channel, vec![x]);
    commit_queue_exit(&mut state, channel, &ctx(), Vec::new(), vec![debt.clone()]).unwrap();
    let before = bytes(channel);

    let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
        provider: Some(ProviderKind::Claude),
        channel_id: Some(channel),
        tmux_name: String::new(),
    };
    let purged = block_on(crate::services::queue::force_purge_channel_mailbox(
        None, &target, None,
    ));
    assert_eq!(purged.map(|counts| counts.disk_files_removed), Some(0));
    assert_eq!(bytes(channel), before);
    let downgraded = downgrade_channel_snapshot(&ProviderKind::Claude, TOKEN, channel, |_| true);
    assert!(downgraded.unwrap_err().contains("queue-exit debt"));
    assert_eq!(bytes(channel), before);
    state.queued_head.meta.queue_exit_debts.clear();
    let dropped = persist_queue(&mut state, channel, &ctx()).unwrap_err();
    assert!(dropped.contains("queue-exit debt"), "{dropped}");
    assert_eq!(bytes(channel), before);
    drop(state);

    block_on(async {
        let handle = ChannelMailboxRegistry::default().handle(channel);
        assert!(handle.enqueue(human(92, 8, "y", 5), ctx()).await.enqueued);
        assert_eq!(disk(channel)["items"][0]["message_id"], 92);
        let cleared = handle.clear(ctx()).await;
        assert!(cleared.persistence_error.is_none());
        assert_eq!(disk(channel)["items"], json!([]));
        assert!(handle.enqueue(human(93, 9, "z", 5), ctx()).await.enqueued);
        let _ = handle.purge_queue(ctx(), true).await;
    });
    assert_eq!(read(channel), [value(&debt)]);
}

/// A message id that exits twice, and two exits of one merged intervention, keep distinct event
/// ids across a re-read; recording an id twice is refused.
#[test]
fn event_ids_stay_distinct_for_a_requeued_message_and_a_merged_source() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_340_901);
    let x = human(101, 7, "again", 5);
    let mut merged = human(103, 8, "a\nb", 5);
    merged.source_message_ids = vec![MessageId::new(102), MessageId::new(103)];
    merged.source_message_queued_generations = vec![
        SourceMessageQueuedGeneration::user_instruction(MessageId::new(102), 4),
        SourceMessageQueuedGeneration::user_instruction(MessageId::new(103), 5),
    ];
    merged.source_text_segments = vec![
        SourceMessageTextSegment::new(MessageId::new(102), "a"),
        SourceMessageTextSegment::new(MessageId::new(103), "b"),
    ];
    let mut state = seeded(channel, vec![x.clone(), merged.clone()]);
    let first = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), channel);
    commit_queue_exit(
        &mut state,
        channel,
        &ctx(),
        vec![merged.clone()],
        vec![first.clone()],
    )
    .unwrap();
    let parts = [
        QueueExitDebt::new(EPISODE, &exit(&merged, QueueExitKind::Superseded), channel),
        QueueExitDebt::new(EPISODE, &exit(&merged, QueueExitKind::Superseded), channel),
    ];
    let requeued = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), channel);
    let mut later = parts.to_vec();
    later.push(requeued);
    commit_queue_exit(&mut state, channel, &ctx(), Vec::new(), later).unwrap();

    let stored: Vec<String> = read(channel)
        .iter()
        .map(|d| d["event_id"].as_str().unwrap().into())
        .collect();
    assert_eq!(stored.len(), 4);
    let mut unique = stored.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 4, "{stored:?}");
    assert_eq!(stored[0], first.event_id);
    let again: Vec<String> = read(channel)
        .iter()
        .map(|d| d["event_id"].as_str().unwrap().into())
        .collect();
    assert_eq!(again, stored, "a re-read mints nothing");

    let before = bytes(channel);
    let twice = commit_queue_exit(&mut state, channel, &ctx(), Vec::new(), vec![first]);
    assert!(matches!(twice, Err(QueueExitCommitError::NotCommitted(_))));
    assert_eq!(bytes(channel), before);
    assert_eq!(state.queued_head.meta.queue_exit_debts.len(), 4);
}

/// The handback reads the queue as a legacy array, so it fails on a debt-bearing snapshot and
/// leaves the file as it was rather than rewriting it without the debt.
#[test]
fn handback_refuses_a_debt_bearing_snapshot_without_rewriting_it() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_341_001);
    let (x, y) = (human(111, 7, "x", 5), human(112, 8, "y", 5));
    let debt = QueueExitDebt::new(EPISODE, &exit(&x, QueueExitKind::Cancelled), channel);
    let mut state = seeded(channel, vec![x, y.clone()]);
    commit_queue_exit(&mut state, channel, &ctx(), vec![y], vec![debt.clone()]).unwrap();
    let before = bytes(channel);

    let root = pending_queue_root().unwrap();
    let destination = crate::services::turn_orchestrator::input_handback::Destination {
        root: root.parent().unwrap(),
        provider: &ProviderKind::Claude,
        token_hash: TOKEN,
        channel: channel.get(),
        authorized: true,
        active_sources: &[],
    };
    let input = json!({"author_id": 9, "message_id": 113, "text": "handed back", "channel_id": channel.get()});
    let handed = crate::services::turn_orchestrator::input_handback::enqueue(&destination, &input);
    let error = handed.unwrap_err().to_string();
    assert!(error.contains("expected a sequence"), "{error}");
    assert_eq!(bytes(channel), before);
    assert_eq!(read(channel), [value(&debt)]);
}

/// The test writer, which carries no metadata, refuses a V1 snapshot or an unparseable file
/// instead of replacing or removing it; a missing file and a legacy array are written as before.
#[test]
fn the_metadata_free_test_writer_refuses_v1_and_unparseable_files() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = ChannelId::new(5_341_101);
    let (x, y) = (human(121, 7, "x", 5), human(122, 8, "y", 5));
    let meta = SnapshotMeta {
        queue_exit_debts: vec![QueueExitDebt::new(
            EPISODE,
            &exit(&x, QueueExitKind::Cancelled),
            channel,
        )],
        ..SnapshotMeta::default()
    };
    let v1 = snapshot_json(Vec::new(), Some(&meta)).unwrap();
    fs::create_dir_all(path(channel).parent().unwrap()).unwrap();
    let save = |queue: &[Intervention]| {
        super::super::save_channel_queue(&ProviderKind::Claude, TOKEN, channel, queue, None)
    };
    for content in [v1.as_bytes(), b"{\"version\":1,\"items\":[".as_slice()] {
        fs::write(path(channel), content).unwrap();
        for queue in [vec![y.clone()], Vec::new()] {
            assert!(save(&queue).is_err());
            assert_eq!(bytes(channel).unwrap(), content);
        }
    }

    fs::remove_file(path(channel)).unwrap();
    save(&[y.clone()]).unwrap();
    assert_eq!(disk(channel)[0]["message_id"], 122);
    save(&[x, y]).unwrap();
    assert_eq!(disk(channel)[1]["message_id"], 122);
    save(&[]).unwrap();
    assert!(bytes(channel).is_none());
}
