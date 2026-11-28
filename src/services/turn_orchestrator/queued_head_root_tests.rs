use std::sync::{Arc, atomic::Ordering};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use crate::services::discord::health::HealthRegistry;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::queue::force_purge_channel_mailbox;
use crate::services::turn_lifecycle::TurnLifecycleTarget;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use crate::services::turn_orchestrator::queued_head::{
    QueueIdentity,
    root_test_support::{TOKEN, bytes, ctx, human, path, queue, reserve_unknown, root, run},
};

/// Without a live actor, force purge removes only namespaces whose file holds no head or
/// submitted source and parses cleanly.
#[test]
fn an_actorless_force_purge_keeps_held_or_unreadable_snapshots() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_171);
        let item = r#"{"author_id":7,"message_id":71,"text":"h"}"#;
        let identity = r#"{"primary":71,"queued_generation":5,"sources":[{"message_id":71,"queued_generation":5}]}"#;
        let unknown = format!(
            r#"{{"version":1,"items":[{item}],"head_injection":{{"identity":{identity},"original":{item},"phase":"unknown"}}}}"#
        );
        let terminal = format!(r#"{{"version":1,"items":[],"terminal_identities":[{identity}]}}"#);
        let base = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
        let file = |token: &str| {
            base.join("claude")
                .join(token)
                .join(format!("{}.json", channel.get()))
        };
        for (token, body) in [
            ("unknown", unknown.as_str()),
            ("terminal", terminal.as_str()),
            ("legacy", "[]"),
            ("torn", r#"{"version":1,"items":["#),
            ("broken", "[{"),
        ] {
            std::fs::create_dir_all(file(token).parent().unwrap()).unwrap();
            std::fs::write(file(token), body).unwrap();
        }
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let purged = force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, None).await;
        assert_eq!(purged.map(|counts| counts.disk_files_removed), Some(1));
        let kept = ["unknown", "terminal", "legacy", "torn"].map(|token| file(token).exists());
        assert_eq!(kept, [true, true, false, true]);
        assert_eq!(std::fs::read(file("broken")).unwrap(), b"[{");
    });
}

#[test]
fn s2a_force_purge_refusal_does_not_finalize_a_cancelled_holder() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_281);
        let handle = ChannelMailboxRegistry::default().handle(channel);
        let token = Arc::new(CancelToken::new());
        handle
            .restore_active_turn(token.clone(), UserId::new(9), MessageId::new(901))
            .await;
        let head = human(281, 7, "H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        reserve_unknown(&handle, &head).await;
        assert!(handle.enqueue(human(282, 8, "B"), ctx()).await.enqueued);
        token.cancelled.store(true, Ordering::Relaxed);
        let before = bytes(channel);
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let key = format!("claude/{TOKEN}/test:inactive-fixture");
        assert!(
            force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, Some(&key))
                .await
                .is_none()
        );
        assert_eq!(bytes(channel), before);
        assert_eq!(queue(&handle).await, [(281, "H".into()), (282, "B".into())]);
        assert!(Arc::ptr_eq(
            handle.snapshot().await.cancel_token.as_ref().unwrap(),
            &token
        ));
    });
}

#[test]
fn s2a_actorless_purge_preserves_valid_unknown_and_terminal_namespaces() {
    let _root = root();
    run(async {
        let channel = ChannelId::new(6_813_291);
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(channel);
        let head = human(291, 7, "H");
        assert!(handle.enqueue(head.clone(), ctx()).await.enqueued);
        reserve_unknown(&handle, &head).await;
        let unknown = bytes(channel);
        let terminal = serde_json::to_vec(&serde_json::json!({
            "version": 1, "items": [], "terminal_identities": [QueueIdentity::of(&head)],
        }))
        .unwrap();
        let base = path(channel).parent().unwrap().parent().unwrap().to_owned();
        let file = |token: &str| base.join(token).join(format!("{}.json", channel.get()));
        let fixtures: [(&str, &[u8]); 4] = [
            ("terminal", &terminal),
            ("broken", b"[{"),
            ("torn", b"{\"version\":1,\"items\":["),
            ("legacy", b"[]"),
        ];
        for (token, body) in fixtures {
            std::fs::create_dir_all(file(token).parent().unwrap()).unwrap();
            std::fs::write(file(token), body).unwrap();
        }
        registry.remove_fixture_for_test(channel);
        assert!(ChannelMailboxRegistry::global_handle(channel).is_none());
        let target = TurnLifecycleTarget {
            provider: Some(ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let purged = force_purge_channel_mailbox(None::<&Arc<HealthRegistry>>, &target, None)
            .await
            .unwrap();
        assert_eq!(purged.disk_files_removed, 1);
        assert_eq!(bytes(channel), unknown);
        for (token, body) in fixtures.into_iter().filter(|(token, _)| *token != "legacy") {
            assert_eq!(std::fs::read(file(token)).unwrap(), body);
        }
        assert!(!file("legacy").exists());
    });
}
