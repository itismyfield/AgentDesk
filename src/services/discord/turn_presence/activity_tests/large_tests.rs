use super::*;
use crate::services::discord::{mailbox_snapshot, mailbox_take_next_automatic_intervention};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::{MessageId, UserId};

fn large_transcript(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut file = std::io::BufWriter::new(file);
    let row = format!("{}\n", json!({"type":"summary","summary":"x".repeat(8192)}));
    let mut written = 0;
    while written <= MAX_TRANSCRIPT_BYTES {
        file.write_all(row.as_bytes()).unwrap();
        written += row.len() as u64;
    }
    writeln!(file, "{}", prompt("large-current")).unwrap();
    writeln!(file, "{}", end()).unwrap();
    file.flush().unwrap();
    assert!(std::fs::metadata(path).unwrap().len() > MAX_TRANSCRIPT_BYTES);
}

fn queued(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: Vec::new(),
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("eligible {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_large_default_limits_reads_idle_and_dequeues_exact_head() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let path = root.path().join("large.jsonl");
    large_transcript(&path);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(69_181_001);
    let _turn = bind_turn_mode_transcript(&shared, &provider, channel.get(), "s1-large", &path);
    let queue = vec![queued(69_181_101), queued(69_181_102)];
    let context = crate::services::discord::queue_persistence_context(&shared, &provider, channel);
    shared.mailbox(channel).replace_queue(queue, context).await;
    assert_eq!(
        settled_reason(&shared, &provider, channel.get()).await,
        "closed"
    );
    let take = mailbox_take_next_automatic_intervention(&shared, &provider, channel).await;
    assert_eq!(
        take.intervention.as_ref().map(|i| i.message_id.get()),
        Some(69_181_101)
    );
    let snapshot = mailbox_snapshot(&shared, channel).await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(snapshot.intervention_queue[0].message_id.get(), 69_181_102);
}

#[tokio::test(flavor = "current_thread")]
async fn s1_unbound_restored_owner_without_core_is_not_idle() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(69_181_002);
    let provider = ProviderKind::Claude;
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
    shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session("s1-restored", channel);
    assert!(shared.tmux_watchers.channel_binding(&channel).is_none());
    assert!(!shared.core.lock().await.sessions.contains_key(&channel));
    let observed = activity_now(&shared, &provider, channel).await;
    assert_eq!(
        observed.activity,
        Activity::Unknown,
        "a watcher-less channel needs complete absence evidence"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1e_large_automatic_promotion_reaches_exact_pending_and_disk_head() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    for (index, large) in [false, true].into_iter().enumerate() {
        let channel = ChannelId::new(69_182_200 + index as u64);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let path = root.path().join(format!("{channel}.jsonl"));
        if large {
            large_transcript(&path);
        } else {
            write(&path, &[prompt("control"), end()]);
        }
        let _turn = bind_turn_mode_transcript(
            &shared,
            &provider,
            channel.get(),
            &format!("s1-promotion-{channel}"),
            &path,
        );
        let _ = settled_reason(&shared, &provider, channel.get()).await;
        let first = channel.get() + 1_000_000;
        let context =
            crate::services::discord::queue_persistence_context(&shared, &provider, channel);
        shared
            .mailbox(channel)
            .replace_queue(vec![queued(first), queued(first + 1)], context)
            .await;
        let taken = mailbox_take_next_automatic_intervention(&shared, &provider, channel).await;
        assert_eq!(
            taken.intervention.as_ref().map(|i| i.message_id.get()),
            Some(first),
            "large={large}: actual automatic promotion never consumed its eligible head"
        );
        assert!(taken.dispatch_lease.is_some());
        assert!(taken.persistence_error.is_none());
        let snapshot = mailbox_snapshot(&shared, channel).await;
        assert_eq!(
            snapshot.pending_user_dispatch.map(|id| id.get()),
            Some(first)
        );
        assert!(snapshot.pending_user_dispatch_lease_held_by_caller);
        assert_eq!(
            snapshot
                .intervention_queue
                .iter()
                .map(|i| i.message_id.get())
                .collect::<Vec<_>>(),
            vec![first + 1]
        );
        let (disk, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &provider,
            &shared.token_hash,
            channel,
        );
        assert_eq!(
            disk.iter().map(|i| i.message_id.get()).collect::<Vec<_>>(),
            vec![first + 1]
        );
        let pending = crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &provider,
            &shared.token_hash,
            channel,
        )
        .unwrap();
        assert_eq!(pending.0.message_id.get(), first);
    }
}
