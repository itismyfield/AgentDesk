//! The clear reply counts each discarded waiting source once, and says so when it cannot count.

use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId, UserId};

use super::super::clear_persist_failure_tests::seed_session;
use super::super::{ClearReport, clear_channel_session_state};
use crate::services::discord::{SharedData, make_shared_data_for_tests};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, Intervention, InterventionMode, QueuePersistenceContext,
    save_channel_pending_dispatch_marker,
};

const PROVIDER: ProviderKind = ProviderKind::Claude;

fn input(id: MessageId, sources: &[MessageId]) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: id,
        queued_generation: crate::services::discord::runtime_store::process_generation(),
        source_message_ids: sources.to_vec(),
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("waiting before the clear {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

async fn clear(shared: &Arc<SharedData>, channel: ChannelId) -> ClearReport {
    let http = Arc::new(serenity::Http::new(""));

    let clear = clear_channel_session_state(&http, shared, &PROVIDER, channel, "/clear");
    clear.await.expect("a clear always commits")
}

fn count_line(report: &ClearReport) -> String {
    report.reply().lines().nth(1).unwrap_or_default().to_owned()
}

/// A merged entry of three sources, a dispatch marker left from before a restart and the running
/// turn: the merged sources and the marker's own source count once each, the running turn never.
#[tokio::test]
async fn clear_reply_counts_each_waiting_source_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(1_479_671_305_341_700_000);
    seed_session(&shared, channel).await;
    let id = |n: u64| MessageId::new(channel.get() + n);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(
        start(
            &shared,
            channel,
            Arc::new(CancelToken::new()),
            UserId::new(1),
            id(1)
        )
        .await
    );
    let persistence = QueuePersistenceContext::new(&PROVIDER, &shared.token_hash, None);
    let merged = input(id(2), &[id(2), id(3), id(4)]);
    let command = input(id(9), &[id(9)]);
    for item in [merged, command] {
        let mailbox = shared.mailbox(channel);
        let queued = mailbox.enqueue(item, persistence.clone()).await;
        assert!(queued.enqueued, "{:?}", queued.refusal_reason);
    }
    let marker = input(id(5), &[id(5), id(3), id(1)]);
    save_channel_pending_dispatch_marker(&PROVIDER, &shared.token_hash, channel, &marker, None)
        .expect("a dispatch marker from before a restart");

    let report = clear(&shared, channel).await;

    assert_eq!(count_line(&report), "대기 메시지 5건을 비웠어요.");
    let report = report.excluding(id(9));
    assert_eq!(
        count_line(&report),
        "대기 메시지 4건을 비웠어요.",
        "the clear command itself was never waiting"
    );
}

/// A mailbox that cannot answer leaves the count unknown, never zero.
#[tokio::test]
async fn clear_reply_says_the_count_is_unknown_when_the_mailbox_cannot_answer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(1_479_671_305_341_700_100);
    seed_session(&shared, channel).await;
    shared.mailboxes.insert_unreachable_for_test(channel);

    let report = clear(&shared, channel).await;

    assert_eq!(count_line(&report), "대기 메시지 수를 확인하지 못했어요.");
    assert!(
        report.reply().starts_with("세션을 초기화했어요.\n"),
        "{}",
        report.reply()
    );
}

/// Two merged inputs started as one turn whose claim left the dispatch marker on disk: the clear
/// counts neither, and counts a later input that waits behind the turn.
#[tokio::test]
async fn clear_reply_never_counts_the_running_turns_absorbed_source() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    for (n, waiting) in [(0u64, 0usize), (1, 1)] {
        let channel = ChannelId::new(1_479_671_305_341_700_200 + n);
        seed_session(&shared, channel).await;
        let id = |k: u64| MessageId::new(channel.get() + 10 + k);
        let mailbox = shared.mailbox(channel);
        let persistence = QueuePersistenceContext::new(&PROVIDER, &shared.token_hash, None);
        for k in [1, 2] {
            let item = Intervention {
                merge_consecutive: true,
                ..input(id(k), &[id(k)])
            };
            let queued = mailbox.enqueue(item, persistence.clone()).await;
            assert!(queued.enqueued, "{:?}", queued.refusal_reason);
        }
        let taken = mailbox.take_next_soft(persistence.clone()).await;
        let merged = taken.intervention.expect("the merged input is dequeued");
        let mut sources = merged.source_message_ids.clone();
        sources.sort();
        assert_eq!(sources, [id(1), id(2)], "the two inputs merged");
        ChannelMailboxRegistry::fail_next_marker_settle(channel);
        let token = Arc::new(CancelToken::new());
        let owner = UserId::new(7);
        let start = mailbox.try_start_turn_with_persistence(
            token,
            owner,
            merged.message_id,
            persistence.clone(),
        );
        assert!(start.await.started);
        let live = mailbox.snapshot().await;
        let absorbed = if merged.message_id == id(1) {
            id(2)
        } else {
            id(1)
        };
        assert_eq!(live.active_user_message_id, Some(merged.message_id));
        assert_eq!(live.active_absorbed_source_ids, [absorbed]);
        assert_eq!(live.pending_user_dispatch, None);
        assert!(live.intervention_queue.is_empty());
        let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let marker = root.join(PROVIDER.as_str()).join(&shared.token_hash);
        assert!(
            marker.join(format!("{channel}.dispatch")).exists(),
            "the claim left the marker"
        );
        if waiting == 1 {
            let later = Intervention {
                author_id: UserId::new(8),
                ..input(id(3), &[id(3)])
            };
            let queued = mailbox.enqueue(later, persistence.clone()).await;
            assert!(queued.enqueued, "{:?}", queued.refusal_reason);
        }

        let report = clear(&shared, channel).await;

        let expected = format!("대기 메시지 {waiting}건을 비웠어요.");
        assert_eq!(count_line(&report), expected, "waiting={waiting}");
    }
}

/// A dispatch marker the clear cannot read or parse leaves the count unknown, never zero, and the
/// clear still runs.
#[tokio::test]
async fn clear_reply_says_the_count_is_unknown_when_the_marker_is_unreadable() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = make_shared_data_for_tests();
    for (n, directory) in [(0u64, false), (1, true)] {
        let channel = ChannelId::new(1_479_671_305_341_700_300 + n);
        seed_session(&shared, channel).await;
        let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let dir = root.join(PROVIDER.as_str()).join(&shared.token_hash);
        let marker = dir.join(format!("{channel}.dispatch"));
        std::fs::create_dir_all(&dir).unwrap();
        if directory {
            std::fs::create_dir(&marker).unwrap();
        } else {
            std::fs::write(&marker, "{").unwrap();
        }
        let live = shared.mailbox(channel).snapshot().await;
        assert!(live.intervention_queue.is_empty() && live.pending_user_dispatch.is_none());

        let report = clear(&shared, channel).await;

        let reply = report.reply();
        assert!(reply.starts_with("세션을 초기화했어요.\n"), "{reply}");
        let case = if directory {
            "unreadable"
        } else {
            "unparsable"
        };
        assert_eq!(
            count_line(&report),
            "대기 메시지 수를 확인하지 못했어요.",
            "{case}"
        );
    }
}
