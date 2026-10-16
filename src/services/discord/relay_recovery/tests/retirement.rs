use super::*;
use crate::services::discord::relay_recovery::tests::orphan_token_finish::queued;
use crate::services::provider::CancelToken;
use poise::serenity_prelude::UserId;
use std::sync::atomic::Ordering;

static HOST_BARRIERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<ChannelId, Arc<tokio::sync::Barrier>>>,
> = std::sync::LazyLock::new(Default::default);

pub(super) async fn host_barrier(channel: ChannelId) {
    let barrier = HOST_BARRIERS.lock().unwrap().get(&channel).cloned();
    if let Some(barrier) = barrier {
        barrier.wait().await;
        barrier.wait().await;
    }
}

struct HostBarrier(ChannelId, Arc<tokio::sync::Barrier>);

impl HostBarrier {
    fn new(channel: ChannelId) -> Self {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        HOST_BARRIERS
            .lock()
            .unwrap()
            .insert(channel, barrier.clone());
        Self(channel, barrier)
    }
}

impl Drop for HostBarrier {
    fn drop(&mut self) {
        HOST_BARRIERS.lock().unwrap().remove(&self.0);
    }
}

#[tokio::test]
async fn legacy_orphan_host_check_finishes_exact_token_and_preserves_queue_for_every_source() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let sources = [
        RelayRecoveryApplySource::ProbeAutoHeal,
        RelayRecoveryApplySource::StallWatchdog,
        RelayRecoveryApplySource::Manual,
    ];
    for (source_index, source) in sources.into_iter().enumerate() {
        let channel = ChannelId::new(6_325_420_200 + source_index as u64);
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        let token = Arc::new(CancelToken::new());
        let anchor = MessageId::new(channel.get() + 100);
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                UserId::new(7),
                anchor,
            )
            .await
        );
        for id in 1..=3 {
            crate::services::discord::mailbox_enqueue_intervention(
                &shared,
                &provider,
                channel,
                queued(anchor.get() + id),
            )
            .await;
        }
        shared.restart.global_active.store(1, Ordering::Relaxed);
        let decision = super::super::run_relay_recovery_at(
            &registry,
            Some(provider.as_str()),
            channel.get(),
            false,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .expect("plan orphan episode")
        .decision;
        assert_eq!(
            decision.action,
            RelayRecoveryActionKind::ClearOrphanPendingToken
        );
        let barrier = HostBarrier::new(channel);
        let apply =
            apply_relay_recovery_decision(&registry, &shared, &provider, &decision, None, source);
        let during_host_check = async {
            barrier.1.wait().await;
            barrier.1.wait().await;
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(apply, during_host_check)
        })
        .await
        .expect("host barrier must complete");
        assert_eq!(result.status, "applied");
        assert!(result.removed_mailbox_token);
        let after = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        assert!(after.cancel_token.is_none());
        assert_eq!(after.intervention_queue.len(), 3);
        assert!(token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(shared.restart.deferred_hook_channels.contains_key(&channel));
    }
}
