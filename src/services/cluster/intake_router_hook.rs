//! Leader-side intake-routing hook. Phase 4 of intake-node-routing
//! (docs/design/intake-node-routing.md).
//!
//! Sits in the leader's Discord intake gate immediately before
//! `handle_text_message`. For each incoming message, decides:
//!
//! 1. Does the agent for this channel opt into worker forwarding
//!    (`agents.preferred_intake_node_labels` non-empty)?
//! 2. If yes, is there an eligible worker right now (online +
//!    superset of the requested labels)?
//! 3. If yes AND mode is `Enforce`, INSERT a row into `intake_outbox`
//!    and signal the caller to skip local execution. The worker's
//!    poll loop (Phase 3) picks it up.
//! 4. If yes AND mode is `Observe`, log the would-be forward but let
//!    the leader run it locally — that's how we dark-launch the
//!    routing decisions before flipping `Enforce`.
//! 5. If anything fails (no eligible worker, INSERT conflict on the
//!    partial unique index, DB timeout), fall back to local execution
//!    so a routing failure can never lose an intake.
//!
//! Phase 5 will add the env-var driven config and ops CLI; this PR
//! ships the hook + observe-mode plumbing only.

use crate::db::intake_outbox::{
    InsertPendingPayload, IntakeInsertConflict, classify_insert_pending_error, insert_pending,
};
use crate::services::cluster::intake_routing::{
    IntakeRouteTarget, LocalRouteReason, candidates_from_worker_nodes_json, pick_intake_target,
};
use sqlx::PgPool;

/// How aggressively to apply the Phase-2 routing decision in front of
/// the existing leader intake path. Phase 5 promotes the config from
/// an env-var snapshot (read at startup) into per-agent overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRoutingMode {
    /// Hook is a no-op; the leader runs every intake locally as today.
    /// Default until Phase 5 flips the global flag — keeps prod
    /// behaviour byte-identical while phases 1-4 land.
    Disabled,
    /// The hook does the full decision (label match, worker eligibility,
    /// 23505 classification) but never INSERTs. Logs the decision so
    /// operators can verify routing behaviour against real traffic
    /// before promoting to `Enforce`.
    Observe,
    /// The hook actually INSERTs into `intake_outbox` and tells the
    /// caller to skip local execution.
    Enforce,
}

impl IntakeRoutingMode {
    /// Read the global mode from the `ADK_INTAKE_ROUTING_MODE` env
    /// var, defaulting to `Disabled` for unrecognised / unset values.
    /// Phase 5 ops CLI will toggle this at runtime via per-agent rows.
    pub(crate) fn from_env() -> Self {
        match std::env::var("ADK_INTAKE_ROUTING_MODE")
            .ok()
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("observe") => Self::Observe,
            Some("enforce") => Self::Enforce,
            _ => Self::Disabled,
        }
    }
}

/// What the hook decided. The intake gate uses this to choose between
/// "skip local execution; the worker has the row" and "fall through
/// to `handle_text_message` as today".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRouterDecision {
    /// No forwarding happened (Disabled mode, no preference, no
    /// eligible worker, OpenRoute conflict, DB error). The caller
    /// MUST run the turn locally.
    RanLocal { reason: RanLocalReason },
    /// Observe mode would have forwarded but the caller MUST still
    /// run locally. The chosen target is reported for logging only.
    ObservedWouldForward { target_instance_id: String },
    /// The hook inserted a row for the worker. The caller MUST NOT
    /// run the turn locally — that would double-emit the Discord turn.
    /// `outbox_id` is the row's PK for log correlation.
    Forwarded {
        target_instance_id: String,
        outbox_id: i64,
    },
    /// At-most-once skip: Discord redelivered the same
    /// `(channel_id, user_msg_id)` and the 3-tuple unique constraint
    /// rejected the attempt-1 INSERT. An earlier path already covers
    /// the message. Caller MUST NOT run the turn locally — running it
    /// would double-emit. Distinct from `RanLocal { DbErrorFellBack }`
    /// because the gate's response differs (skip vs run-local).
    SkippedDuplicate,
}

/// Diagnostic enum for `RanLocal` — keeps the metric surface and
/// operator-log telemetry stable across the three "no-op" code paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RanLocalReason {
    /// Mode is `Disabled` (Phase 1-4 default).
    HookDisabled,
    /// Agent opted out (`preferred_intake_node_labels` empty).
    AgentHasNoPreference,
    /// Agent opted in but no worker matches (offline, missing labels).
    NoEligibleWorker,
    /// Agent opted in and a worker matched, but the only eligible
    /// candidate IS the leader.
    LeaderIsOnlyEligible,
    /// Routing decision succeeded and `Enforce` tried to INSERT, but
    /// the partial unique index says another row for this channel is
    /// already OPEN. Falling back to local is the design's chosen
    /// recovery path.
    OpenRouteAlreadyExists,
    /// Some DB or schema error during the routing decision. Reported
    /// so operators see WHY a forward turned into a local fallback.
    DbErrorFellBackToLocal { detail: String },
    /// `Disabled` mode looked up the agent and found a non-empty
    /// preference; recorded for the eventual cutover (Phase 5 ops
    /// uses this to find agents whose preferences are set but
    /// not yet enforced).
    DisabledButPreferenceSet,
    /// Agent for this channel could not be looked up (channel not
    /// mapped to an agent). Treated as no-preference.
    NoAgentForChannel,
}

/// Inputs to the hook. Bundled into a struct so the intake gate can
/// thread per-channel context cleanly without a 6-argument fn call.
#[derive(Clone, Debug)]
pub(crate) struct IntakeRouterContext<'a> {
    pub mode: IntakeRoutingMode,
    pub leader_instance_id: &'a str,
    pub channel_id: &'a str,
    pub user_msg_id: &'a str,
    pub request_owner_id: &'a str,
    pub request_owner_name: Option<&'a str>,
    pub user_text: &'a str,
    pub reply_context: Option<&'a str>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: &'a str,
    pub merge_consecutive: bool,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
}

/// Heartbeat lease window for treating a worker_node row as fresh.
/// Mirrors the dispatch routing engine's default; Phase 5 will allow
/// per-cluster override via config.
const WORKER_HEARTBEAT_LEASE_SECS: u64 = 30;

/// Run the hook. Never fails — every error path turns into
/// `RanLocal { reason: DbErrorFellBackToLocal }` because losing an
/// intake message is a strictly worse failure mode than executing
/// it on the leader.
pub(crate) async fn try_route_intake(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
) -> IntakeRouterDecision {
    if matches!(ctx.mode, IntakeRoutingMode::Disabled) {
        // Disabled-but-preference-set is reported separately so Phase 5
        // operators can spot agents whose label preferences are set
        // but the global mode hasn't been flipped yet.
        let preference_set = match agent_preferred_labels_for_channel(pool, ctx.channel_id).await {
            Ok(Some(labels)) => !labels.is_empty(),
            _ => false,
        };
        return IntakeRouterDecision::RanLocal {
            reason: if preference_set {
                RanLocalReason::DisabledButPreferenceSet
            } else {
                RanLocalReason::HookDisabled
            },
        };
    }

    // Resolve agent + preference. NoAgentForChannel is NOT an error —
    // many channels (DMs, ad-hoc cross-bot) have no agent row.
    let (agent_id, preferred_labels) =
        match agent_id_and_preferred_labels(pool, ctx.channel_id).await {
            Ok(Some((agent_id, labels))) => (agent_id, labels),
            Ok(None) => {
                return IntakeRouterDecision::RanLocal {
                    reason: RanLocalReason::NoAgentForChannel,
                };
            }
            Err(error) => {
                return IntakeRouterDecision::RanLocal {
                    reason: RanLocalReason::DbErrorFellBackToLocal {
                        detail: format!("agent lookup: {error}"),
                    },
                };
            }
        };

    if preferred_labels.is_empty() {
        return IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::AgentHasNoPreference,
        };
    }

    let candidates =
        match crate::server::cluster::list_worker_nodes(pool, WORKER_HEARTBEAT_LEASE_SECS).await {
            Ok(nodes) => candidates_from_worker_nodes_json(&nodes),
            Err(error) => {
                return IntakeRouterDecision::RanLocal {
                    reason: RanLocalReason::DbErrorFellBackToLocal {
                        detail: format!("list worker_nodes: {error}"),
                    },
                };
            }
        };

    let target = match pick_intake_target(&candidates, &preferred_labels, ctx.leader_instance_id) {
        IntakeRouteTarget::Worker { instance_id } => instance_id,
        IntakeRouteTarget::Local { reason } => {
            return IntakeRouterDecision::RanLocal {
                reason: match reason {
                    LocalRouteReason::NoPreference => RanLocalReason::AgentHasNoPreference,
                    LocalRouteReason::NoEligibleWorker => RanLocalReason::NoEligibleWorker,
                    LocalRouteReason::LeaderIsOnlyEligible => RanLocalReason::LeaderIsOnlyEligible,
                },
            };
        }
    };

    if matches!(ctx.mode, IntakeRoutingMode::Observe) {
        tracing::info!(
            target_instance_id = %target,
            channel_id = ctx.channel_id,
            user_msg_id = ctx.user_msg_id,
            agent_id = %agent_id,
            "[intake_router] OBSERVE: would forward to worker (running locally)"
        );
        return IntakeRouterDecision::ObservedWouldForward {
            target_instance_id: target,
        };
    }

    // Enforce mode: INSERT the row. Live ingress is always
    // `attempt_no = 1` (codex Phase 4 blocker #3) — the
    // `family_max + 1` retry shape is reserved for the
    // `failed_pre_accept` retry path with a `parent_outbox_id`. A
    // `DuplicateMessageAttempt` 23505 here means Discord redelivered
    // the same message and we already accepted it once; running it
    // again would double-emit, so fall back to local-no-op rather
    // than allocating a fresh attempt_no.
    let payload = build_payload_for_insert(ctx, &target, &preferred_labels, &agent_id);

    match insert_pending(pool, &payload, 1, None).await {
        Ok(outbox_id) => IntakeRouterDecision::Forwarded {
            target_instance_id: target,
            outbox_id,
        },
        Err(error) => match classify_insert_pending_error(&error) {
            Some(IntakeInsertConflict::OpenRoutePerChannel) => IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::OpenRouteAlreadyExists,
            },
            Some(IntakeInsertConflict::DuplicateMessageAttempt) => {
                // Discord delivered the same message twice (or the leader
                // received it via two different intake paths). The first
                // delivery already produced a row; honour at-most-once
                // and skip — running it again would double-emit.
                tracing::info!(
                    channel_id = ctx.channel_id,
                    user_msg_id = ctx.user_msg_id,
                    "[intake_router] duplicate Discord message — existing row already covers it; skipping local execution"
                );
                IntakeRouterDecision::SkippedDuplicate
            }
            None => IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::DbErrorFellBackToLocal {
                    detail: format!("insert_pending: {error}"),
                },
            },
        },
    }
}

fn build_payload_for_insert(
    ctx: &IntakeRouterContext<'_>,
    target: &str,
    preferred_labels: &[String],
    agent_id: &str,
) -> InsertPendingPayload {
    InsertPendingPayload {
        target_instance_id: target.to_string(),
        forwarded_by_instance_id: ctx.leader_instance_id.to_string(),
        required_labels: serde_json::Value::Array(
            preferred_labels
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        ),
        channel_id: ctx.channel_id.to_string(),
        user_msg_id: ctx.user_msg_id.to_string(),
        request_owner_id: ctx.request_owner_id.to_string(),
        request_owner_name: ctx.request_owner_name.map(str::to_string),
        user_text: ctx.user_text.to_string(),
        reply_context: ctx.reply_context.map(str::to_string),
        has_reply_boundary: ctx.has_reply_boundary,
        dm_hint: ctx.dm_hint,
        // Phase 4 codex follow-up: leader emits canonical "foreground"
        // for `TurnKind::Foreground`; the worker's `parse_turn_kind`
        // accepts both "foreground" and "standard" for backwards
        // compatibility with rows already in the queue.
        turn_kind: ctx.turn_kind.to_string(),
        merge_consecutive: ctx.merge_consecutive,
        reply_to_user_message: ctx.reply_to_user_message,
        defer_watcher_resume: ctx.defer_watcher_resume,
        wait_for_completion: ctx.wait_for_completion,
        agent_id: agent_id.to_string(),
    }
}

/// Look up the agent_id + `preferred_intake_node_labels` for a channel.
/// Returns `Ok(None)` when the channel isn't mapped to any agent.
async fn agent_id_and_preferred_labels(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<(String, Vec<String>)>, sqlx::Error> {
    let row: Option<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, preferred_intake_node_labels FROM agents
         WHERE discord_channel_id = $1
            OR discord_channel_alt = $1
            OR discord_channel_cc = $1
            OR discord_channel_cdx = $1
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;

    let Some((agent_id, labels_value)) = row else {
        return Ok(None);
    };

    let labels: Vec<String> = labels_value
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Ok(Some((agent_id, labels)))
}

/// Variant used by the `Disabled` branch — only the labels matter.
async fn agent_preferred_labels_for_channel(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<Vec<String>>, sqlx::Error> {
    Ok(agent_id_and_preferred_labels(pool, channel_id)
        .await?
        .map(|(_, labels)| labels))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn intake_routing_mode_from_env_parses_observe_enforce_disabled() {
        // SAFETY: tests run serially within a thread, but env state is
        // process-global. Restore on drop.
        struct EnvGuard {
            previous: Option<String>,
        }
        impl EnvGuard {
            fn set(value: Option<&str>) -> Self {
                let previous = std::env::var("ADK_INTAKE_ROUTING_MODE").ok();
                match value {
                    Some(v) => unsafe { std::env::set_var("ADK_INTAKE_ROUTING_MODE", v) },
                    None => unsafe { std::env::remove_var("ADK_INTAKE_ROUTING_MODE") },
                }
                Self { previous }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.previous {
                    Some(v) => unsafe { std::env::set_var("ADK_INTAKE_ROUTING_MODE", v) },
                    None => unsafe { std::env::remove_var("ADK_INTAKE_ROUTING_MODE") },
                }
            }
        }

        {
            let _g = EnvGuard::set(Some("observe"));
            assert_eq!(IntakeRoutingMode::from_env(), IntakeRoutingMode::Observe);
        }
        {
            let _g = EnvGuard::set(Some("ENFORCE"));
            assert_eq!(IntakeRoutingMode::from_env(), IntakeRoutingMode::Enforce);
        }
        {
            let _g = EnvGuard::set(Some("garbage"));
            assert_eq!(IntakeRoutingMode::from_env(), IntakeRoutingMode::Disabled);
        }
        {
            let _g = EnvGuard::set(None);
            assert_eq!(IntakeRoutingMode::from_env(), IntakeRoutingMode::Disabled);
        }
    }
}

#[cfg(test)]
mod pg_tests {
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;

    fn ctx_for_channel<'a>(mode: IntakeRoutingMode, channel: &'a str) -> IntakeRouterContext<'a> {
        IntakeRouterContext {
            mode,
            leader_instance_id: "leader-1",
            channel_id: channel,
            user_msg_id: "9999",
            request_owner_id: "100",
            request_owner_name: Some("Tester"),
            user_text: "hello",
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: "foreground",
            merge_consecutive: false,
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
        }
    }

    async fn seed_agent_with_preference(
        pool: &PgPool,
        agent_id: &str,
        channel_id: &str,
        labels: serde_json::Value,
    ) {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, discord_channel_id,
             preferred_intake_node_labels)
             VALUES ($1, 'Test', 'claude', $2, $3)",
        )
        .bind(agent_id)
        .bind(channel_id)
        .bind(labels)
        .execute(pool)
        .await
        .expect("seed agent");
    }

    async fn seed_worker_node(
        pool: &PgPool,
        instance_id: &str,
        labels: serde_json::Value,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO worker_nodes (instance_id, status, role, effective_role,
             labels, last_heartbeat_at, started_at, updated_at)
             VALUES ($1, $2, 'worker', 'worker', $3, NOW(), NOW(), NOW())",
        )
        .bind(instance_id)
        .bind(status)
        .bind(labels)
        .execute(pool)
        .await
        .expect("seed worker_nodes");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_mode_with_no_preference_records_hook_disabled() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-disabled-noop",
            "ch-disabled",
            serde_json::json!([]),
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Disabled, "ch-disabled"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::HookDisabled
            }
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn disabled_mode_with_preference_set_records_disabled_but_preference_set() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-pref",
            "ch-pref",
            serde_json::json!(["unreal"]),
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Disabled, "ch-pref"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::DisabledButPreferenceSet
            }
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_mode_with_eligible_worker_returns_observed_without_inserting() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-observe",
            "ch-observe",
            serde_json::json!(["unreal"]),
        )
        .await;
        seed_worker_node(&pool, "worker-mac", serde_json::json!(["unreal"]), "online").await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Observe, "ch-observe"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::ObservedWouldForward {
                target_instance_id: "worker-mac".to_string()
            }
        );

        // Critical: observe mode must NEVER write to intake_outbox.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-observe")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "observe mode must not insert any rows");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enforce_mode_inserts_outbox_row_with_payload_fields() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-enforce",
            "ch-enforce",
            serde_json::json!(["unreal"]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-enforce",
            serde_json::json!(["unreal"]),
            "online",
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-enforce"),
        )
        .await;
        let outbox_id = match decision {
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                outbox_id,
            } => {
                assert_eq!(target_instance_id, "worker-enforce");
                outbox_id
            }
            other => panic!("expected Forwarded, got {other:?}"),
        };

        let row: (String, String, String, String, String, i32) = sqlx::query_as(
            "SELECT target_instance_id, channel_id, user_msg_id, agent_id,
             status, attempt_no FROM intake_outbox WHERE id = $1",
        )
        .bind(outbox_id)
        .fetch_one(&pool)
        .await
        .expect("read inserted row");
        assert_eq!(row.0, "worker-enforce");
        assert_eq!(row.1, "ch-enforce");
        assert_eq!(row.2, "9999");
        assert_eq!(row.3, "agent-enforce");
        assert_eq!(row.4, "pending");
        assert_eq!(row.5, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enforce_mode_with_open_route_conflict_falls_back_to_local() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-conflict",
            "ch-conflict",
            serde_json::json!(["unreal"]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-conflict",
            serde_json::json!(["unreal"]),
            "online",
        )
        .await;

        // Insert one OPEN row directly so the partial unique index
        // already holds for this channel.
        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, required_labels,
                channel_id, user_msg_id, request_owner_id, user_text,
                turn_kind, agent_id, status, attempt_no
             ) VALUES (
                'worker-conflict', 'leader-1', '[\"unreal\"]'::JSONB,
                'ch-conflict', 'msg-prior', '50', 'prior',
                'foreground', 'agent-conflict', 'pending', 1
             )",
        )
        .execute(&pool)
        .await
        .expect("seed prior open row");

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-conflict"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::OpenRouteAlreadyExists
            }
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enforce_mode_with_no_eligible_worker_falls_back_to_local() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-noworker",
            "ch-noworker",
            serde_json::json!(["unreal"]),
        )
        .await;
        // Worker exists but with WRONG labels.
        seed_worker_node(&pool, "worker-x", serde_json::json!(["api"]), "online").await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-noworker"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NoEligibleWorker
            }
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-noworker")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "no-eligible-worker must not insert");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enforce_mode_with_duplicate_message_returns_skipped_duplicate() {
        // Codex Phase 4 blocker #3: live ingress always inserts as
        // attempt_no=1. If Discord redelivers the same message, the
        // 3-tuple constraint fires and the hook MUST skip local
        // execution — running it would double-emit. Pin that the
        // attempt_no logic doesn't allocate `family_max + 1`.
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(&pool, "agent-dup", "ch-dup", serde_json::json!(["unreal"]))
            .await;
        seed_worker_node(&pool, "worker-dup", serde_json::json!(["unreal"]), "online").await;

        // First call should forward.
        let first = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-dup"),
        )
        .await;
        match first {
            IntakeRouterDecision::Forwarded { .. } => {}
            other => panic!("expected first Forwarded, got {other:?}"),
        }

        // Drive the existing row to terminal so the partial unique
        // index doesn't catch the second insert; only the 3-tuple
        // constraint should remain.
        sqlx::query(
            "UPDATE intake_outbox SET status='done', completed_at=NOW()
             WHERE channel_id='ch-dup' AND user_msg_id='9999' AND attempt_no=1",
        )
        .execute(&pool)
        .await
        .expect("terminate first");

        // Second call (same Discord message) — must report SkippedDuplicate.
        let second = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-dup"),
        )
        .await;
        assert_eq!(second, IntakeRouterDecision::SkippedDuplicate);

        // CRITICAL: the family did NOT grow — only one row exists.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = 'ch-dup'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(
            count, 1,
            "duplicate ingress must NOT allocate a fresh attempt_no"
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unmapped_channel_records_no_agent_for_channel() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-no-agent"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NoAgentForChannel
            }
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
