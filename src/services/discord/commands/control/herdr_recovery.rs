//! A replaced-root clear discards waiting input independently of execution retirement.
use std::collections::BTreeSet;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};
use tokio::sync::OwnedMutexGuard;

use super::super::super::{SharedData, inflight};
use super::clear_report::ExecutionCleanup;
use super::{ClearReport, native};
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    self, HostedLookup, HostedLookupKey, HostedRecord, HostedState,
};
use crate::services::discord::herdr_terminate::root_replaced::{self, Recovery};
use crate::services::discord::session_transition::SessionTransitionLease;
use crate::services::provider::ProviderKind;

pub(super) async fn clear(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    guard: Option<&OwnedMutexGuard<()>>,
    report: &mut ClearReport,
) -> bool {
    let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get()).is_some();
    let Some(pool) = shared.pg_pool.as_ref() else {
        if !endpoint {
            return false;
        }
        return discard(
            shared,
            provider,
            channel,
            None,
            None,
            "postgres unavailable".into(),
            report,
        )
        .await;
    };
    let key = channel.get().to_string();
    let lookup = hosted_execution::load_hosted_execution_pg(
        pool,
        HostedLookupKey::Canonical {
            provider: provider.as_str(),
            identity: CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &shared.token_hash,
                channel_id: &key,
            },
        },
    )
    .await;
    let row = match lookup {
        HostedLookup::Missing => return false,
        HostedLookup::Found(row) if matches!(&row.record, HostedRecord::Known(_)) => row,
        _ if !endpoint => return false,
        other => {
            return discard(
                shared,
                provider,
                channel,
                None,
                None,
                format!("execution lookup: {other:?}"),
                report,
            )
            .await;
        }
    };
    let HostedRecord::Known(record) = &row.record else {
        unreachable!()
    };
    if record.state == HostedState::Pending {
        return false;
    }
    let logical = record.owner.logical_key.clone();
    let candidate = if record.state == HostedState::Retired {
        None
    } else {
        let prepared = crate::services::discord::recovery_engine::herdr_reader::prepare_root_replaced_candidate(&row);
        let observed = match prepared {
            Ok(probe) => tokio::task::spawn_blocking(move || probe.observe()).await,
            Err(error) => {
                return discard(
                    shared,
                    provider,
                    channel,
                    Some(&logical),
                    None,
                    error,
                    report,
                )
                .await;
            }
        };
        match observed {
            Ok(Ok(Some(candidate))) => Some(candidate),
            Ok(Ok(None)) => return false,
            other => {
                return discard(
                    shared,
                    provider,
                    channel,
                    Some(&logical),
                    None,
                    format!("RootReplaced observation: {other:?}"),
                    report,
                )
                .await;
            }
        }
    };
    if let Some(candidate) = candidate.as_ref() {
        root_replaced::remember_candidate(candidate);
    }
    let (original, excluded) = original_execution(shared, provider, channel).await;
    let reason = original.err();
    discard_waiting(shared, provider, channel, Some(&logical), &excluded, report).await;
    let lease = guard.and_then(|guard| SessionTransitionLease::borrow(shared, channel, guard).ok());
    let result = if let Some(reason) = reason {
        Recovery::Held(reason)
    } else if let Some(lease) = lease.as_ref() {
        match candidate {
            Some(candidate) => root_replaced::recover(shared, pool, &candidate, Some(lease)).await,
            None => root_replaced::recover_retired(shared, pool, &row, Some(lease)).await,
        }
    } else {
        Recovery::Held("transition busy".into())
    };
    if result == Recovery::Retired {
        // A successor must not inherit a predecessor clear's selector or memory writes.
        let mut expected = record.clone();
        expected.state = HostedState::Retired;
        let retired = hosted_execution::load_hosted_execution_pg(
            pool,
            HostedLookupKey::Canonical {
                provider: provider.as_str(),
                identity: CanonicalSessionIdentity {
                    kind: SessionIdentityKind::DiscordChannel,
                    discord_token_hash: &shared.token_hash,
                    channel_id: &key,
                },
            },
        )
        .await;
        let same = match retired {
            HostedLookup::Found(ref retired)
                if retired.session_id() == row.session_id()
                    && retired.record == HostedRecord::Known(expected) =>
            {
                root_replaced::current(pool, retired).await.is_ok()
            }
            _ => false,
        };
        if same {
            let exact = crate::db::dispatched_session_canonical_identity::resolve_session_row_pg(
                pool,
                None,
                Some(provider.as_str()),
                Some(CanonicalSessionIdentity {
                    kind: SessionIdentityKind::DiscordChannel,
                    discord_token_hash: &shared.token_hash,
                    channel_id: &key,
                }),
            )
            .await;
            match exact {
                Ok(Some((id, key))) if id == row.session_id() => {
                    super::clear_teardown::forget_selector(shared, &key, report).await
                }
                other => report.fail(
                    "session_selector",
                    &format!("retired session key: {other:?}"),
                ),
            }
            native::clear_session_memory(shared, channel).await;
            native::clear_process_reset_pending(shared, channel).await;
            super::clear_teardown::write_boundary(shared, channel, report).await;
        } else {
            report.execution = Some(ExecutionCleanup::Pending(
                "execution changed after cleanup".into(),
            ));
            return true;
        }
    }
    report.execution = Some(match result {
        Recovery::Held(e) => ExecutionCleanup::Held(e),
        Recovery::RetiredCleanupPending(e) => ExecutionCleanup::Pending(e),
        Recovery::Retired => ExecutionCleanup::Retired,
    });
    true
}

async fn original_execution(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> (Result<(), String>, BTreeSet<MessageId>) {
    let mut excluded = BTreeSet::new();
    let mut reason = None;
    if let Some(mailbox) = shared.mailbox_peek(channel) {
        match mailbox.try_snapshot().await {
            Ok(s) => {
                excluded.extend(s.active_user_message_id);
                excluded.extend(s.active_absorbed_source_ids.iter().copied());
                if s.cancel_token.is_some()
                    || s.active_user_message_id.is_some()
                    || s.active_turn_nonce.is_some()
                    || s.active_request_owner.is_some()
                    || !s.active_absorbed_source_ids.is_empty()
                {
                    reason = Some("original actor is active".into());
                }
            }
            Err(_) => reason = Some("original mailbox unreadable".into()),
        }
    }
    match inflight::load_inflight_state_read_only_result(provider, channel.get()) {
        Ok(Some(row)) => {
            if row.user_msg_id != 0 {
                excluded.insert(MessageId::new(row.user_msg_id));
            }
            reason = Some("original inflight execution exists".into());
        }
        Err(e) => reason = Some(format!("original inflight unreadable: {e}")),
        Ok(None) => {}
    }
    (reason.map_or(Ok(()), Err), excluded)
}

async fn discard(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    logical: Option<&str>,
    excluded: Option<&BTreeSet<MessageId>>,
    reason: String,
    report: &mut ClearReport,
) -> bool {
    let (_, original) = original_execution(shared, provider, channel).await;
    discard_waiting(
        shared,
        provider,
        channel,
        logical,
        excluded.unwrap_or(&original),
        report,
    )
    .await;
    report.execution = Some(ExecutionCleanup::Held(reason));
    true
}

async fn discard_waiting(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    logical: Option<&str>,
    excluded: &BTreeSet<MessageId>,
    report: &mut ClearReport,
) {
    use crate::services::turn_orchestrator::QueuePersistenceContext;
    let persistence = QueuePersistenceContext::new(provider, &shared.token_hash, None);
    match shared
        .mailbox(channel)
        .discard_waiting_or_refused(persistence)
        .await
    {
        Ok(mut cleared) => {
            let mut excluded = excluded.clone();
            excluded.extend(cleared.persistence.not_waiting.iter().copied());
            let readable =
                match inflight::load_inflight_state_read_only_result(provider, channel.get()) {
                    Ok(Some(row)) => {
                        if row.user_msg_id != 0 {
                            excluded.insert(MessageId::new(row.user_msg_id));
                        }
                        true
                    }
                    Ok(None) => true,
                    Err(_) => false,
                };
            if !readable {
                cleared.persistence.count_unknown = true;
                report.fail("legacy_pending_discard", "original inflight unreadable");
            }
            if logical.is_none() {
                cleared.persistence.count_unknown = true;
                report.fail(
                    "legacy_pending_identity",
                    "original logical target unavailable",
                );
            }
            if let Some(logical) = logical.filter(|_| readable) {
                match super::super::super::tui_direct_pending_start::discard_legacy_waiting(
                    provider.as_str(),
                    channel.get(),
                    logical,
                    &excluded,
                ) {
                    Ok(ids) => cleared.discarded_message_ids.extend(ids),
                    Err(error) => {
                        cleared.persistence.count_unknown = true;
                        report.fail("legacy_pending_discard", &error);
                    }
                }
            }
            super::super::super::catch_up::retry_state::discard_cleared_backlog(
                shared, provider, channel, &cleared,
            );
            report.absorb(&cleared);
            if !cleared.persistence.failed.is_empty() {
                report.discarded = None;
            }
            tracing::info!(channel_id = channel.get(), discarded_ids = ?cleared.discarded_message_ids, failed = ?report.failed, "herdr_clear_waiting_discard");
        }
        Err(_) => {
            report.discarded = None;
            report.fail("mailbox_discard", "mailbox unavailable");
        }
    }
    let mut core = shared.core.lock().await;
    if let Some(session) = core.sessions.get_mut(&channel) {
        session.history.clear();
        session.pending_uploads.clear();
    }
    shared.dispatch.role_overrides.remove(&channel);
}
