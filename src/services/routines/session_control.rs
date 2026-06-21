use anyhow::{Result, anyhow};
use poise::serenity_prelude::ChannelId;
use serde::Serialize;
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::sync::Arc;

use crate::services::discord::health::{HealthRegistry, clear_provider_channel_runtime};
use crate::services::provider::ProviderKind;
use crate::services::turn_lifecycle::{TurnLifecycleTarget, force_kill_turn};

use super::store::RoutineRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutineSessionCommand {
    Reset,
    Kill,
}

impl RoutineSessionCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reset => "reset",
            Self::Kill => "kill",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutineSessionControlResult {
    pub action: &'static str,
    pub routine_id: String,
    pub agent_id: String,
    pub provider: String,
    pub channel_id: String,
    pub session_key: Option<String>,
    pub tmux_session: String,
    pub provider_clear_behavior: &'static str,
    pub runtime_cleared: bool,
    pub tmux_killed: bool,
    pub inflight_cleared: bool,
    pub lifecycle_path: &'static str,
    pub queued_remaining: Option<usize>,
    pub queue_preserved: bool,
    pub disconnected_sessions: u64,
}

#[derive(Clone)]
pub struct RoutineSessionController {
    pool: Arc<PgPool>,
    health_registry: Option<Arc<HealthRegistry>>,
}

#[derive(Debug, Clone)]
struct RoutineSessionTarget {
    agent_id: String,
    provider: ProviderKind,
    channel_id: ChannelId,
    session_id: Option<i64>,
    session_key: Option<String>,
    tmux_session: String,
}

#[derive(Debug, Clone)]
struct RoutineSessionRow {
    id: i64,
    session_key: Option<String>,
    thread_channel_id: Option<String>,
}

impl RoutineSessionController {
    pub fn new(pool: Arc<PgPool>, health_registry: Option<Arc<HealthRegistry>>) -> Self {
        Self {
            pool,
            health_registry,
        }
    }

    pub async fn control_persistent_session(
        &self,
        routine: &RoutineRecord,
        command: RoutineSessionCommand,
        reason: &str,
    ) -> Result<RoutineSessionControlResult> {
        ensure_persistent_routine(routine)?;
        let target = self.resolve_target(routine).await?;
        let provider_clear_behavior = provider_clear_behavior(&target.provider);

        let mut runtime_cleared = false;
        let mut tmux_killed = false;
        let mut inflight_cleared = false;
        let mut lifecycle_path = "registry-unavailable";
        let mut queued_remaining = None;
        let mut queue_preserved = true;
        let mut disconnected_sessions = 0;

        match command {
            RoutineSessionCommand::Reset => {
                if let Some(registry) = self.health_registry.as_deref() {
                    runtime_cleared = clear_provider_channel_runtime(
                        registry,
                        target.provider.as_str(),
                        target.channel_id,
                        target.session_key.as_deref(),
                    )
                    .await;
                    lifecycle_path = if runtime_cleared {
                        "runtime-clear"
                    } else {
                        "runtime-clear-unavailable"
                    };
                }
            }
            RoutineSessionCommand::Kill => {
                let lifecycle = force_kill_turn(
                    self.health_registry.as_deref(),
                    &TurnLifecycleTarget {
                        provider: Some(target.provider.clone()),
                        channel_id: Some(target.channel_id),
                        tmux_name: target.tmux_session.clone(),
                    },
                    reason,
                    "routine_session_kill",
                )
                .await;
                tmux_killed = lifecycle.tmux_killed;
                inflight_cleared = lifecycle.inflight_cleared;
                lifecycle_path = lifecycle.lifecycle_path;
                queued_remaining = lifecycle.queue_depth;
                queue_preserved = lifecycle.queue_preserved;
                disconnected_sessions = self.disconnect_matching_sessions(&target).await?;
            }
        }

        Ok(RoutineSessionControlResult {
            action: command.as_str(),
            routine_id: routine.id.clone(),
            agent_id: target.agent_id,
            provider: target.provider.as_str().to_string(),
            channel_id: target.channel_id.get().to_string(),
            session_key: target.session_key,
            tmux_session: target.tmux_session,
            provider_clear_behavior,
            runtime_cleared,
            tmux_killed,
            inflight_cleared,
            lifecycle_path,
            queued_remaining,
            queue_preserved,
            disconnected_sessions,
        })
    }

    pub async fn teardown_fresh_session(
        &self,
        routine: &RoutineRecord,
        result_json: Option<&Value>,
        reason: &str,
    ) -> Result<RoutineSessionControlResult> {
        ensure_fresh_routine(routine)?;
        let target = self.resolve_fresh_target(routine, result_json).await?;
        let provider_clear_behavior = provider_clear_behavior(&target.provider);

        let lifecycle = force_kill_turn(
            self.health_registry.as_deref(),
            &TurnLifecycleTarget {
                provider: Some(target.provider.clone()),
                channel_id: Some(target.channel_id),
                tmux_name: target.tmux_session.clone(),
            },
            reason,
            "routine_fresh_session_teardown",
        )
        .await;
        let disconnected_sessions = self.disconnect_matching_sessions(&target).await?;

        Ok(RoutineSessionControlResult {
            action: "fresh_teardown",
            routine_id: routine.id.clone(),
            agent_id: target.agent_id,
            provider: target.provider.as_str().to_string(),
            channel_id: target.channel_id.get().to_string(),
            session_key: target.session_key,
            tmux_session: target.tmux_session,
            provider_clear_behavior,
            runtime_cleared: false,
            tmux_killed: lifecycle.tmux_killed,
            inflight_cleared: lifecycle.inflight_cleared,
            lifecycle_path: lifecycle.lifecycle_path,
            queued_remaining: lifecycle.queue_depth,
            queue_preserved: lifecycle.queue_preserved,
            disconnected_sessions,
        })
    }

    /// Resolves the ownership token a fresh routine run should persist at
    /// turn-start time (#3022), so boot recovery can later reap the exact
    /// session this run created. Reuses the same resolution as
    /// [`teardown_fresh_session`]; called immediately after the fresh session is
    /// created, when the latest session in the thread is unambiguously this
    /// run's session.
    ///
    /// Returns the resolved session's full `session_key` (`host:<tmux>`) ONLY
    /// when an actual started session row exists. The host/token prefix makes
    /// the token namespace-exact, so recovery never reaps a different token/host
    /// session sharing the deterministic `:<tmux>` suffix. Returns `None` when no
    /// concrete session row is resolvable yet: a *derived* fallback tmux name can
    /// diverge from the name `start_reserved_headless_agent_turn` actually used
    /// (e.g. a primary `channel_name_hint`, or a not-yet-thread-tagged row), so
    /// persisting it would make recovery reap a non-existent session while
    /// leaving the real orphan alive. Recording nothing keeps such a run out of
    /// the reap (idle-kill backstop still collects it) rather than guessing.
    pub async fn resolve_fresh_ownership_token(
        &self,
        routine: &RoutineRecord,
        result_json: Option<&Value>,
    ) -> Result<Option<String>> {
        ensure_fresh_routine(routine)?;
        let target = self.resolve_fresh_target(routine, result_json).await?;
        Ok(target.session_key)
    }

    /// Tears down the fresh routine session a recovered run recorded as owned,
    /// identified by its persisted ownership token (#3022). Unlike
    /// [`teardown_fresh_session`], this does NOT resolve "the latest session in
    /// the thread" — recovery passes the token the run recorded, so the reap
    /// targets that orphan precisely and can never collect an unrelated session
    /// that happens to be latest in a shared log thread.
    ///
    /// `ownership_token` is what [`resolve_fresh_ownership_token`] persisted:
    /// either a full `session_key` (`host:<tmux>`) or a bare tmux name. The tmux
    /// kill always uses the bare tmux name (local namespace); the DB disconnect
    /// is scoped to the resolved session row id, looked up namespace-exact when
    /// a full key was recorded so a different token/host session sharing the
    /// `:<tmux>` suffix is never disconnected.
    ///
    /// The provider/channel routing is still derived from the routine's agent
    /// bindings (needed so `force_kill_turn` clears the right runtime mailbox).
    pub async fn teardown_fresh_session_by_name(
        &self,
        routine: &RoutineRecord,
        ownership_token: &str,
        reason: &str,
    ) -> Result<RoutineSessionControlResult> {
        ensure_fresh_routine(routine)?;
        let ownership_token = ownership_token.trim();
        if ownership_token.is_empty() {
            return Err(anyhow!(
                "routine {} fresh teardown-by-name requires a non-empty ownership token",
                routine.id
            ));
        }
        // A full session_key is `host:<tmux>`; a bare token is the tmux name.
        let tmux_session = tmux_name_from_session_key(ownership_token).unwrap_or(ownership_token);
        let target = self
            .resolve_fresh_target_for_name(routine, ownership_token, tmux_session)
            .await?;
        let provider_clear_behavior = provider_clear_behavior(&target.provider);

        // NB: the owned session row's own status is NOT used to gate the reap.
        // After a dcserver restart `recover_stale_running_runs()` only marks the
        // run interrupted; it never touches `sessions`, so the stranded orphan
        // still carries its turn-start `turn_active`/`working` status. Skipping on
        // that status would skip the very orphan this reap exists to collect
        // (#3022). The "a replacement turn is live" decision is instead made by
        // the caller via `routine_has_other_running_run` — proof of a *different*
        // running run — before this teardown is ever invoked.
        //
        // #3022 P1: the local tmux kill uses the bare `<tmux>` name, which is
        // deterministic across hosts. When boot recovery runs on a different
        // cluster node / token namespace than the one that recorded
        // `owned_tmux_session`, a same-named LIVE local session can be an
        // unrelated process — killing it by bare name would violate the
        // positive-ownership guarantee. Gate the kill on the recorded key
        // belonging to this host; the namespace-exact DB disconnect still runs.
        let remote_owned = session_key_is_remote_owned(
            ownership_token,
            &crate::services::platform::shell::hostname_short(),
        );
        let lifecycle = if remote_owned {
            tracing::warn!(
                routine_id = %routine.id,
                ownership_token = %ownership_token,
                tmux = %target.tmux_session,
                "routine fresh teardown: owned session_key is remote-owned (host namespace differs from this node); skipping local tmux kill so an unrelated same-named live local session is not collaterally killed (#3022 P1). DB disconnect remains namespace-exact."
            );
            None
        } else {
            Some(
                force_kill_turn(
                    self.health_registry.as_deref(),
                    &TurnLifecycleTarget {
                        provider: Some(target.provider.clone()),
                        channel_id: Some(target.channel_id),
                        tmux_name: target.tmux_session.clone(),
                    },
                    reason,
                    "routine_fresh_session_teardown",
                )
                .await,
            )
        };
        let disconnected_sessions = self.disconnect_sessions_by_tmux(&target).await?;

        Ok(RoutineSessionControlResult {
            action: "fresh_teardown",
            routine_id: routine.id.clone(),
            agent_id: target.agent_id,
            provider: target.provider.as_str().to_string(),
            channel_id: target.channel_id.get().to_string(),
            session_key: target.session_key,
            tmux_session: target.tmux_session,
            provider_clear_behavior,
            runtime_cleared: false,
            tmux_killed: lifecycle.as_ref().is_some_and(|l| l.tmux_killed),
            inflight_cleared: lifecycle.as_ref().is_some_and(|l| l.inflight_cleared),
            lifecycle_path: lifecycle
                .as_ref()
                .map(|l| l.lifecycle_path)
                .unwrap_or("skipped_remote_owned_session"),
            queued_remaining: lifecycle.as_ref().and_then(|l| l.queue_depth),
            queue_preserved: lifecycle.as_ref().is_none_or(|l| l.queue_preserved),
            disconnected_sessions,
        })
    }

    /// Builds a teardown target pinned to a recovered run's owned session
    /// (#3022). The provider and primary channel come from the agent bindings
    /// (for runtime-mailbox routing); the channel is the routine log thread when
    /// known, otherwise the primary channel. The owned session row is resolved
    /// namespace-exact from `ownership_token` when it is a full `session_key`,
    /// or by the bare tmux suffix otherwise, so the disconnect targets only that
    /// exact row.
    ///
    async fn resolve_fresh_target_for_name(
        &self,
        routine: &RoutineRecord,
        ownership_token: &str,
        tmux_session: &str,
    ) -> Result<RoutineSessionTarget> {
        let agent_id = routine_agent_id(routine, "fresh routine session teardown by name")?;

        let bindings = crate::db::agents::load_agent_channel_bindings_pg(&self.pool, &agent_id)
            .await
            .map_err(|error| {
                anyhow!("load agent bindings for fresh routine session {agent_id}: {error}")
            })?
            .ok_or_else(|| anyhow!("agent {agent_id} not found for fresh routine teardown"))?;
        let provider = bindings
            .resolved_primary_provider_kind()
            .ok_or_else(|| anyhow!("agent {agent_id} primary provider is not configured"))?;
        let primary_channel = bindings
            .primary_channel()
            .ok_or_else(|| anyhow!("agent {agent_id} primary channel is not configured"))?;
        let primary_channel_id =
            crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&primary_channel)
                .or_else(|| primary_channel.parse::<u64>().ok())
                .ok_or_else(|| {
                    anyhow!("agent {agent_id} primary channel is invalid: {primary_channel}")
                })?;
        let routine_thread_channel_id = routine
            .discord_thread_id
            .as_deref()
            .and_then(parse_discord_channel_id);

        let session = self
            .load_owned_session(&agent_id, &provider, ownership_token, tmux_session)
            .await?;
        let session_id = session.as_ref().map(|row| row.id);
        let session_key = session.as_ref().and_then(|row| row.session_key.clone());

        Ok(RoutineSessionTarget {
            agent_id,
            provider,
            channel_id: ChannelId::new(routine_thread_channel_id.unwrap_or(primary_channel_id)),
            session_id,
            session_key,
            tmux_session: tmux_session.to_string(),
        })
    }

    async fn resolve_target(&self, routine: &RoutineRecord) -> Result<RoutineSessionTarget> {
        let agent_id = routine
            .agent_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "persistent routine {} is not attached to an agent",
                    routine.id
                )
            })?
            .to_string();

        let bindings = crate::db::agents::load_agent_channel_bindings_pg(&self.pool, &agent_id)
            .await
            .map_err(|error| {
                anyhow!("load agent bindings for routine session {agent_id}: {error}")
            })?
            .ok_or_else(|| anyhow!("agent {agent_id} not found for routine session control"))?;
        let provider = bindings
            .resolved_primary_provider_kind()
            .ok_or_else(|| anyhow!("agent {agent_id} primary provider is not configured"))?;
        let primary_channel = bindings
            .primary_channel()
            .ok_or_else(|| anyhow!("agent {agent_id} primary channel is not configured"))?;
        let channel_id =
            crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&primary_channel)
                .or_else(|| primary_channel.parse::<u64>().ok())
                .ok_or_else(|| {
                    anyhow!("agent {agent_id} primary channel is invalid: {primary_channel}")
                })?;
        let routine_thread_channel_id = routine
            .discord_thread_id
            .as_deref()
            .and_then(parse_discord_channel_id);

        let session = self
            .load_latest_session(&agent_id, &provider, channel_id, routine_thread_channel_id)
            .await?;
        let session_thread_channel_id = session
            .as_ref()
            .and_then(|row| row.thread_channel_id.as_deref())
            .and_then(parse_discord_channel_id);
        let target_channel_id = target_channel_id(
            channel_id,
            routine_thread_channel_id,
            session_thread_channel_id,
        );
        let channel = ChannelId::new(target_channel_id);
        let fallback_channel_name =
            fallback_tmux_channel_name(&primary_channel, channel_id, target_channel_id);
        let session_id = session.as_ref().map(|row| row.id);
        let session_key = session.as_ref().and_then(|row| row.session_key.clone());
        let tmux_session = session_key
            .as_deref()
            .and_then(tmux_name_from_session_key)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| provider.build_tmux_session_name(&fallback_channel_name));

        Ok(RoutineSessionTarget {
            agent_id,
            provider,
            channel_id: channel,
            session_id,
            session_key,
            tmux_session,
        })
    }

    async fn resolve_fresh_target(
        &self,
        routine: &RoutineRecord,
        result_json: Option<&Value>,
    ) -> Result<RoutineSessionTarget> {
        let agent_id = routine_agent_id(routine, "fresh routine session teardown")?;

        let bindings = crate::db::agents::load_agent_channel_bindings_pg(&self.pool, &agent_id)
            .await
            .map_err(|error| {
                anyhow!("load agent bindings for fresh routine session {agent_id}: {error}")
            })?
            .ok_or_else(|| anyhow!("agent {agent_id} not found for fresh routine teardown"))?;
        let provider = bindings
            .resolved_primary_provider_kind()
            .ok_or_else(|| anyhow!("agent {agent_id} primary provider is not configured"))?;
        let primary_channel = bindings
            .primary_channel()
            .ok_or_else(|| anyhow!("agent {agent_id} primary channel is not configured"))?;
        let primary_channel_id =
            crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&primary_channel)
                .or_else(|| primary_channel.parse::<u64>().ok())
                .ok_or_else(|| {
                    anyhow!("agent {agent_id} primary channel is invalid: {primary_channel}")
                })?;
        let routine_thread_channel_id =
            fresh_teardown_thread_channel_id(routine, result_json).ok_or_else(|| {
                anyhow!(
                    "fresh routine {} has no routine thread id; refusing to teardown primary agent session",
                    routine.id
                )
            })?;

        let session = self
            .load_latest_thread_session(&agent_id, &provider, routine_thread_channel_id)
            .await?;
        let session_id = session.as_ref().map(|row| row.id);
        let session_key = session.as_ref().and_then(|row| row.session_key.clone());
        let tmux_session = session_key
            .as_deref()
            .and_then(tmux_name_from_session_key)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                provider.build_tmux_session_name(&fallback_tmux_channel_name(
                    &primary_channel,
                    primary_channel_id,
                    routine_thread_channel_id,
                ))
            });

        Ok(RoutineSessionTarget {
            agent_id,
            provider,
            channel_id: ChannelId::new(routine_thread_channel_id),
            session_id,
            session_key,
            tmux_session,
        })
    }

    async fn load_latest_session(
        &self,
        agent_id: &str,
        provider: &ProviderKind,
        primary_channel_id: u64,
        routine_thread_channel_id: Option<u64>,
    ) -> Result<Option<RoutineSessionRow>> {
        let primary_channel_id = primary_channel_id.to_string();
        let routine_thread_channel_id = routine_thread_channel_id.map(|value| value.to_string());
        let row = sqlx::query(
            r#"
            SELECT id, session_key, thread_channel_id
            FROM sessions
            WHERE agent_id = $1
              AND LOWER(COALESCE(provider, '')) = LOWER($2)
              AND status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle', 'connected')
              AND (
                thread_channel_id = $3
                OR ($4::text IS NOT NULL AND thread_channel_id = $4)
                OR thread_channel_id IS NULL
              )
            ORDER BY
              CASE
                WHEN $4::text IS NOT NULL AND thread_channel_id = $4 THEN 0
                WHEN thread_channel_id = $3 THEN 1
                ELSE 2
              END,
              CASE status
                WHEN 'turn_active' THEN 0
                WHEN 'working' THEN 0
                WHEN 'awaiting_bg' THEN 1
                WHEN 'awaiting_user' THEN 2
                WHEN 'idle' THEN 3
                WHEN 'connected' THEN 4
                ELSE 5
              END,
              last_heartbeat DESC NULLS LAST,
              created_at DESC
            LIMIT 1
            "#,
        )
        .bind(agent_id)
        .bind(provider.as_str())
        .bind(primary_channel_id)
        .bind(routine_thread_channel_id)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| anyhow!("load routine session for {agent_id}: {error}"))?;

        row.map(|row| {
            Ok(RoutineSessionRow {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode routine session id: {error}"))?,
                session_key: row
                    .try_get("session_key")
                    .map_err(|error| anyhow!("decode routine session_key: {error}"))?,
                thread_channel_id: row
                    .try_get("thread_channel_id")
                    .map_err(|error| anyhow!("decode routine thread_channel_id: {error}"))?,
            })
        })
        .transpose()
    }

    async fn load_latest_thread_session(
        &self,
        agent_id: &str,
        provider: &ProviderKind,
        thread_channel_id: u64,
    ) -> Result<Option<RoutineSessionRow>> {
        let thread_channel_id = thread_channel_id.to_string();
        let thread_suffix_regex = format!("-t{}(-dev)?$", thread_channel_id);
        let row = sqlx::query(
            r#"
            SELECT id, session_key, thread_channel_id
            FROM sessions
            WHERE agent_id = $1
              AND LOWER(COALESCE(provider, '')) = LOWER($2)
              AND status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working', 'idle', 'connected')
              AND (
                thread_channel_id = $3
                OR session_key ~ $4
              )
            ORDER BY
              CASE WHEN thread_channel_id = $3 THEN 0 ELSE 1 END,
              CASE status
                WHEN 'turn_active' THEN 0
                WHEN 'working' THEN 0
                WHEN 'awaiting_bg' THEN 1
                WHEN 'awaiting_user' THEN 2
                WHEN 'idle' THEN 3
                WHEN 'connected' THEN 4
                ELSE 5
              END,
              last_heartbeat DESC NULLS LAST,
              created_at DESC
            LIMIT 1
            "#,
        )
        .bind(agent_id)
        .bind(provider.as_str())
        .bind(thread_channel_id)
        .bind(thread_suffix_regex)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| anyhow!("load routine thread session for {agent_id}: {error}"))?;

        row.map(|row| {
            Ok(RoutineSessionRow {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode routine session id: {error}"))?,
                session_key: row
                    .try_get("session_key")
                    .map_err(|error| anyhow!("decode routine session_key: {error}"))?,
                thread_channel_id: row
                    .try_get("thread_channel_id")
                    .map_err(|error| anyhow!("decode routine thread_channel_id: {error}"))?,
            })
        })
        .transpose()
    }

    /// Loads the single session row a recovered fresh run owns (#3022), scoped
    /// to this agent + provider. When `ownership_token` is a full `session_key`
    /// (`host:<tmux>`) the match is namespace-exact (`session_key = token`), so a
    /// different token/host session sharing the `:<tmux>` suffix is never picked
    /// up. When it is a bare tmux name (legacy/fallback), the match is by the
    /// `:<tmux>` suffix, preferring the most recently active row. Returns `None`
    /// when the owned session is already gone (idempotent — recovery may re-run,
    /// or the orphan may already be reaped).
    async fn load_owned_session(
        &self,
        agent_id: &str,
        provider: &ProviderKind,
        ownership_token: &str,
        tmux_session: &str,
    ) -> Result<Option<RoutineSessionRow>> {
        let is_full_session_key = ownership_token != tmux_session;
        // `:` anchors the host/tmux split (see `tmux_name_from_session_key`);
        // tmux names are `[A-Za-z0-9_-]` so this regex needs no escaping.
        let session_key_suffix_regex = format!(":{tmux_session}$");
        let row = sqlx::query(
            r#"
            SELECT id, session_key, thread_channel_id
            FROM sessions
            WHERE agent_id = $1
              AND LOWER(COALESCE(provider, '')) = LOWER($2)
              AND (
                ($3::bool AND session_key = $4)
                OR (NOT $3::bool AND session_key ~ $5)
              )
            ORDER BY last_heartbeat DESC NULLS LAST, created_at DESC
            LIMIT 1
            "#,
        )
        .bind(agent_id)
        .bind(provider.as_str())
        .bind(is_full_session_key)
        .bind(ownership_token)
        .bind(session_key_suffix_regex)
        .fetch_optional(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!("load routine owned session {ownership_token} for {agent_id}: {error}")
        })?;

        row.map(|row| {
            Ok(RoutineSessionRow {
                id: row
                    .try_get("id")
                    .map_err(|error| anyhow!("decode routine session id: {error}"))?,
                session_key: row
                    .try_get("session_key")
                    .map_err(|error| anyhow!("decode routine session_key: {error}"))?,
                thread_channel_id: row
                    .try_get("thread_channel_id")
                    .map_err(|error| anyhow!("decode routine thread_channel_id: {error}"))?,
            })
        })
        .transpose()
    }

    /// Disconnects only the single session row resolved for the owned tmux
    /// session (#3022). Scoped strictly to the resolved primary key
    /// (`session_id`) — never a tmux-name suffix match — so it can never touch
    /// an unrelated session that merely shares the deterministic tmux name in a
    /// different token/host namespace (the `session_key` host/token prefix
    /// differs even when the `:<tmux>` suffix matches). If the owned session row
    /// is already gone (`session_id` is `None`), nothing is disconnected, which
    /// is the correct idempotent no-op for a re-run recovery pass.
    async fn disconnect_sessions_by_tmux(&self, target: &RoutineSessionTarget) -> Result<u64> {
        let Some(session_id) = target.session_id else {
            return Ok(0);
        };
        let result = sqlx::query(
            r#"
            UPDATE sessions
            SET status = 'disconnected',
                active_dispatch_id = NULL,
                claude_session_id = NULL,
                raw_provider_session_id = NULL
            WHERE id = $1
              AND status <> 'disconnected'
            "#,
        )
        .bind(session_id)
        .execute(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "disconnect routine session by id {session_id} (tmux {}) for {}: {error}",
                target.tmux_session,
                target.agent_id
            )
        })?;

        Ok(result.rows_affected())
    }

    async fn disconnect_matching_sessions(&self, target: &RoutineSessionTarget) -> Result<u64> {
        let session_key = target.session_key.as_deref();
        let target_channel_id = target.channel_id.get().to_string();
        let result = sqlx::query(
            r#"
            UPDATE sessions
            SET status = 'disconnected',
                active_dispatch_id = NULL,
                claude_session_id = NULL,
                raw_provider_session_id = NULL
            WHERE agent_id = $1
              AND LOWER(COALESCE(provider, '')) = LOWER($2)
              AND status <> 'disconnected'
              AND (
                ($3::bigint IS NOT NULL AND id = $3)
                OR ($4::text IS NOT NULL AND thread_channel_id = $4)
                OR ($5::text IS NOT NULL AND session_key = $5)
              )
            "#,
        )
        .bind(&target.agent_id)
        .bind(target.provider.as_str())
        .bind(target.session_id)
        .bind(target_channel_id)
        .bind(session_key)
        .execute(&*self.pool)
        .await
        .map_err(|error| {
            anyhow!(
                "disconnect routine sessions for {}: {error}",
                target.agent_id
            )
        })?;

        Ok(result.rows_affected())
    }
}

fn ensure_persistent_routine(routine: &RoutineRecord) -> Result<()> {
    if routine.execution_strategy == "persistent" {
        Ok(())
    } else {
        Err(anyhow!(
            "routine {} requires execution_strategy=persistent for session control",
            routine.id
        ))
    }
}

fn ensure_fresh_routine(routine: &RoutineRecord) -> Result<()> {
    if routine.execution_strategy == "fresh" {
        Ok(())
    } else {
        Err(anyhow!(
            "routine {} requires execution_strategy=fresh for automatic session teardown",
            routine.id
        ))
    }
}

fn routine_agent_id(routine: &RoutineRecord, context: &str) -> Result<String> {
    routine
        .agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            anyhow!(
                "routine {} is not attached to an agent for {context}",
                routine.id
            )
        })
}

fn string_field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value?
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn fresh_teardown_thread_channel_id(
    routine: &RoutineRecord,
    result_json: Option<&Value>,
) -> Option<u64> {
    routine
        .discord_thread_id
        .as_deref()
        .and_then(parse_discord_channel_id)
        .or_else(|| {
            string_field(result_json, "discord_thread_id").and_then(parse_discord_channel_id)
        })
        .or_else(|| {
            let channel_id =
                string_field(result_json, "channel_id").and_then(parse_discord_channel_id)?;
            let parent_channel_id =
                string_field(result_json, "parent_channel_id").and_then(parse_discord_channel_id);
            if Some(channel_id) == parent_channel_id {
                None
            } else {
                Some(channel_id)
            }
        })
}

fn tmux_name_from_session_key(session_key: &str) -> Option<&str> {
    session_key
        .split_once(':')
        .map(|(_, tmux_name)| tmux_name)
        .filter(|value| !value.trim().is_empty())
}

/// True when `ownership_token` is a full `host:<tmux>` session_key whose host
/// namespace differs from `local_host`. A remote-owned key must NOT drive a
/// local tmux kill: the bare `<tmux>` name is deterministic, so a same-named
/// LIVE local session could be an unrelated process on this host (#3022 P1).
/// A bare token (no `host:` prefix) is treated as local.
fn session_key_is_remote_owned(ownership_token: &str, local_host: &str) -> bool {
    ownership_token
        .split_once(':')
        .map(|(host, _)| host.trim())
        .filter(|host| !host.is_empty())
        .is_some_and(|host| host != local_host.trim())
}

fn parse_discord_channel_id(value: &str) -> Option<u64> {
    value.trim().parse::<u64>().ok()
}

fn target_channel_id(
    primary_channel_id: u64,
    routine_thread_channel_id: Option<u64>,
    session_thread_channel_id: Option<u64>,
) -> u64 {
    session_thread_channel_id
        .or(routine_thread_channel_id)
        .unwrap_or(primary_channel_id)
}

fn fallback_tmux_channel_name(
    primary_channel: &str,
    primary_channel_id: u64,
    target_channel_id: u64,
) -> String {
    if target_channel_id == primary_channel_id {
        primary_channel.to_string()
    } else {
        format!("{primary_channel}-t{target_channel_id}")
    }
}

pub fn provider_clear_behavior(provider: &ProviderKind) -> &'static str {
    if *provider == ProviderKind::Claude {
        "runtime clear plus /clear in the existing Claude tmux session"
    } else if provider.uses_managed_tmux_backend() {
        "runtime clear plus managed process session reset for the provider tmux session"
    } else {
        "runtime mailbox clear only; provider has no managed tmux reset hook"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn routine_with_thread(
        execution_strategy: &str,
        discord_thread_id: Option<&str>,
    ) -> RoutineRecord {
        RoutineRecord {
            id: "routine-1".to_string(),
            agent_id: Some("agent-1".to_string()),
            fallback_agent_id: None,
            max_retries: 0,
            script_ref: "script".to_string(),
            name: "Routine".to_string(),
            status: "enabled".to_string(),
            execution_strategy: execution_strategy.to_string(),
            schedule: None,
            next_due_at: None,
            last_run_at: None,
            last_result: None,
            checkpoint: None,
            discord_thread_id: discord_thread_id.map(ToOwned::to_owned),
            timeout_secs: None,
            in_flight_run_id: None,
            pause_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn tmux_name_from_session_key_uses_suffix_after_host() {
        assert_eq!(
            tmux_name_from_session_key("host:AgentDesk-codex-sandbox-cdx"),
            Some("AgentDesk-codex-sandbox-cdx")
        );
        assert_eq!(tmux_name_from_session_key("missing-separator"), None);
        assert_eq!(tmux_name_from_session_key("host:"), None);
    }

    #[test]
    fn session_key_remote_owned_gates_local_tmux_kill() {
        // Full key whose host differs from this node → remote-owned, skip kill.
        assert!(session_key_is_remote_owned(
            "other-node:AgentDesk-codex-sandbox-cdx",
            "this-node"
        ));
        // Full key written by this node → local, kill proceeds.
        assert!(!session_key_is_remote_owned(
            "this-node:AgentDesk-codex-sandbox-cdx",
            "this-node"
        ));
        // Bare token (no host prefix) is treated as local.
        assert!(!session_key_is_remote_owned(
            "AgentDesk-codex-sandbox-cdx",
            "this-node"
        ));
        // Empty host prefix is not a remote namespace claim.
        assert!(!session_key_is_remote_owned(":AgentDesk-bare", "this-node"));
        // Hostname comparison ignores surrounding whitespace.
        assert!(!session_key_is_remote_owned(
            "this-node:AgentDesk-x",
            " this-node "
        ));
    }

    #[test]
    fn provider_clear_behavior_documents_supported_reset_paths() {
        assert!(provider_clear_behavior(&ProviderKind::Claude).contains("/clear"));
        assert!(provider_clear_behavior(&ProviderKind::Codex).contains("managed process"));
        assert!(provider_clear_behavior(&ProviderKind::Gemini).contains("mailbox clear only"));
    }

    #[test]
    fn target_channel_prefers_session_thread_then_routine_thread_then_primary() {
        assert_eq!(target_channel_id(100, Some(200), Some(300)), 300);
        assert_eq!(target_channel_id(100, Some(200), None), 200);
        assert_eq!(target_channel_id(100, None, None), 100);
    }

    #[test]
    fn fallback_tmux_channel_name_preserves_thread_suffix() {
        assert_eq!(
            fallback_tmux_channel_name("agent-cdx", 100, 100),
            "agent-cdx"
        );
        assert_eq!(
            fallback_tmux_channel_name("agent-cdx", 100, 200),
            "agent-cdx-t200"
        );
    }

    #[test]
    fn fresh_teardown_prefers_routine_thread_and_never_primary_channel() {
        let routine = routine_with_thread("fresh", Some("300"));
        let result = json!({
            "channel_id": "200",
            "parent_channel_id": "100",
            "discord_thread_id": "200"
        });
        assert_eq!(
            fresh_teardown_thread_channel_id(&routine, Some(&result)),
            Some(300)
        );

        let routine = routine_with_thread("fresh", None);
        let primary_result = json!({
            "channel_id": "100",
            "parent_channel_id": "100"
        });
        assert_eq!(
            fresh_teardown_thread_channel_id(&routine, Some(&primary_result)),
            None
        );

        let thread_result = json!({
            "channel_id": "200",
            "parent_channel_id": "100"
        });
        assert_eq!(
            fresh_teardown_thread_channel_id(&routine, Some(&thread_result)),
            Some(200)
        );

        let metadata_fallback = json!({
            "channel_id": "100",
            "parent_channel_id": "100",
            "discord_thread_id": "400"
        });
        assert_eq!(
            fresh_teardown_thread_channel_id(&routine, Some(&metadata_fallback)),
            Some(400)
        );
    }

    #[test]
    fn fresh_teardown_rejects_persistent_routine() {
        let routine = routine_with_thread("persistent", Some("300"));
        assert!(ensure_fresh_routine(&routine).is_err());
    }
}
