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
//! `cluster.intake_routing` is the primary authority for disabled /
//! observe / enforce mode. `ADK_INTAKE_ROUTING_MODE` remains as an
//! emergency override and is surfaced in health.

use crate::config::{ClusterIntakeRoutingConfig, ClusterIntakeRoutingMode};
use crate::db::intake_outbox::{
    InsertPendingPayload, IntakeInsertConflict, classify_insert_pending_error, insert_pending,
};
use crate::services::cluster::intake_routing::{
    IntakeRouteTarget, LocalRouteReason, candidates_from_worker_nodes_json, pick_intake_target,
};
use sqlx::PgPool;

/// How aggressively to apply the Phase-2 routing decision in front of
/// the existing leader intake path.
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
    fn from_config(mode: ClusterIntakeRoutingMode) -> Self {
        match mode {
            ClusterIntakeRoutingMode::Disabled => Self::Disabled,
            ClusterIntakeRoutingMode::Observe => Self::Observe,
            ClusterIntakeRoutingMode::Enforce => Self::Enforce,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntakeRoutingModeSource {
    ConfigYaml,
    EnvOverride,
}

impl IntakeRoutingModeSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ConfigYaml => "yaml",
            Self::EnvOverride => "env_override",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IntakeRoutingEnvOverride {
    Disabled,
    Observe,
    Enforce,
    Invalid,
}

impl IntakeRoutingEnvOverride {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Observe => "observe",
            Self::Enforce => "enforce",
            Self::Invalid => "invalid",
        }
    }

    fn mode(self) -> IntakeRoutingMode {
        match self {
            Self::Disabled | Self::Invalid => IntakeRoutingMode::Disabled,
            Self::Observe => IntakeRoutingMode::Observe,
            Self::Enforce => IntakeRoutingMode::Enforce,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveIntakeRoutingConfig {
    pub(crate) mode: IntakeRoutingMode,
    pub(crate) source: IntakeRoutingModeSource,
    pub(crate) yaml_enabled: bool,
    pub(crate) yaml_mode: ClusterIntakeRoutingMode,
    pub(crate) env_override: Option<&'static str>,
    pub(crate) warnings: Vec<&'static str>,
    pub(crate) forward_pre_claim_timeout_secs: u64,
    pub(crate) stale_claim_recovery_secs: u64,
}

impl EffectiveIntakeRoutingConfig {
    pub(crate) fn mode_is_enforce(&self) -> bool {
        matches!(self.mode, IntakeRoutingMode::Enforce)
    }

    pub(crate) fn worker_consumer_should_spawn(&self) -> bool {
        !matches!(self.mode, IntakeRoutingMode::Disabled)
    }

    pub(crate) fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode.as_str(),
            "source": self.source.as_str(),
            "yaml": {
                "enabled": self.yaml_enabled,
                "mode": self.yaml_mode.as_str(),
                "forward_pre_claim_timeout_secs": self.forward_pre_claim_timeout_secs,
                "stale_claim_recovery_secs": self.stale_claim_recovery_secs,
            },
            "env_override": self.env_override,
            "warning_count": self.warnings.len(),
            "configuration_warnings": self.warnings,
        })
    }
}

fn parse_intake_routing_env_override(value: &str) -> IntakeRoutingEnvOverride {
    match value.trim().to_ascii_lowercase().as_str() {
        "disabled" | "disable" | "off" | "false" | "0" => IntakeRoutingEnvOverride::Disabled,
        "observe" => IntakeRoutingEnvOverride::Observe,
        "enforce" => IntakeRoutingEnvOverride::Enforce,
        _ => IntakeRoutingEnvOverride::Invalid,
    }
}

fn effective_intake_routing_config_for(
    config: &ClusterIntakeRoutingConfig,
    env_override: Option<&str>,
) -> EffectiveIntakeRoutingConfig {
    let yaml_mode = if config.enabled {
        IntakeRoutingMode::from_config(config.mode)
    } else {
        IntakeRoutingMode::Disabled
    };
    let parsed_env = env_override.map(parse_intake_routing_env_override);
    let (mode, source) = match parsed_env {
        Some(value) => (value.mode(), IntakeRoutingModeSource::EnvOverride),
        None => (yaml_mode, IntakeRoutingModeSource::ConfigYaml),
    };
    let mut warnings = Vec::new();
    if parsed_env == Some(IntakeRoutingEnvOverride::Invalid) {
        warnings.push("invalid_ADK_INTAKE_ROUTING_MODE_fail_closed");
    }
    EffectiveIntakeRoutingConfig {
        mode,
        source,
        yaml_enabled: config.enabled,
        yaml_mode: config.mode,
        env_override: parsed_env.map(IntakeRoutingEnvOverride::as_str),
        warnings,
        forward_pre_claim_timeout_secs: config.forward_pre_claim_timeout_secs,
        stale_claim_recovery_secs: config.stale_claim_recovery_secs,
    }
}

pub(crate) fn effective_intake_routing_config() -> EffectiveIntakeRoutingConfig {
    let config = crate::config_live_reload::current()
        .map(|config| config.cluster.intake_routing.clone())
        .unwrap_or_else(|| crate::config::load_graceful().cluster.intake_routing);
    let env_override = std::env::var("ADK_INTAKE_ROUTING_MODE").ok();
    effective_intake_routing_config_for(&config, env_override.as_deref())
}

pub(crate) fn effective_intake_routing_mode() -> IntakeRoutingMode {
    effective_intake_routing_config().mode
}

pub(crate) fn intake_routing_status_json() -> serde_json::Value {
    effective_intake_routing_config().status_json()
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
    /// Channel has an explicit `/node` override to this leader, so the
    /// leader should keep the turn local.
    NodeOverrideIsLeader,
    /// Channel has an explicit `/node` override, but that worker is not
    /// currently online or does not advertise a matching intake consumer.
    NodeOverrideUnavailable,
    /// Channel has an explicit `/node` override, but the intake worker is only
    /// spawned in `Enforce` mode. Running locally avoids pending-row loss.
    NodeOverrideRoutingDisabled,
}

/// Inputs to the hook. Bundled into a struct so the intake gate can
/// thread per-channel context cleanly without a 6-argument fn call.
#[derive(Clone, Debug)]
pub(crate) struct IntakeRouterContext<'a> {
    pub mode: IntakeRoutingMode,
    pub leader_instance_id: &'a str,
    /// Provider of the bot handling this intake (#4349). Worker claim is
    /// scoped on this, so it must be the forwarding bot's provider — never
    /// `agents.provider`, which is a single column shared by an agent's
    /// cc and cdx channels.
    pub provider: &'a str,
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
    pub node_override_instance_id: Option<&'a str>,
}

fn worker_heartbeat_lease_secs() -> u64 {
    crate::config::load_graceful().cluster.lease_ttl_secs.max(1)
}

/// Run the hook. Never fails — every error path turns into
/// `RanLocal { reason: DbErrorFellBackToLocal }` because losing an
/// intake message is a strictly worse failure mode than executing
/// it on the leader.
pub(crate) async fn try_route_intake(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
) -> IntakeRouterDecision {
    if let Some(target) = ctx
        .node_override_instance_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !matches!(ctx.mode, IntakeRoutingMode::Enforce) {
            tracing::info!(
                target_instance_id = %target,
                channel_id = ctx.channel_id,
                user_msg_id = ctx.user_msg_id,
                mode = ?ctx.mode,
                "[intake_router] /node override ignored because intake routing is not enforce"
            );
            return IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideRoutingDisabled,
            };
        }
        return try_route_node_override(pool, ctx, target).await;
    }

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
    //
    // #4349: the agent's own `provider` column is deliberately ignored for
    // routing. It is a single value shared by the agent's cc and cdx
    // channels, so on a paired agent it disagrees with the bot that is
    // actually handling this message. `ctx.provider` is that bot.
    let (agent_id, _agent_provider, preferred_labels) =
        match agent_id_and_preferred_labels(pool, ctx.channel_id).await {
            Ok(Some((agent_id, provider, labels))) => (agent_id, provider, labels),
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

    let candidates = match crate::services::cluster::node_registry::list_worker_nodes(
        pool,
        worker_heartbeat_lease_secs(),
    )
    .await
    {
        Ok(nodes) => {
            let eligible_nodes: Vec<_> = nodes
                .into_iter()
                .filter(|node| {
                    crate::services::cluster::node_registry::node_supports_intake_provider(
                        node,
                        ctx.provider,
                    )
                })
                .collect();
            candidates_from_worker_nodes_json(&eligible_nodes)
        }
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
                    LocalRouteReason::NoEligibleWorker => RanLocalReason::NoEligibleWorker,
                    LocalRouteReason::LeaderIsOnlyEligible => RanLocalReason::LeaderIsOnlyEligible,
                    LocalRouteReason::NoPreference => unreachable!(
                        "pick_intake_target cannot return no-preference after non-empty preference gate"
                    ),
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

async fn try_route_node_override(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
    target: &str,
) -> IntakeRouterDecision {
    // #4349: `agents.provider` is ignored here for the same reason as in
    // `try_route_intake` — the handling bot is `ctx.provider`.
    let (agent_id, _agent_provider, _) =
        match agent_id_and_preferred_labels(pool, ctx.channel_id).await {
            Ok(Some((agent_id, provider, labels))) => (agent_id, provider, labels),
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

    if target == ctx.leader_instance_id {
        return IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::NodeOverrideIsLeader,
        };
    }

    let nodes = match crate::services::cluster::node_registry::list_worker_nodes(
        pool,
        worker_heartbeat_lease_secs(),
    )
    .await
    {
        Ok(nodes) => nodes,
        Err(error) => {
            return IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::DbErrorFellBackToLocal {
                    detail: format!("list worker_nodes: {error}"),
                },
            };
        }
    };
    let target_online = nodes.iter().any(|node| {
        node.get("instance_id").and_then(|value| value.as_str()) == Some(target)
            && node
                .get("status")
                .and_then(|value| value.as_str())
                .is_some_and(|status| status.eq_ignore_ascii_case("online"))
            && crate::services::cluster::node_registry::node_supports_intake_provider(
                node,
                ctx.provider,
            )
    });
    if !target_online {
        return IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::NodeOverrideUnavailable,
        };
    }

    let required_labels: Vec<String> = Vec::new();
    let payload = build_payload_for_insert(ctx, target, &required_labels, &agent_id);
    match insert_pending(pool, &payload, 1, None).await {
        Ok(outbox_id) => IntakeRouterDecision::Forwarded {
            target_instance_id: target.to_string(),
            outbox_id,
        },
        Err(error) => match classify_insert_pending_error(&error) {
            Some(IntakeInsertConflict::OpenRoutePerChannel) => IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::OpenRouteAlreadyExists,
            },
            Some(IntakeInsertConflict::DuplicateMessageAttempt) => {
                tracing::info!(
                    channel_id = ctx.channel_id,
                    user_msg_id = ctx.user_msg_id,
                    "[intake_router] duplicate Discord message (node override) — existing row already covers it; skipping local execution"
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
        provider: ctx.provider.to_string(),
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

/// Look up the agent_id + provider + `preferred_intake_node_labels` for a channel.
/// Returns `Ok(None)` when the channel isn't mapped to any agent.
async fn agent_id_and_preferred_labels(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<(String, String, Vec<String>)>, sqlx::Error> {
    let row: Option<(String, String, serde_json::Value)> = sqlx::query_as(
        "SELECT id, provider, preferred_intake_node_labels FROM agents
         WHERE discord_channel_id = $1
            OR discord_channel_alt = $1
            OR discord_channel_cc = $1
            OR discord_channel_cdx = $1
         LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;

    let Some((agent_id, provider, labels_value)) = row else {
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

    Ok(Some((agent_id, provider, labels)))
}

/// Variant used by the `Disabled` branch — only the labels matter.
async fn agent_preferred_labels_for_channel(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<Vec<String>>, sqlx::Error> {
    Ok(agent_id_and_preferred_labels(pool, channel_id)
        .await?
        .map(|(_, _, labels)| labels))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn effective_intake_routing_config_resolves_yaml_and_env_override() {
        let mut yaml = ClusterIntakeRoutingConfig::default();
        assert_eq!(
            effective_intake_routing_config_for(&yaml, None).mode,
            IntakeRoutingMode::Disabled
        );

        yaml.enabled = true;
        assert_eq!(
            effective_intake_routing_config_for(&yaml, None).mode,
            IntakeRoutingMode::Observe
        );

        yaml.mode = ClusterIntakeRoutingMode::Enforce;
        let from_yaml = effective_intake_routing_config_for(&yaml, None);
        assert_eq!(from_yaml.mode, IntakeRoutingMode::Enforce);
        assert_eq!(from_yaml.source, IntakeRoutingModeSource::ConfigYaml);

        let env_observe = effective_intake_routing_config_for(&yaml, Some("observe"));
        assert_eq!(env_observe.mode, IntakeRoutingMode::Observe);
        assert_eq!(env_observe.source, IntakeRoutingModeSource::EnvOverride);
        assert_eq!(env_observe.env_override, Some("observe"));

        let env_disabled = effective_intake_routing_config_for(&yaml, Some("OFF"));
        assert_eq!(env_disabled.mode, IntakeRoutingMode::Disabled);
        assert_eq!(env_disabled.env_override, Some("disabled"));

        let invalid = effective_intake_routing_config_for(&yaml, Some("garbage"));
        assert_eq!(invalid.mode, IntakeRoutingMode::Disabled);
        assert_eq!(invalid.source, IntakeRoutingModeSource::EnvOverride);
        assert_eq!(invalid.env_override, Some("invalid"));
        assert_eq!(
            invalid.warnings,
            vec!["invalid_ADK_INTAKE_ROUTING_MODE_fail_closed"]
        );
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
            provider: "claude",
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
            node_override_instance_id: None,
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
        seed_worker_node_with_capabilities(
            pool,
            instance_id,
            labels,
            status,
            serde_json::json!({
                "intake_worker": {
                    "enabled": true,
                    "providers": ["claude"],
                },
            }),
        )
        .await;
    }

    async fn seed_worker_node_with_capabilities(
        pool: &PgPool,
        instance_id: &str,
        labels: serde_json::Value,
        status: &str,
        capabilities: serde_json::Value,
    ) {
        sqlx::query(
            "INSERT INTO worker_nodes (instance_id, status, role, effective_role,
             labels, capabilities, last_heartbeat_at, started_at, updated_at)
             VALUES ($1, $2, 'worker', 'worker', $3, $4, NOW(), NOW(), NOW())",
        )
        .bind(instance_id)
        .bind(status)
        .bind(labels)
        .bind(capabilities)
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
    async fn node_override_disabled_mode_runs_local_without_inserting() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-node-override",
            "ch-node-override",
            serde_json::json!([]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-selected",
            serde_json::json!(["mac-mini"]),
            "online",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Disabled, "ch-node-override");
        ctx.node_override_instance_id = Some("worker-selected");
        let decision = try_route_intake(&pool, &ctx).await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideRoutingDisabled
            }
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-node-override")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "disabled /node override must not insert");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_override_forwards_in_enforce_mode() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-node-override",
            "ch-node-override",
            serde_json::json!([]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-selected",
            serde_json::json!(["mac-mini"]),
            "online",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-node-override");
        ctx.node_override_instance_id = Some("worker-selected");
        let decision = try_route_intake(&pool, &ctx).await;
        let outbox_id = match decision {
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                outbox_id,
            } => {
                assert_eq!(target_instance_id, "worker-selected");
                outbox_id
            }
            other => panic!("expected explicit node override to forward, got {other:?}"),
        };

        let row: (String, serde_json::Value, String) = sqlx::query_as(
            "SELECT target_instance_id, required_labels, agent_id
               FROM intake_outbox WHERE id = $1",
        )
        .bind(outbox_id)
        .fetch_one(&pool)
        .await
        .expect("read node override row");
        assert_eq!(row.0, "worker-selected");
        assert_eq!(row.1, serde_json::json!([]));
        assert_eq!(row.2, "agent-node-override");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_override_with_duplicate_message_returns_skipped_duplicate() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-node-override-dup",
            "ch-node-override-dup",
            serde_json::json!([]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-selected-dup",
            serde_json::json!(["mac-mini"]),
            "online",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-node-override-dup");
        ctx.node_override_instance_id = Some("worker-selected-dup");

        // First call should forward.
        let first = try_route_intake(&pool, &ctx).await;
        match first {
            IntakeRouterDecision::Forwarded { .. } => {}
            other => panic!("expected first Forwarded, got {other:?}"),
        }

        // Drive the existing row to terminal so the partial unique
        // index doesn't catch the second insert; only the 3-tuple
        // constraint should remain.
        sqlx::query(
            "UPDATE intake_outbox SET status='done', completed_at=NOW()
             WHERE channel_id='ch-node-override-dup' AND user_msg_id='9999' AND attempt_no=1",
        )
        .execute(&pool)
        .await
        .expect("terminate first");

        // Second call (same Discord message) — must report SkippedDuplicate.
        let second = try_route_intake(&pool, &ctx).await;
        assert_eq!(second, IntakeRouterDecision::SkippedDuplicate);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_override_unavailable_runs_local_without_inserting() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-node-offline",
            "ch-node-offline",
            serde_json::json!([]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-offline",
            serde_json::json!(["mac-mini"]),
            "offline",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-node-offline");
        ctx.node_override_instance_id = Some("worker-offline");
        let decision = try_route_intake(&pool, &ctx).await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideUnavailable
            }
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-node-offline")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "offline /node override must not insert");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn node_override_without_intake_capability_runs_local_without_inserting() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-node-no-consumer",
            "ch-node-no-consumer",
            serde_json::json!([]),
        )
        .await;
        seed_worker_node_with_capabilities(
            &pool,
            "worker-online-no-consumer",
            serde_json::json!(["mac-mini"]),
            "online",
            serde_json::json!({}),
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-node-no-consumer");
        ctx.node_override_instance_id = Some("worker-online-no-consumer");
        let decision = try_route_intake(&pool, &ctx).await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideUnavailable
            }
        );

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-node-no-consumer")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "non-consuming /node target must not insert");

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
