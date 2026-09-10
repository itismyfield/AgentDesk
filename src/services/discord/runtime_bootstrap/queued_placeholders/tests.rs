use super::*;
use crate::services::discord::{
    make_shared_data_for_tests, queue_dispatch, queued_placeholders_store,
};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::UserId;

fn queued(head: MessageId, sources: &[MessageId]) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: head,
        queued_generation: 1,
        source_message_ids: sources.to_vec(),
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("queued {}", head.get()),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn seed(shared: &Arc<SharedData>, c: ChannelId, ids: &[MessageId]) {
    let ctx = queue_dispatch::persistence_context(shared, &shared.provider, c);
    for id in ids {
        assert!(
            shared
                .mailbox(c)
                .enqueue(queued(*id, &[*id]), ctx.clone())
                .await
                .enqueued
        );
    }
}
fn assert_disk(shared: &SharedData) {
    let disk =
        queued_placeholders_store::load_queued_placeholders(&shared.provider, &shared.token_hash);
    let map: std::collections::HashMap<_, _> = shared
        .queued
        .queued_placeholders
        .iter()
        .map(|e| (*e.key(), *e.value()))
        .collect();
    assert_eq!(disk, map);
}
#[derive(Default)]
struct Recorder(std::sync::Mutex<Vec<MessageId>>);
impl StalePlaceholderDeleter for Recorder {
    fn delete<'a>(
        &'a self,
        _: ChannelId,
        card: MessageId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(card);
            Ok(())
        })
    }
}

#[tokio::test]
async fn restore_rows_and_cleanup_preserve_current_owners() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (u, v, x, y) = (
        MessageId::new(11),
        MessageId::new(12),
        MessageId::new(91),
        MessageId::new(92),
    );
    for case in 1..=7 {
        let c = ChannelId::new(514100 + case);
        let ids = if case == 5 { vec![u] } else { vec![u, v] };
        seed(&shared, c, &ids).await;
        let mut live = vec![((c, u), x)];
        let mut stale = std::collections::HashSet::new();
        if case == 1 {
            live.push(((c, v), y));
        }
        if case == 2 {
            live.clear();
            stale.insert(c);
            queued_placeholders_store::save_channel_queued_placeholders(
                &shared.provider,
                &shared.token_hash,
                c,
                &[(u, x)],
            );
        }
        if case == 3 {
            let ctx = queue_dispatch::persistence_context(&shared, &shared.provider, c);
            assert_eq!(
                shared
                    .mailbox(c)
                    .take_next_soft(ctx)
                    .await
                    .intervention
                    .unwrap()
                    .message_id,
                u
            );
        }
        if case >= 4 {
            let lock = shared.queued_placeholders_persist_lock(c);
            let _guard = lock.lock().await;
            shared.insert_queued_placeholder_locked(
                c,
                if case == 4 { v } else { u },
                if case == 4 { x } else { y },
            );
        }
        if case == 7 {
            live = vec![((c, u), y)];
        }
        let late = install_restored_queued_placeholders(&shared, live, &stale).await;
        assert_disk(&shared);
        let get = |owner| {
            shared
                .queued
                .queued_placeholders
                .get(&(c, owner))
                .map(|v| *v)
        };
        match case {
            1 => {
                assert!(late.is_empty());
                assert_eq!(get(u), Some(x));
                assert_eq!(get(v), Some(y));
            }
            2 => {
                assert!(late.is_empty());
                assert_eq!(get(u), None);
            }
            3 => {
                assert_eq!(late, vec![(c, u, x)]);
                assert_eq!(get(u), None);
            }
            4 => {
                assert_eq!(late, vec![(c, u, x)]);
                assert_eq!(get(u), None);
                assert_eq!(get(v), Some(x));
            }
            5 | 6 => {
                assert_eq!(late, vec![(c, u, x)]);
                assert_eq!(get(u), Some(y));
            }
            7 => {
                assert!(late.is_empty());
                assert_eq!(get(u), Some(y));
            }
            _ => unreachable!(),
        }
        let recorder = Recorder::default();
        delete_stale_queued_placeholder_cards_with(&recorder, &shared, &late).await;
        assert_eq!(
            *recorder.0.lock().unwrap(),
            if case == 5 { vec![x] } else { vec![] }
        );
        if case == 6 {
            assert_eq!(get(u), Some(y));
            assert_eq!(get(v), Some(x));
        }
        assert_disk(&shared);
    }
}

#[tokio::test]
async fn restore_waits_for_channel_lock_and_keeps_other_channel() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let shared = make_shared_data_for_tests();
    let (c, other, u, x) = (
        ChannelId::new(514120),
        ChannelId::new(514121),
        MessageId::new(1),
        MessageId::new(2),
    );
    seed(&shared, c, &[u]).await;
    shared.insert_queued_placeholder_locked(other, u, x);
    let lock = shared.queued_placeholders_persist_lock(c);
    let guard = lock.lock().await;
    let stale = std::collections::HashSet::new();
    let mut install = Box::pin(install_restored_queued_placeholders(
        &shared,
        vec![((c, u), x)],
        &stale,
    ));
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(install.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    // A second request to this actor is a FIFO barrier for an unguarded
    // helper's snapshot request; poll again after its reply is available.
    mailbox_snapshot(&shared, c).await;
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(install.as_mut().poll(cx)))
            .await
            .is_pending()
    );
    assert!(!shared.queued.queued_placeholders.contains_key(&(c, u)));
    assert_disk(&shared);
    drop(guard);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), install)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(shared.queued.queued_placeholders.len(), 2);
    assert_disk(&shared);
}

#[test]
fn recovery_consumer_routes_uninstalled_candidates_to_cleanup() {
    // Lexical auxiliary only: this is not an HTTP execution proof.
    let source = include_str!("../recovery_flush.rs");
    let region =
        &source[source.find("let filter_outcome =").unwrap()..source.find("// P1-2:").unwrap()];
    assert!(region.contains("install_restored_queued_placeholders("));
    assert!(region.contains("stale_cards_to_delete.extend(uninstalled)"));
    assert!(!region.contains(".insert("));
    assert!(!region.contains("persist_channel_from_map"));
}
