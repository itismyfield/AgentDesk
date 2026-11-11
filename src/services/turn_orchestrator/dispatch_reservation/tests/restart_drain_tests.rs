use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;
use serde_json::{Value, json};

const TOKEN: &str = "discord_c2adc5461e12b989";

fn context() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, TOKEN, None)
}

fn write_v2(root: &Path, channel: ChannelId, items: Value) -> (std::path::PathBuf, String) {
    let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
    assert!(path.starts_with(root));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bytes = serde_json::to_string_pretty(&json!({
        "version": 2,
        "items": items,
        "head_injection": null,
        "retained_inputs": [],
        "terminal_identities": [],
        "namespace": format!("claude/{TOKEN}"),
        "admission_receipts": (0..17).map(|id| format!("tk:camping:{id}")).collect::<Vec<_>>(),
        "admission_sequence": 17
    }))
    .unwrap();
    std::fs::write(&path, &bytes).unwrap();
    (path, bytes)
}

#[cfg(unix)]
#[test]
fn restart_drain_all_skips_receipts_only_v2_after_marker_only_mailbox_creation() {
    let root = TestRuntimeRootGuard::new();
    run_async(async {
        let registry = ChannelMailboxRegistry::default();
        let camping = ChannelId::new(1_527_859_708_180_828_350);
        let channels = [
            camping,
            ChannelId::new(1_527_859_700_677_349_437),
            ChannelId::new(1_490_141_479_707_086_938),
            ChannelId::new(1_479_671_298_497_183_835),
            ChannelId::new(1_559_670_200_419_352_696),
            ChannelId::new(1_509_350_490_461_180_105),
            ChannelId::new(1_473_922_824_350_601_297),
            ChannelId::new(1_509_350_778_043_895_902),
        ];
        let files: Vec<_> = channels
            .iter()
            .map(|channel| write_v2(root.path(), *channel, json!([])))
            .collect();
        assert!(
            super::super::super::pending_queue_persistence::v2_channels(
                &ProviderKind::Claude,
                TOKEN
            )
            .ready
            .is_empty()
        );
        for channel in channels {
            // mailbox_clear_recovery_marker creates a handle and clears only the recovery flag.
            let handle = registry.handle(channel);
            handle.clear_recovery_marker_or_refused().await.unwrap();
            handle.recovery_done().mark_done();
            if channel != camping {
                handle.v2_ensure_channel(&context(), channel).await.unwrap();
            }
            assert_eq!(
                handle.v2_request(|owner| Ok(owner.is_some())).await,
                Ok(channel != camping)
            );
            let snapshot = handle.snapshot().await;
            assert!(snapshot.cancel_token.is_none());
            assert!(snapshot.intervention_queue.is_empty());
            assert!(snapshot.pending_user_dispatch.is_none());
            assert!(snapshot.recovery_started_at.is_none());
        }
        let files: Vec<_> = files
            .into_iter()
            .map(|(path, bytes)| {
                let adopted = std::fs::read_to_string(&path).unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&adopted).unwrap(),
                    serde_json::from_str::<Value>(&bytes).unwrap()
                );
                (path, adopted)
            })
            .collect();
        let drain = registry
            .restart_drain_all(&ProviderKind::Claude, TOKEN, &dashmap::DashMap::new())
            .await;
        assert_eq!(drain.queued_count, 0);
        assert!(drain.persistence_errors.is_empty(), "{drain:?}");
        for (path, bytes) in files {
            assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
        }
    });
}

#[test]
fn restart_drain_all_refuses_legacy_backlog_beside_v2() {
    let root = TestRuntimeRootGuard::new();
    run_async(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_884_001);
        let handle = registry.handle(channel);
        assert!(
            handle
                .enqueue(
                    single_source_intervention(MessageId::new(78_841), "legacy input"),
                    context()
                )
                .await
                .enqueued
        );
        let (path, bytes) = write_v2(root.path(), channel, json!([]));
        handle.v2_ensure_channel(&context(), channel).await.unwrap();
        assert_eq!(
            handle.v2_request(|owner| Ok(owner.is_none())).await,
            Ok(true)
        );

        let drain = registry
            .restart_drain_all(&ProviderKind::Claude, TOKEN, &dashmap::DashMap::new())
            .await;
        assert_eq!(drain.queued_count, 0);
        assert_eq!(drain.persistence_errors.len(), 1);
        assert_eq!(drain.persistence_errors[0].channel_id, channel);
        assert!(
            drain.persistence_errors[0].error.contains("legacy"),
            "{drain:?}"
        );
        assert_eq!(
            handle.snapshot().await.intervention_queue[0]
                .message_id
                .get(),
            78_841
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
    });
}

#[test]
fn restart_drain_all_leaves_unowned_v2_items_uncounted_and_unchanged() {
    let root = TestRuntimeRootGuard::new();
    run_async(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_884_002);
        let (path, bytes) = write_v2(
            root.path(),
            channel,
            json!([{
                "author_id": 7, "message_id": 78842, "channel_id": channel.get(),
                "text": "V2 owner input", "queued_generation": 17
            }]),
        );
        registry
            .handle(channel)
            .clear_recovery_marker_or_refused()
            .await
            .unwrap();
        let drain = registry
            .restart_drain_all(&ProviderKind::Claude, TOKEN, &dashmap::DashMap::new())
            .await;
        assert_eq!(drain.queued_count, 0);
        assert!(drain.persistence_errors.is_empty(), "{drain:?}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
    });
}

#[test]
fn restart_drain_all_reports_malformed_v2_instead_of_skipping_it() {
    let root = TestRuntimeRootGuard::new();
    run_async(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_884_003);
        let (path, bytes) = write_v2(root.path(), channel, json!("invalid items"));
        registry
            .handle(channel)
            .clear_recovery_marker_or_refused()
            .await
            .unwrap();
        let drain = registry
            .restart_drain_all(&ProviderKind::Claude, TOKEN, &dashmap::DashMap::new())
            .await;
        assert_eq!(drain.queued_count, 0);
        assert_eq!(drain.persistence_errors.len(), 1);
        assert!(is_queue_unread(&drain.persistence_errors[0].error));
        assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
    });
}

#[test]
fn restart_drain_all_excludes_idle_closed_receipts_only_v2_mailbox() {
    let root = TestRuntimeRootGuard::new();
    run_async(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_884_004);
        let (path, bytes) = write_v2(root.path(), channel, json!([]));
        registry
            .handle(channel)
            .clear_recovery_marker_or_refused()
            .await
            .unwrap();
        assert_eq!(
            registry.remove_idle_entry(channel).await,
            MailboxPurgeOutcome::Removed
        );
        assert!(registry.peek(channel).is_none());
        let drain = registry
            .restart_drain_all(&ProviderKind::Claude, TOKEN, &dashmap::DashMap::new())
            .await;
        assert_eq!(drain.queued_count, 0);
        assert!(drain.persistence_errors.is_empty(), "{drain:?}");
        assert_eq!(std::fs::read_to_string(path).unwrap(), bytes);
    });
}

#[test]
fn restart_drain_all_still_persists_and_counts_legacy_items() {
    let root = TestRuntimeRootGuard::new();
    run_async(async {
        let registry = ChannelMailboxRegistry::default();
        let channel = ChannelId::new(7_884_005);
        assert!(
            registry
                .handle(channel)
                .enqueue(
                    single_source_intervention(MessageId::new(78_845), "legacy input"),
                    context(),
                )
                .await
                .enqueued
        );
        let path = pending_queue_file_path(&ProviderKind::Claude, TOKEN, channel).unwrap();
        assert!(path.starts_with(root.path()));
        std::fs::remove_file(&path).unwrap();
        let drain = registry
            .restart_drain_all(&ProviderKind::Claude, TOKEN, &dashmap::DashMap::new())
            .await;
        assert_eq!(drain.queued_count, 1);
        assert!(drain.persistence_errors.is_empty(), "{drain:?}");
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved[0]["message_id"], 78_845);
    });
}
