use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};
use crate::services::turn_lifecycle::{
    TurnLifecycleTarget, force_kill_turn_without_cancel_event,
    stop_turn_preserving_queue_without_cancel_event,
};
use poise::serenity_prelude::ChannelId;

#[derive(Clone)]
pub struct QueueService {
    pg_pool: Option<PgPool>,
}

#[derive(Debug)]
struct CancelTurnSessionInfo {
    session_key: String,
    dispatch_id: Option<String>,
    provider_name: Option<String>,
    agent_id: Option<String>,
    requested_provider: Option<String>,
    match_rank: i64,
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
                    let tmux_name = active_turn
                        .session_key
                        .split(':')
                        .next_back()
                        .unwrap_or_default()
                        .to_string();
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
                    turn_queue_preserved = Some(lifecycle.queue_preserved);
                    turn_inflight_cleared = Some(lifecycle.inflight_cleared);
                    turn_queued_remaining = lifecycle.queue_depth;
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
    ) -> ServiceResult<Value> {
        let channel_target = self.resolve_cancel_turn_channel_target(channel_id).await?;
        let session_info = self.load_cancel_turn_session(channel_id).await?;

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
                        .with_code(ErrorCode::Queue)
                        .with_context("channel_id", channel_id),
                );
            };

        let provider_kind = provider_name.as_deref().and_then(ProviderKind::from_str);
        let parsed_channel_id = channel_id.parse::<u64>().ok().map(ChannelId::new);
        if session_key.is_none()
            && (provider_kind.is_none() || parsed_channel_id.is_none() || health_registry.is_none())
        {
            return Err(
                ServiceError::not_found("no active turn found for this channel")
                    .with_code(ErrorCode::Queue)
                    .with_context("channel_id", channel_id),
            );
        }

        let tmux_name = session_key
            .as_deref()
            .and_then(|session_key| session_key.split(':').next_back())
            .unwrap_or_default()
            .to_string();
        let target = TurnLifecycleTarget {
            provider: provider_kind,
            channel_id: parsed_channel_id,
            tmux_name: tmux_name.clone(),
        };
        let lifecycle = if force {
            force_kill_turn_without_cancel_event(
                health_registry.map(Arc::as_ref),
                &target,
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
        let queued_remaining = lifecycle.queue_depth_after.or(lifecycle.queue_depth);

        // #1672: with the cancel completed and the channel idle, kick
        // any survived pending_queue items so the next intervention is
        // picked up without needing a fresh user message to drive the
        // mailbox poll.
        if !force
            && let (Some(registry), Some(provider_kind), Some(channel)) =
                (health_registry, target.provider.as_ref(), target.channel_id)
        {
            let _ = crate::services::discord::health::schedule_pending_queue_drain_after_cancel(
                registry.as_ref(),
                provider_kind.as_str(),
                channel,
                "queue_api_cancel_turn",
            )
            .await;
        }

        tracing::info!(
            "[queue-api] Cancelled turn: channel={}, session={:?}, tmux={}, killed={}, dispatch={:?}, lifecycle={}, agent={:?}, requested_provider={:?}, exact_match={}, queue_preserved={}, queued_before={:?}, queued_after={:?}, queue_disk_before={}, queue_disk_after={}",
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
            "queue_preserved": lifecycle.queue_preserved,
            "queue_disk_present_before": lifecycle.queue_disk_present_before,
            "queue_disk_present_after": lifecycle.queue_disk_present_after,
            "inflight_cleared": lifecycle.inflight_cleared,
            "dispatch_cancelled": dispatch_id,
            "turn_status": finalizer.status,
            "turn_completed_at": finalizer.completed_at.to_rfc3339(),
        }))
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

    async fn load_cancel_turn_session(
        &self,
        channel_id: &str,
    ) -> ServiceResult<Option<CancelTurnSessionInfo>> {
        let Some(pool) = self.pg_pool.as_ref() else {
            return Err(ServiceError::internal(
                "postgres pool unavailable for cancel_turn session lookup",
            )
            .with_code(ErrorCode::Database)
            .with_operation("cancel_turn.query_active_session.no_pool")
            .with_context("channel_id", channel_id));
        };
        sqlx::query_as::<
            _,
            (
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                i64,
            ),
        >(
            "WITH channel_agent AS (
               SELECT id AS agent_id,
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
               LIMIT 1
             )
             SELECT s.session_key,
                    s.active_dispatch_id,
                    s.provider,
                    s.agent_id,
                    ca.requested_provider,
                    CASE
                      WHEN COALESCE(s.thread_channel_id, '') = $1 THEN 0
                      WHEN s.session_key LIKE '%' || $1 || '%' THEN 1
                      WHEN ca.requested_provider IS NOT NULL
                           AND COALESCE(s.provider, '') = ca.requested_provider THEN 2
                      ELSE 3
                    END::BIGINT AS match_rank
             FROM sessions s
             LEFT JOIN channel_agent ca ON s.agent_id = ca.agent_id
             WHERE s.status IN ('turn_active', 'working')
               AND (
                 COALESCE(s.thread_channel_id, '') = $1
                 OR s.session_key LIKE '%' || $1 || '%'
                 OR (
                   ca.agent_id IS NOT NULL
                   AND (
                     ca.requested_provider IS NULL
                     OR COALESCE(s.provider, '') = ca.requested_provider
                   )
                 )
               )
             ORDER BY match_rank ASC, s.last_heartbeat DESC
             LIMIT 1",
        )
        .bind(channel_id)
        .fetch_optional(pool)
        .await
        .map(|row| {
            row.map(
                |(
                    session_key,
                    dispatch_id,
                    provider_name,
                    agent_id,
                    requested_provider,
                    match_rank,
                )| CancelTurnSessionInfo {
                    session_key,
                    dispatch_id,
                    provider_name,
                    agent_id,
                    requested_provider,
                    match_rank,
                },
            )
        })
        .map_err(|error| {
            ServiceError::internal(format!("load postgres active turn: {error}"))
                .with_code(ErrorCode::Database)
                .with_operation("cancel_turn.query_active_session_pg")
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
