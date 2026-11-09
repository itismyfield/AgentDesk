use super::*;

#[derive(Default)]
pub(super) struct Pending {
    channels: std::sync::Mutex<HashMap<ChannelId, HashMap<MessageId, TurnViewIdentity>>>,
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn target(message: u64) -> TurnViewTarget {
        TurnViewTarget::intake_user_message(
            ChannelId::new(100_000_000_079_920),
            MessageId::new(message),
        )
    }

    #[tokio::test]
    async fn confirmed_cold_injection_has_exactly_one_plus_op() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        let reconciler = &shared.turn_view_reconciler;
        reconciler
            .note_confirmed_injection(
                &shared,
                &shared.provider,
                target(100_000_000_079_911),
                TurnViewIdentity::Test("intake"),
            )
            .await;
        assert_eq!(
            reconciler
                .ops()
                .iter()
                .map(|op| (op.emoji, op.add))
                .collect::<Vec<_>>(),
            [('➕', true)]
        );
    }

    #[tokio::test]
    async fn confirmed_persisted_injection_clears_legacy_and_live_queue_markers() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        for version in [1, 2, 3, 4] {
            let target = target(100_000_000_079_920 + u64::from(version));
            let path = TurnViewReconciler::persisted_target_path(target).unwrap();
            let record = serde_json::json!({"version":version,"provider":shared.provider.as_str(),
                "kind":target.kind.as_str(),"channel_id":target.channel_id.get(),"message_id":target.message_id.get(),
                "owner_generation":1,"owner_turn_id":"legacy-input","applied":"queued_merged",
                "identity_label":target.kind.identity_label(),"token_hash":shared.token_hash});
            runtime_store::atomic_write(&path, &record.to_string()).unwrap();
            let reconciler = TurnViewReconciler::default();
            let old = match version {
                2 => &['➕', '⏳'][..],
                4 => &['📬'][..],
                _ => &['➕'][..],
            };
            reconciler
                .ops
                .lock()
                .unwrap()
                .extend(old.iter().map(|emoji| TestReactionOp {
                    target,
                    emoji: *emoji,
                    add: true,
                    identity: target.kind.identity_label().into(),
                }));
            reconciler
                .note_confirmed_injection(
                    &shared,
                    &shared.provider,
                    target,
                    TurnViewIdentity::Test("intake"),
                )
                .await;
            let mut visible = std::collections::BTreeSet::new();
            for op in reconciler.ops() {
                if op.add {
                    visible.insert(op.emoji);
                } else {
                    visible.remove(&op.emoji);
                }
            }
            assert_eq!(
                visible.into_iter().collect::<Vec<_>>(),
                ['➕'],
                "version {version}"
            );
            assert!(!path.exists());
            for emoji in old {
                assert!(
                    reconciler
                        .ops()
                        .iter()
                        .any(|op| op.emoji == *emoji && !op.add),
                    "version {version} must remove its owned old marker"
                );
            }
        }
    }
}

async fn retry_delivery<F, Fut>(mut attempt: F) -> TurnViewDelivery
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = TurnViewDelivery>,
{
    for _ in 0..2 {
        let delivery = attempt().await;
        if delivery != TurnViewDelivery::Failed {
            return delivery;
        }
    }
    TurnViewDelivery::Failed
}

impl TurnViewReconciler {
    /// Leaves an absorption receipt, with completion tracking only for supervised channels.
    pub(in crate::services::discord) async fn note_confirmed_injection(
        &self,
        shared: &SharedData,
        provider: &crate::services::provider::ProviderKind,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) {
        if !reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            return;
        }
        retry_delivery(|| {
            self.confirmed_injection_attempt(shared, provider, target, identity.clone())
        })
        .await;
    }

    async fn confirmed_injection_attempt(
        &self,
        shared: &SharedData,
        provider: &crate::services::provider::ProviderKind,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) -> TurnViewDelivery {
        let current = self
            .targets
            .get(&target)
            .map(|entry| entry.clone())
            .or_else(|| self.load_persisted_target(target, shared, "busy_inject_reaction"));
        let tracked = current.is_some();
        let owner = current.map(|entry| entry.owner).unwrap_or_else(|| {
            turn_view_owner_for_message(
                target.channel_id,
                target.message_id,
                shared.restart.current_generation,
            )
        });
        for emoji in queue_reactions::QUEUE_PENDING_REACTION_EMOJIS
            .into_iter()
            .filter(|_| tracked)
        {
            let delivery = self
                .note_queue_marker_removed_delivery(
                    shared,
                    target,
                    owner.clone(),
                    identity.clone(),
                    emoji,
                    "busy_inject_reaction",
                )
                .await;
            if !delivery.delivered() {
                return delivery;
            }
        }
        if crate::services::discord::turn_presence::supervisor::observes(
            provider,
            target.channel_id.get(),
        ) {
            self.note_busy_injected(shared, target, identity).await
        } else {
            self.note_untracked_reaction(
                shared,
                target,
                identity,
                '➕',
                true,
                "busy_inject_reaction",
            )
            .await
        }
    }

    #[cfg(test)]
    pub(in crate::services::discord) fn pending_injected_count(&self, channel: ChannelId) -> usize {
        self.injected
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&channel)
            .map_or(0, HashMap::len)
    }

    /// Remembers immediate-injection messages separately from turn and queue markers.
    pub(in crate::services::discord) async fn note_busy_injected(
        &self,
        shared: &SharedData,
        target: TurnViewTarget,
        identity: TurnViewIdentity,
    ) -> TurnViewDelivery {
        if !reaction_lifecycle::is_real_discord_message_id(target.message_id) {
            return TurnViewDelivery::Delivered;
        }
        let delivery = self
            .note_untracked_reaction(
                shared,
                target,
                identity.clone(),
                '➕',
                true,
                "busy_inject_reaction",
            )
            .await;
        self.injected
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(target.channel_id)
            .or_default()
            .insert(target.message_id, identity);
        delivery
    }

    /// Takes the list synchronously at the idle observation; later arrivals keep their own list.
    pub(in crate::services::discord) fn clear_busy_injected(
        &self,
        shared: Arc<SharedData>,
        channel: ChannelId,
    ) {
        let messages = self
            .injected
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&channel)
            .unwrap_or_default();
        if messages.is_empty() {
            return;
        }
        super::super::task_supervisor::spawn_observed("busy_inject_reaction_cleanup", async move {
            for (message, identity) in messages {
                let target = TurnViewTarget::intake_user_message(channel, message);
                retry_delivery(|| {
                    shared.turn_view_reconciler.note_untracked_reaction(
                        &shared,
                        target,
                        identity.clone(),
                        '✅',
                        true,
                        "busy_inject_idle",
                    )
                })
                .await;
            }
        });
    }
}
