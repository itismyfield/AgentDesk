//! Runtime mailbox ownership lookup and attributed turn cleanup.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardStopRuntimeResult {
    pub cleanup_path: &'static str,
    pub had_active_turn: bool,
    pub has_pending_queue: bool,
    pub runtime_session_cleared: bool,
}

impl Default for HardStopRuntimeResult {
    fn default() -> Self {
        Self {
            cleanup_path: "runtime_unavailable_fallback",
            had_active_turn: false,
            has_pending_queue: false,
            runtime_session_cleared: false,
        }
    }
}

pub(super) struct RuntimeChannelMatch {
    pub(super) provider: ProviderKind,
    pub(super) shared: Arc<SharedData>,
    pub(super) channel_id: ChannelId,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MailboxOwnership {
    None,
    PendingQueue,
    ActiveTurn,
}

async fn local_mailbox_ownership(shared: &SharedData, channel_id: ChannelId) -> MailboxOwnership {
    let Some(handle) = shared.mailbox_peek(channel_id) else {
        return MailboxOwnership::None;
    };

    let snapshot = handle.snapshot().await;
    // Snapshot/observation paths can materialize empty per-runtime mailboxes as
    // a side effect, so bare local-handle existence is not ownership evidence.
    if snapshot.cancel_token.is_some() {
        MailboxOwnership::ActiveTurn
    } else if !snapshot.intervention_queue.is_empty() {
        MailboxOwnership::PendingQueue
    } else {
        MailboxOwnership::None
    }
}

pub(super) async fn find_runtime_channel_match(
    registry: &HealthRegistry,
    provider_name: Option<&str>,
    channel_id: Option<ChannelId>,
    tmux_name: Option<&str>,
) -> Option<RuntimeChannelMatch> {
    let preferred_provider = provider_name.and_then(ProviderKind::from_str);
    let providers: Vec<_> = registry
        .providers
        .lock()
        .await
        .iter()
        .filter_map(|entry| {
            let provider = ProviderKind::from_str(&entry.name)?;
            if preferred_provider
                .as_ref()
                .is_some_and(|preferred| preferred != &provider)
            {
                return None;
            }
            Some((provider, entry.shared.clone()))
        })
        .collect();

    if let Some(channel_id) = channel_id {
        let mut queued_owner = None;
        let mut runtime_evidence = None;
        for (provider, shared) in providers {
            match local_mailbox_ownership(&shared, channel_id).await {
                MailboxOwnership::ActiveTurn => {
                    return Some(RuntimeChannelMatch {
                        provider,
                        shared,
                        channel_id,
                    });
                }
                MailboxOwnership::PendingQueue if queued_owner.is_none() => {
                    queued_owner = Some(RuntimeChannelMatch {
                        provider: provider.clone(),
                        shared: shared.clone(),
                        channel_id,
                    });
                }
                _ => {}
            }
            // Same-provider runtimes are explicitly supported by HealthRegistry::register.
            // Session/watcher rows may be stale, so retain them only as a fallback after
            // every sibling has been scanned for a live token or pending queue owner.
            if runtime_evidence.is_none() {
                let has_session = {
                    let data = shared.core.lock().await;
                    data.sessions.contains_key(&channel_id)
                };
                let has_watcher = shared.tmux_watchers.contains_key(&channel_id);
                if has_session || has_watcher {
                    runtime_evidence = Some(RuntimeChannelMatch {
                        provider,
                        shared,
                        channel_id,
                    });
                }
            }
        }
        return queued_owner.or(runtime_evidence);
    }

    for (provider, shared) in providers {
        let Some(tmux_name) = tmux_name else {
            continue;
        };
        let matched_channel_id = {
            let data = shared.core.lock().await;
            data.sessions
                .iter()
                .find_map(|(candidate_channel_id, session)| {
                    session.channel_name.as_ref().and_then(|channel_name| {
                        let expected_tmux_name = provider.build_tmux_session_name(channel_name);
                        (expected_tmux_name == tmux_name).then_some(*candidate_channel_id)
                    })
                })
        };
        if let Some(channel_id) = matched_channel_id {
            return Some(RuntimeChannelMatch {
                provider,
                shared,
                channel_id,
            });
        }
    }

    None
}

pub async fn provider_channel_mailbox_state(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: u64,
) -> Option<ProviderMailboxState> {
    let provider = ProviderKind::from_str(provider_name)?;
    let channel = ChannelId::new(channel_id);
    let shared = shared_for_provider(registry, &provider, channel).await?;
    // #3293 (c): peek, never create — the previous `shared.mailbox()` probe
    // minted a permanent registry entry for every queried channel id.
    Some(super::super::mailbox::peeked_provider_mailbox_state(&shared, channel_id).await)
}

pub async fn stop_runtime_turn_preserving_watcher(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    runtime_turn_cleanup_by_lookup(
        registry,
        provider_name,
        channel_id,
        tmux_name,
        stop_source,
        false,
    )
    .await
}

pub async fn stop_providerless_runtime_turn_preserving_watcher_strict_ownership(
    registry: Option<&HealthRegistry>,
    channel_id: u64,
    stop_source: &'static str,
) -> HardStopRuntimeResult {
    runtime_turn_cleanup_by_lookup(registry, None, Some(channel_id), None, stop_source, false).await
}

/// `expected_actor` binds the finish to the mailbox incarnation that accepted
/// the caller's earlier request (#5951); `None` finishes whichever is live.
pub async fn finish_cancelled_provider_channel_mailbox(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    stop_source: &'static str,
    expected_actor: Option<&crate::services::turn_orchestrator::ChannelMailboxHandle>,
) -> FinishCancelledMailboxResult {
    let Some(registry) = registry else {
        return FinishCancelledMailboxResult::default();
    };
    let Some(channel_id) = channel_id.map(ChannelId::new) else {
        return FinishCancelledMailboxResult::default();
    };
    let Some(runtime) =
        find_runtime_channel_match(registry, provider_name, Some(channel_id), None).await
    else {
        return FinishCancelledMailboxResult::default();
    };

    let before = runtime.shared.restart.global_active.load(Ordering::Acquire);
    let owned_role_override =
        discord::turn_finalizer::cleanup::snapshot_role_override(&runtime.shared, channel_id);
    let finish =
        discord::mailbox_finish_cancelled_turn_on(&runtime.shared, channel_id, expected_actor)
            .await;
    if finish.removed_token.is_none() {
        return FinishCancelledMailboxResult {
            cleared_active_turn: false,
            global_active_decremented: false,
            has_pending_queue: finish.has_pending,
            runtime_session_cleared: false,
        };
    }

    let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
        &runtime.shared,
        &runtime.provider,
        channel_id,
        &finish,
        owned_role_override,
        stop_source,
        true,
    )
    .await;
    let after = runtime.shared.restart.global_active.load(Ordering::Acquire);
    let global_active_decremented = after < before;
    if !global_active_decremented {
        tracing::warn!(
            target: "agentdesk::services::discord::health::recovery",
            provider = runtime.provider.as_str(),
            channel_id = channel_id.get(),
            global_active_before = before,
            global_active_after = after,
            stop_source,
            "finished cancelled mailbox turn without decrementing global_active"
        );
    }
    FinishCancelledMailboxResult {
        cleared_active_turn: true,
        global_active_decremented,
        has_pending_queue: finish.has_pending,
        runtime_session_cleared,
    }
}

async fn runtime_turn_cleanup_by_lookup(
    registry: Option<&HealthRegistry>,
    provider_name: Option<&str>,
    channel_id: Option<u64>,
    tmux_name: Option<&str>,
    stop_source: &'static str,
    stop_watcher: bool,
) -> HardStopRuntimeResult {
    let channel_id = channel_id.map(ChannelId::new);

    if let Some(registry) = registry
        && let Some(runtime) =
            find_runtime_channel_match(registry, provider_name, channel_id, tmux_name).await
    {
        // A preserve stop leaves a turn whose host is not legacy tmux running.
        let (shared, provider, channel) = (&runtime.shared, &runtime.provider, runtime.channel_id);
        let judged =
            discord::turn_bridge::ChannelStop::judge(shared, provider, channel, None, false);
        let judged = judged.await;
        let judged = discord::turn_bridge::ChannelStop::offline_if_closed(judged);
        if !stop_watcher && discord::turn_bridge::keeps_turn(&judged) {
            let (cleanup_path, had_active_turn) = ("host-guard-kept", true);
            let kept = HardStopRuntimeResult::default();
            return HardStopRuntimeResult {
                cleanup_path,
                had_active_turn,
                ..kept
            };
        }
        let owned_role_override = discord::turn_finalizer::cleanup::snapshot_role_override(
            &runtime.shared,
            runtime.channel_id,
        );
        let finish = stop_judgement::finish_found_turn(shared, provider, channel, &judged).await;
        let finish = match finish {
            Ok(finish) => finish,
            Err(kept) => return kept,
        };
        let runtime_session_cleared = apply_runtime_hard_stop_cleanup(
            &runtime.shared,
            &runtime.provider,
            runtime.channel_id,
            &finish,
            owned_role_override,
            stop_source,
            stop_watcher,
        )
        .await;
        return HardStopRuntimeResult {
            cleanup_path: if finish.mailbox_online {
                "mailbox_canonical"
            } else {
                "mailbox_fallback"
            },
            had_active_turn: finish.removed_token.is_some(),
            has_pending_queue: finish.has_pending,
            runtime_session_cleared,
        };
    }

    if let Some(channel_id) = channel_id
        && let Some(handle) = discord::ChannelMailboxRegistry::global_handle(channel_id)
    {
        // The process-global mirror carries no runtime identity. Reaching this
        // branch means the instance-local lookup above could not resolve an
        // owning runtime, so mutating the mirrored actor could finish a
        // different sibling's mailbox. Observe only; an attributed finish must
        // go through the ownership-gated runtime path above.
        let snapshot = handle.snapshot().await;
        if !snapshot.intervention_queue.is_empty() {
            discord::turn_completion_events::warn_unresolvable_hard_stop_pending_backlog(
                channel_id,
                true,
                stop_source,
            );
        }
        if snapshot.cancel_token.is_some() {
            tracing::warn!(
                target: "agentdesk::services::discord::health::recovery",
                channel_id = channel_id.get(),
                requested_provider = provider_name.unwrap_or("<unspecified>"),
                stop_source,
                "hard-stop declined for observed mailbox turn because runtime ownership was unresolved or the provider did not match"
            );
        }
    }

    HardStopRuntimeResult::default()
}
