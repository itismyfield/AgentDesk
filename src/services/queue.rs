#[cfg(test)]
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, Intervention, QueuePersistenceContext,
};
#[cfg(test)]
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

#[cfg(test)]
static PRE_PURGE_HOOK_FOR_TESTS: LazyLock<Mutex<HashMap<ChannelId, (Vec<Intervention>, String)>>> =
    LazyLock::new(Default::default);

#[cfg(test)]
pub(crate) fn set_pre_purge_hook_for_test(
    channel: ChannelId,
    items: Vec<Intervention>,
    token: &str,
) {
    PRE_PURGE_HOOK_FOR_TESTS
        .lock()
        .unwrap()
        .insert(channel, (items, token.into()));
}

#[cfg(test)]
async fn run_pre_purge_hook_for_test(
    provider: &ProviderKind,
    channel: ChannelId,
    handle: &ChannelMailboxHandle,
) {
    let entry = PRE_PURGE_HOOK_FOR_TESTS.lock().unwrap().remove(&channel);
    if let Some((items, token)) = entry {
        handle
            .replace_queue(items, QueuePersistenceContext::new(provider, &token, None))
            .await;
    }
}

use crate::services::turn_cancel_queue_guard::{
    CancelRemovalWitness, PurgeCounts, disk_queue_lost,
};
use std::sync::Arc;

use axum::{Json, http::HeaderMap};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::services::discord::health::HealthRegistry;
use crate::services::discord::session_identity::{SessionIdentity, tmux_name_from_session_key};
use crate::services::provider::ProviderKind;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};
use crate::services::turn_lifecycle::{
    ForceKillRow, TurnLifecycleTarget, force_kill_turn_without_cancel_event,
    stop_turn_preserving_queue_without_cancel_event,
};
use poise::serenity_prelude::ChannelId;

/// Schedule preserved work and return only the depth observed after the drain attempt.
async fn schedule_post_cancel_queue_drain(
    health_registry: Option<&Arc<HealthRegistry>>,
    target: &TurnLifecycleTarget,
    reason: &'static str,
) -> Option<usize> {
    let registry = health_registry?;
    let provider = target.provider.as_ref()?;
    let channel_id = target.channel_id?;
    let outcome = crate::services::discord::health::schedule_pending_queue_drain_after_cancel(
        registry.as_ref(),
        provider.as_str(),
        channel_id,
        reason,
    )
    .await;
    outcome.queue_depth_after
}

/// Purge through the actor and retain its measured removal counts and post-rollback depth.
/// Without a global mirror, sweep files only; another registry's actor may still hold a queue.
pub(super) async fn force_purge_channel_mailbox(
    health_registry: Option<&Arc<HealthRegistry>>,
    target: &TurnLifecycleTarget,
    session_key: Option<&str>,
) -> Option<PurgeCounts> {
    let provider = target.provider.as_ref()?;
    let channel_id = target.channel_id?;
    let Some(handle) =
        crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(channel_id)
    else {
        let disk_files_removed =
            crate::services::turn_orchestrator::remove_channel_pending_queue_files_all_tokens(
                provider, channel_id,
            );
        tracing::info!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            disk_files_removed,
            "force purge found no live mailbox handle"
        );
        return Some(PurgeCounts {
            drained: 0,
            disk_files_removed,
            own_files_removed: Some(0),
            queue_len_after: None,
        });
    };
    let token_hash = session_key
        .and_then(SessionIdentity::parse)
        .and_then(|identity| identity.token_hash)
        .unwrap_or_default();
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        provider,
        &token_hash,
        None,
    );
    // #3029(D): force purge must also release the active-turn anchor so the
    // next dispatch is not blocked by a stale `cancel_token` /
    // `active_user_message_id`. The handler only clears an anchor whose token
    // is already `cancelled` (the force-kill above flipped it), so a fresh
    // turn that raced in after the kill keeps its anchor (#2706).
    #[cfg(test)]
    run_pre_purge_hook_for_test(provider, channel_id, &handle).await;
    let purge = handle.try_purge_queue(persistence, true).await.ok()?;
    let purged = purge.drained;
    tracing::info!(
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        purged,
        disk_files_removed = purge.disk_files_removed,
        cleared_active_anchor = purge.cleared_active_anchor,
        session_key,
        "force purged channel mailbox queue"
    );
    let finish = crate::services::discord::health::finish_cancelled_provider_channel_mailbox(
        health_registry.map(Arc::as_ref),
        Some(provider.as_str()),
        Some(channel_id.get()),
        "queue_api_force_cancel_post_purge",
        Some(&handle),
    )
    .await;
    if finish.cleared_active_turn {
        tracing::info!(
            provider = provider.as_str(),
            channel_id = channel_id.get(),
            global_active_decremented = finish.global_active_decremented,
            runtime_session_cleared = finish.runtime_session_cleared,
            "force purge finalized cancelled active mailbox turn"
        );
    }
    Some(purge.into())
}

#[derive(Clone)]
pub struct QueueService {
    pg_pool: Option<PgPool>,
}

#[derive(Debug)]
struct CancelTurnChannelTarget {
    agent_id: String,
    requested_provider: Option<String>,
}

#[derive(Debug)]
struct CancelDispatchTurnInfo {
    session_key: String,
    provider_name: Option<String>,
    agent_id: Option<String>,
    channel_id: Option<String>,
}

impl QueueService {
    pub fn new(pg_pool: Option<PgPool>) -> Self {
        Self { pg_pool }
    }

    pub async fn cancel_dispatch(
        &self,
        health_registry: Option<&Arc<HealthRegistry>>,
        dispatch_id: &str,
    ) -> ServiceResult<Value> {
        let Some(pool) = self.pg_pool.as_ref() else {
            return Err(ServiceError::internal(
                "postgres pool unavailable for queue.cancel_dispatch",
            )
            .with_code(ErrorCode::Database)
            .with_operation("cancel_dispatch.no_pool")
            .with_context("dispatch_id", dispatch_id));
        };
        self.cancel_dispatch_pg(pool, health_registry, dispatch_id)
            .await
    }

    async fn cancel_dispatch_pg(
        &self,
        pool: &PgPool,
        health_registry: Option<&Arc<HealthRegistry>>,
        dispatch_id: &str,
    ) -> ServiceResult<Value> {
        let current_status =
            sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
                .bind(dispatch_id)
                .fetch_optional(pool)
                .await
                .map_err(|error| {
                    ServiceError::internal(format!("load postgres dispatch status: {error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("cancel_dispatch.query_status_pg")
                        .with_context("dispatch_id", dispatch_id)
                })?;

        match current_status.as_deref() {
            None => Err(ServiceError::not_found("dispatch not found")
                .with_code(ErrorCode::Dispatch)
                .with_context("dispatch_id", dispatch_id)),
            Some("completed") | Some("cancelled") | Some("failed") => {
                Err(ServiceError::conflict(format!(
                    "dispatch already in terminal state: {}",
                    current_status.unwrap_or_default()
                ))
                .with_code(ErrorCode::Dispatch)
                .with_context("dispatch_id", dispatch_id))
            }
            Some(_) => {
                let active_turn = self
                    .load_cancel_dispatch_turn_session(pool, dispatch_id)
                    .await?;
                let permit = match active_turn.as_ref() {
                    None => None,
                    Some(turn) => {
                        let channel = turn.channel_id.as_deref().ok_or_else(|| {
                            ServiceError::conflict("active turn channel unobserved")
                        })?;
                        crate::services::cluster::channel_home::admit_command(
                            channel,
                            turn.provider_name.as_deref().unwrap_or("cancel_dispatch"),
                        )
                        .map_err(|reason| ServiceError::conflict(reason.to_string()))?
                    }
                };
                crate::services::cluster::channel_home::command_scope(permit, async {
                let mut turn_cancelled = false;
                let mut turn_session_key = None;
                let mut turn_tmux_name = None;
                let mut turn_channel_id = None;
                let mut turn_agent_id = None;
                let mut turn_status = None;
                let mut turn_completed_at = None;
                let mut turn_lifecycle_path = None;
                let mut turn_tmux_killed = None;
                let mut turn_queue_preserved = None;
                let mut turn_inflight_cleared = None;
                let mut turn_queued_remaining = None;
                // #1672 P2: capture the lifecycle target so we can kick a
                // post-cancel queue drain after the dispatch row is
                // updated — same semantics the `/turns/{id}/cancel`
                // surface already provides.
                let mut drain_target: Option<TurnLifecycleTarget> = None;

                if let Some(active_turn) = active_turn.as_ref() {
                    let provider_kind = active_turn
                        .provider_name
                        .as_deref()
                        .and_then(ProviderKind::from_str);
                    let parsed_channel_id = active_turn
                        .channel_id
                        .as_deref()
                        .and_then(|channel_id| channel_id.parse::<u64>().ok())
                        .map(ChannelId::new);
                    let tmux_name = tmux_name_from_session_key(&active_turn.session_key)
                        .unwrap_or_else(|| active_turn.session_key.clone());
                    let target = TurnLifecycleTarget {
                        provider: provider_kind,
                        channel_id: parsed_channel_id,
                        tmux_name: tmux_name.clone(),
                    };
                    let lifecycle = stop_turn_preserving_queue_without_cancel_event(
                        health_registry.map(Arc::as_ref),
                        &target,
                        "queue-api cancel_dispatch (preserve)",
                    )
                    .await;
                    // A kept turn changed nothing: the dispatch and its session stay as they are.
                    if lifecycle.host_guard_kept() {
                        return Err(ServiceError::conflict(lifecycle.refusal_reason())
                            .with_code(ErrorCode::Dispatch)
                            .with_context("dispatch_id", dispatch_id));
                    }
                    let finalizer =
                        crate::services::turn_cancel_finalizer::finalize_turn_cancel(
                            crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest::from_lifecycle_result(
                                crate::services::turn_cancel_finalizer::TurnCancelCorrelation {
                                    provider: target.provider.clone(),
                                    channel_id: target.channel_id,
                                    dispatch_id: Some(dispatch_id.to_string()),
                                    session_key: Some(active_turn.session_key.clone()),
                                    turn_id: None,
                                },
                                "queue-api cancel_dispatch (preserve)",
                                crate::services::turn_lifecycle::cleanup_policy_observability_surface(
                                    crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                                        restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
                                    },
                                ),
                                &lifecycle,
                            ),
                        );

                    if let Err(error) = sqlx::query(
                        "UPDATE sessions
                         SET status = 'disconnected',
                             active_dispatch_id = NULL,
                             claude_session_id = NULL
                         WHERE session_key = $1",
                    )
                    .bind(&active_turn.session_key)
                    .execute(pool)
                    .await
                    {
                        tracing::warn!(
                            session_key = active_turn.session_key,
                            "failed to mark postgres session disconnected during cancel_dispatch: {error}"
                        );
                    }

                    turn_cancelled = true;
                    turn_session_key = Some(active_turn.session_key.clone());
                    turn_tmux_name = Some(tmux_name);
                    turn_channel_id = active_turn.channel_id.clone();
                    turn_agent_id = active_turn.agent_id.clone();
                    turn_status = Some(finalizer.status);
                    turn_completed_at = Some(finalizer.completed_at.to_rfc3339());
                    turn_lifecycle_path = Some(lifecycle.lifecycle_path);
                    turn_tmux_killed = Some(lifecycle.tmux_killed);
                    turn_queue_preserved = lifecycle.queue_preserved;
                    turn_inflight_cleared = Some(lifecycle.inflight_cleared);
                    turn_queued_remaining = lifecycle.queue_depth_if_observed();
                    drain_target = Some(target);
                }

                crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                    pool,
                    dispatch_id,
                    None,
                )
                .await
                .map_err(|error| {
                    ServiceError::internal(format!("cancel postgres dispatch: {error}"))
                        .with_code(ErrorCode::Database)
                        .with_operation("cancel_dispatch.cancel_pg")
                        .with_context("dispatch_id", dispatch_id)
                })?;

                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(format!("dispatch_notified:{dispatch_id}"))
                    .execute(pool)
                    .await
                    .map_err(|error| {
                        ServiceError::internal(format!(
                            "clear postgres dispatch notify guard: {error}"
                        ))
                        .with_code(ErrorCode::Database)
                        .with_operation("cancel_dispatch.clear_guard_pg")
                        .with_context("dispatch_id", dispatch_id)
                    })?;

                // A refused drain invalidates an earlier depth observation too.
                if let Some(target) = drain_target.as_ref() {
                    turn_queued_remaining = schedule_post_cancel_queue_drain(
                        health_registry,
                        target,
                        "queue_api_cancel_dispatch",
                    )
                    .await;
                }

                tracing::info!("[queue-api] Cancelled dispatch {dispatch_id}");
                Ok(json!({
                    "ok": true,
                    "dispatch_id": dispatch_id,
                    "active_turn_cancelled": turn_cancelled,
                    "turn_session_key": turn_session_key,
                    "turn_tmux_session": turn_tmux_name,
                    "turn_channel_id": turn_channel_id,
                    "turn_agent_id": turn_agent_id,
                    "turn_status": turn_status,
                    "turn_completed_at": turn_completed_at,
                    "turn_lifecycle_path": turn_lifecycle_path,
                    "turn_tmux_killed": turn_tmux_killed,
                    "turn_queue_preserved": turn_queue_preserved,
                    "turn_inflight_cleared": turn_inflight_cleared,
                    "turn_queued_remaining": turn_queued_remaining,
                }))
                }).await
            }
        }
    }

    pub async fn cancel_all_dispatches(
        &self,
        kanban_card_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> ServiceResult<Value> {
        let Some(pool) = self.pg_pool.as_ref() else {
            return Err(ServiceError::internal(
                "postgres pool unavailable for queue.cancel_all_dispatches",
            )
            .with_code(ErrorCode::Database)
            .with_operation("cancel_all_dispatches.no_pool"));
        };
        self.cancel_all_dispatches_pg(pool, kanban_card_id, agent_id)
            .await
    }

    async fn cancel_all_dispatches_pg(
        &self,
        pool: &PgPool,
        kanban_card_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> ServiceResult<Value> {
        let mut query = QueryBuilder::<Postgres>::new(
            "SELECT id FROM task_dispatches WHERE status IN ('pending', 'dispatched')",
        );

        if let Some(card_id) = kanban_card_id {
            query.push(" AND kanban_card_id = ");
            query.push_bind(card_id);
        }
        if let Some(agent_id) = agent_id {
            query.push(" AND to_agent_id = ");
            query.push_bind(agent_id);
        }

        let dispatch_ids = query
            .build_query_scalar::<String>()
            .fetch_all(pool)
            .await
            .map_err(|error| {
                ServiceError::internal(format!("query postgres cancel-all dispatches: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_all_dispatches.query_pg")
            })?;

        let mut count = 0usize;
        for dispatch_id in &dispatch_ids {
            count += crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                pool,
                dispatch_id,
                None,
            )
            .await
            .map_err(|error| {
                ServiceError::internal(format!("cancel postgres dispatch {dispatch_id}: {error}"))
                    .with_code(ErrorCode::Database)
                    .with_operation("cancel_all_dispatches.cancel_pg")
                    .with_context("dispatch_id", dispatch_id)
            })?;
        }

        tracing::info!(
            "[queue-api] Cancelled {count} dispatches (card={:?}, agent={:?})",
            kanban_card_id,
            agent_id
        );
        Ok(json!({"ok": true, "cancelled": count}))
    }

    /// Cancel an in-flight turn for `channel_id`.
    ///
    /// When `force == false` (the default for the public REST surface) the
    /// active provider session and watcher are *preserved*: the queue gets
    /// drained but any tool/process subtree under the live turn keeps
    /// running. This matches what an operator usually means by "remove queue
    /// item" / "cancel queue" — they are reorganising the channel's mailbox,
    /// not asking us to SIGKILL the cargo build that is currently running.
    ///
    /// When `force == true` we fall back to the historical hard-kill path
    /// (`force_kill_turn` → `kill_pid_tree` → SIGTERM/SIGKILL on the process
    /// group). Reserve this for explicit recovery scenarios where the live
    /// turn has to be torn down (#1196).
    pub async fn cancel_turn(
        &self,
        health_registry: Option<&Arc<HealthRegistry>>,
        channel_id: &str,
        force: bool,
        headers: &HeaderMap,
        forward_context: &crate::services::session_forwarding::ForwardCallerContext,
    ) -> ServiceResult<Value> {
        let permit =
            crate::services::cluster::channel_home::admit_command(channel_id, "cancel_turn")
                .map_err(|reason| {
                    ServiceError::conflict(reason.to_string())
                        .with_context("channel_id", channel_id)
                })?;
        crate::services::cluster::channel_home::command_scope(permit, async {
        if let Some(response) =
            crate::services::session_forwarding::forward_remote_cancel_if_needed(
                forward_context,
                headers,
                channel_id,
                force,
            )
            .await?
        {
            return Ok(response);
        }

        let pool = self.pg_pool.as_ref().ok_or_else(|| {
            ServiceError::internal("postgres pool unavailable for cancel_turn session lookup")
                .with_code(ErrorCode::Database)
                .with_operation("cancel_turn.query_active_session.no_pool")
                .with_context("channel_id", channel_id)
        })?;
        let session_info =
            crate::services::session_forwarding::load_cancel_turn_session(pool, channel_id).await?;

        let owner = session_info
            .as_ref()
            .and_then(|session| session.owner_instance_id.as_deref());
        if let Err((_, Json(body))) = crate::services::session_forwarding::enforce_receiver_fence(
            headers,
            owner,
            forward_context.cluster_instance_id.as_deref(),
        ) {
            return Err(ServiceError::conflict(
                "forwarded cancel owner changed before local mutation",
            )
            .with_context("channel_id", channel_id)
            .with_context("forward_fence", body));
        }

        let channel_target = self.resolve_cancel_turn_channel_target(channel_id).await?;

        let (session_key, dispatch_id, provider_name, agent_id, requested_provider, match_rank) =
            if let Some(session_info) = session_info {
                (
                    Some(session_info.session_key),
                    session_info.dispatch_id,
                    session_info.provider_name,
                    session_info.agent_id,
                    session_info.requested_provider,
                    Some(session_info.match_rank),
                )
            } else if let Some(channel_target) = channel_target {
                (
                    None,
                    None,
                    channel_target.requested_provider.clone(),
                    Some(channel_target.agent_id),
                    channel_target.requested_provider,
                    None,
                )
            } else {
                return Err(
                    ServiceError::not_found("no active turn found for this channel")
                        .with_context("channel_id", channel_id)
                        .with_context(
                            "expected_owner_instance_id",
                            crate::services::session_forwarding::forwarded_session_owner(headers),
                        )
                        .with_context(
                            "receiver_instance_id",
                            forward_context.cluster_instance_id.as_deref(),
                        ),
                );
            };

        let provider_kind = provider_name.as_deref().and_then(ProviderKind::from_str);
        let parsed_channel_id = channel_id.parse::<u64>().ok().map(ChannelId::new);
        if session_key.is_none()
            && (provider_kind.is_none() || parsed_channel_id.is_none() || health_registry.is_none())
        {
            return Err(
                ServiceError::not_found("no active turn found for this channel")
                    .with_context("channel_id", channel_id)
                    .with_context(
                        "owner_instance_id",
                        forward_context.cluster_instance_id.as_deref(),
                    ),
            );
        }

        crate::services::session_forwarding::revalidate_local_cancel_owner(
            forward_context,
            channel_id,
            session_key.as_deref(),
        )
        .await?;

        let tmux_name = session_key
            .as_deref()
            .map(|session_key| {
                tmux_name_from_session_key(session_key).unwrap_or_else(|| session_key.to_string())
            })
            .unwrap_or_default();
        let target = TurnLifecycleTarget {
            provider: provider_kind,
            channel_id: parsed_channel_id,
            tmux_name: tmux_name.clone(),
        };
        // #5176 R3: read the queue before the cancel can empty it. Everything
        // below is about putting back whatever this cancel takes.
        let queue_capture =
            crate::services::turn_cancel_queue_guard::capture_queue_before_cancel(&target).await;
        let lifecycle = if force {
            let row = self.pg_pool.as_ref().zip(session_key.as_deref());
            let row = row.map(|(pool, session_key)| ForceKillRow {
                pool,
                session_key,
                stored_provider: provider_name.as_deref(),
            });
            force_kill_turn_without_cancel_event(
                health_registry.map(Arc::as_ref),
                &target,
                row,
                "queue-api cancel_turn (force)",
                "queue_api_cancel_turn",
            )
            .await
        } else {
            stop_turn_preserving_queue_without_cancel_event(
                health_registry.map(Arc::as_ref),
                &target,
                "queue-api cancel_turn (preserve)",
            )
            .await
        };
        // A refused kill changed nothing: no cancel event, dispatch or session update follows.
        if lifecycle.host_guard_kept() {
            return Err(ServiceError::conflict(lifecycle.refusal_reason())
                .with_context("channel_id", channel_id)
                .with_context("session_key", session_key.as_deref()));
        }
        let finalizer = crate::services::turn_cancel_finalizer::finalize_turn_cancel(
            crate::services::turn_cancel_finalizer::FinalizeTurnCancelRequest::from_lifecycle_result(
                crate::services::turn_cancel_finalizer::TurnCancelCorrelation {
                    provider: target.provider.clone(),
                    channel_id: target.channel_id,
                    dispatch_id: dispatch_id.clone(),
                    session_key: session_key.clone(),
                    turn_id: None,
                },
                if force {
                    "queue-api cancel_turn (force)"
                } else {
                    "queue-api cancel_turn (preserve)"
                },
                crate::services::turn_lifecycle::cleanup_policy_observability_surface(if force {
                    crate::services::discord::TmuxCleanupPolicy::CleanupSession {
                        termination_reason_code: Some("queue_api_cancel_turn"),
                    }
                } else {
                    crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                        restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
                    }
                }),
                &lifecycle,
            ),
        );

        if let Some(dispatch_id) = dispatch_id.as_ref() {
            if let Some(pool) = self.pg_pool.as_ref()
                && let Err(error) = crate::dispatch::cancel_dispatch_and_reset_auto_queue_on_pg(
                    pool,
                    dispatch_id,
                    None,
                )
                .await
            {
                tracing::warn!(
                    dispatch_id,
                    "failed to cancel postgres dispatch while cancelling turn: {error}"
                );
            }
        }

        if let Some(session_key) = session_key.as_deref() {
            if let Some(pool) = self.pg_pool.as_ref()
                && let Err(error) = sqlx::query(
                    "UPDATE sessions
                     SET status = 'disconnected',
                         active_dispatch_id = NULL,
                         claude_session_id = NULL
                     WHERE session_key = $1",
                )
                .bind(session_key)
                .execute(pool)
                .await
            {
                tracing::warn!(
                    session_key,
                    "failed to mark postgres session disconnected during cancel_turn: {error}"
                );
            }
        }

        let exact_channel_match = match_rank.is_none_or(|rank| rank <= 2);

        // #1672: prefer the *observed* tmux session name over the
        // session-key-derived one. The latter is empty whenever the
        // session row is missing (cancel-via-watcher fallback) but the
        // runtime still knows the tmux name from the watcher binding /
        // inflight state — those are exactly the incidents where the
        // operator most needs to see the real session name.
        let reported_tmux_session = lifecycle
            .tmux_session_observed
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| tmux_name.clone());
        let queue_purge = if force {
            force_purge_channel_mailbox(health_registry, &target, session_key.as_deref()).await
        } else {
            None
        };
        let drain_depth = if !force {
            schedule_post_cancel_queue_drain(health_registry, &target, "queue_api_cancel_turn")
                .await
        } else {
            None
        };
        let loss = crate::services::turn_cancel_queue_guard::record_queue_loss_after_cancel(
            &target,
            &queue_capture,
            self.pg_pool.as_ref(),
            if force {
                CancelRemovalWitness::Purge(queue_purge)
            } else {
                CancelRemovalWitness::Preserve {
                    disk_lost: disk_queue_lost(
                        lifecycle.queue_disk_present_before,
                        lifecycle.queue_disk_present_after,
                    ),
                }
            },
            if force {
                "queue_api_cancel_turn_force"
            } else {
                "queue_api_cancel_turn"
            },
        )
        .await;
        // Publish only measurements after the final queue mutation attempt.
        let queued_remaining = if force {
            loss.queue_depth_after
                .or(queue_purge.as_ref().and_then(|c| c.queue_len_after))
        } else {
            drain_depth.or(loss.queue_depth_after)
        };
        let queue_purged_count = queue_purge.as_ref().map(|c| c.drained);

        tracing::info!(
            "[queue-api] Cancelled turn: channel={}, session={:?}, tmux={}, killed={}, dispatch={:?}, lifecycle={}, agent={:?}, requested_provider={:?}, exact_match={}, queue_preserved={:?}, queued_before={:?}, queued_after={:?}, queue_disk_before={:?}, queue_disk_after={:?}, queue_purged={:?}, mailbox_foreground_free={:?}, queue_dropped_message_ids={:?}",
            channel_id,
            session_key,
            reported_tmux_session,
            lifecycle.tmux_killed,
            dispatch_id,
            lifecycle.lifecycle_path,
            agent_id,
            requested_provider,
            exact_channel_match,
            lifecycle.queue_preserved,
            lifecycle.queue_depth_before,
            lifecycle.queue_depth_after,
            lifecycle.queue_disk_present_before,
            lifecycle.queue_disk_present_after,
            queue_purged_count,
            lifecycle.mailbox_foreground_free,
            lifecycle.queue_dropped_message_ids,
        );

        Ok(json!({
            "ok": true,
            "channel_id": channel_id,
            "agent_id": agent_id,
            "requested_provider": requested_provider,
            "exact_channel_match": exact_channel_match,
            "session_key": session_key,
            "tmux_session": reported_tmux_session,
            "tmux_killed": lifecycle.tmux_killed,
            "lifecycle_path": lifecycle.lifecycle_path,
            "queued_remaining": queued_remaining,
            "queued_before": lifecycle.queue_depth_before,
            "queue_purged": queue_purged_count,
            "queue_preserved": lifecycle.queue_preserved,
            "queue_disk_present_before": lifecycle.queue_disk_present_before,
            "queue_disk_present_after": lifecycle.queue_disk_present_after,
            "inflight_cleared": lifecycle.inflight_cleared,
            // #5176: the honest cancel-success field. `turn_status: cancelled`
            // only stamps the turn record; this says whether the channel mailbox
            // actually let go of its foreground slot. `false` means the channel
            // is still blocked no matter what `turn_status` says.
            "mailbox_foreground_free": lifecycle.mailbox_foreground_free,
            // #5176: which user instructions the cancel destroyed, by primary
            // Discord message id. Empty is the contract; non-empty is a bug
            // report the operator can act on.
            "queue_dropped_message_ids": lifecycle.queue_dropped_message_ids,
            // True proves no removal; recorded removals remain unknown, and any unrecorded loss is false.
            "queue_dead_lettered_message_ids": loss.dead_lettered_message_ids,
            "queue_unpreserved_message_ids": loss.unpreserved_message_ids,
            "queue_loss_recorded": loss.loss_recorded(),
            "dispatch_cancelled": dispatch_id,
            "turn_status": finalizer.status,
            "turn_completed_at": finalizer.completed_at.to_rfc3339(),
        }))
        }).await
    }

    async fn resolve_cancel_turn_channel_target(
        &self,
        channel_id: &str,
    ) -> ServiceResult<Option<CancelTurnChannelTarget>> {
        let Some(pool) = self.pg_pool.as_ref() else {
            return Err(ServiceError::internal(
                "postgres pool unavailable for cancel_turn channel target",
            )
            .with_code(ErrorCode::Database)
            .with_operation("cancel_turn.query_channel_target.no_pool")
            .with_context("channel_id", channel_id));
        };
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT id,
                    CASE
                      WHEN discord_channel_cc = $1 OR discord_channel_id = $1 THEN 'claude'
                      WHEN discord_channel_cdx = $1 OR discord_channel_alt = $1 THEN 'codex'
                      ELSE NULL
                    END AS requested_provider
             FROM agents
             WHERE discord_channel_id = $1
                OR discord_channel_alt = $1
                OR discord_channel_cc = $1
                OR discord_channel_cdx = $1
             LIMIT 1",
        )
        .bind(channel_id)
        .fetch_optional(pool)
        .await
        .map(|row| {
            row.map(|(agent_id, requested_provider)| CancelTurnChannelTarget {
                agent_id,
                requested_provider,
            })
        })
        .map_err(|error| {
            ServiceError::internal(format!("load postgres cancel channel target: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("cancel_turn.query_channel_target_pg")
                .with_context("channel_id", channel_id)
        })
    }

    async fn load_cancel_dispatch_turn_session(
        &self,
        pool: &PgPool,
        dispatch_id: &str,
    ) -> ServiceResult<Option<CancelDispatchTurnInfo>> {
        sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
            "SELECT s.session_key,
                    COALESCE(s.provider, a.provider) AS provider_name,
                    s.agent_id,
                    COALESCE(
                      NULLIF(s.thread_channel_id, ''),
                      CASE COALESCE(s.provider, a.provider, '')
                        WHEN 'claude' THEN COALESCE(NULLIF(a.discord_channel_cc, ''), NULLIF(a.discord_channel_id, ''))
                        WHEN 'codex' THEN COALESCE(NULLIF(a.discord_channel_cdx, ''), NULLIF(a.discord_channel_alt, ''), NULLIF(a.discord_channel_id, ''))
                        ELSE COALESCE(NULLIF(a.discord_channel_id, ''), NULLIF(a.discord_channel_cc, ''), NULLIF(a.discord_channel_cdx, ''), NULLIF(a.discord_channel_alt, ''))
                      END
                    ) AS channel_id
             FROM sessions s
             LEFT JOIN agents a ON a.id = s.agent_id
             WHERE s.active_dispatch_id = $1
               AND s.status IN ('turn_active', 'working')
             ORDER BY s.last_heartbeat DESC
             LIMIT 1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map(|row| {
            row.map(
                |(session_key, provider_name, agent_id, channel_id)| CancelDispatchTurnInfo {
                    session_key,
                    provider_name,
                    agent_id,
                    channel_id,
                },
            )
        })
        .map_err(|error| {
            ServiceError::internal(format!("load postgres active dispatch turn: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("cancel_dispatch.query_active_session_pg")
                .with_context("dispatch_id", dispatch_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use poise::serenity_prelude::{MessageId, UserId};

    use super::*;
    use crate::services::turn_orchestrator::{
        ChannelMailboxRegistry, EnqueueRefusalReason, Intervention, InterventionMode,
        QueuePersistenceContext, save_channel_queue,
    };

    struct EnvReset(Option<std::ffi::OsString>);

    impl Drop for EnvReset {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn make_intervention(message_id: u64, text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(1),
            author_is_bot: false,
            message_id: MessageId::new(message_id),
            queued_generation: crate::services::discord::runtime_store::process_generation(),
            source_message_ids: vec![MessageId::new(message_id)],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        }
    }

    #[test]
    fn force_purge_channel_mailbox_drains_queue_without_session_key() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(9270601);
            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let persistence = QueuePersistenceContext::new(&provider, "force-purge-test", None);
            handle
                .replace_queue(
                    vec![make_intervention(10, "stale queued prompt")],
                    persistence,
                )
                .await;

            let target = TurnLifecycleTarget {
                provider: Some(provider),
                channel_id: Some(channel_id),
                tmux_name: String::new(),
            };

            let purged = force_purge_channel_mailbox(None, &target, None).await;

            assert_eq!(purged.map(|c| c.drained), Some(1));
            assert!(handle.snapshot().await.intervention_queue.is_empty());
        });
    }

    /// #4535 mutation guard: a restart replay can hydrate durable work and then
    /// dedup without reporting a fresh enqueue. Handle creation must already
    /// have published the actor so force purge reaches the live hydrated queue.
    #[test]
    fn force_purge_reaches_queue_hydrated_by_dedup_replay() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
        let temp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let provider = ProviderKind::Claude;
            let channel_id = ChannelId::new(9_270_602);
            let persistence = QueuePersistenceContext::new(&provider, "", None);
            let durable = make_intervention(11, "hydrated stale prompt");
            save_channel_queue(
                &provider,
                &persistence.token_hash,
                channel_id,
                std::slice::from_ref(&durable),
                None,
            )
            .expect("seed durable pending queue");

            let registry = ChannelMailboxRegistry::default();
            let handle = registry.handle(channel_id);
            let replay = handle.enqueue(durable, persistence.clone()).await;
            assert!(!replay.enqueued);
            assert!(!replay.merged);
            assert_eq!(
                replay.refusal_reason,
                Some(EnqueueRefusalReason::SourceIdAlreadyQueued)
            );
            assert_eq!(handle.snapshot().await.intervention_queue.len(), 1);
            assert!(
                ChannelMailboxRegistry::global_handle(channel_id).is_some(),
                "hydrated actor must remain reachable through the force-purge registry"
            );

            let target = TurnLifecycleTarget {
                provider: Some(provider),
                channel_id: Some(channel_id),
                tmux_name: String::new(),
            };
            let purged = force_purge_channel_mailbox(None, &target, None).await;

            assert_eq!(purged.map(|c| c.drained), Some(1));
            assert!(handle.snapshot().await.intervention_queue.is_empty());
            let later_dispatch = handle.take_next_soft(persistence).await;
            assert!(
                later_dispatch.intervention.is_none(),
                "force-purged hydrated message must not dispatch later"
            );
        });
    }
}
