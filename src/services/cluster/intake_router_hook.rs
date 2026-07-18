//! Leader-side intake-routing hook. Phase 4 of intake-node-routing
//! (docs/design/intake-node-routing.md).
//!
//! Sits in the leader's Discord intake gate immediately before
//! `handle_text_message`. For each incoming message, decides:
//!
//! In `Enforce`, durable live session ownership wins over `/node` and
//! preferred labels. Unknown, stale, or conflicting ownership fails safe:
//! the gateway must not start a same-named tmux on another host. A distinct
//! already-open route is deferred instead of executed locally.
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

mod owner_record;
mod session_owner;

use session_owner::SessionOwnerResolution;

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
    /// No forwarding happened (Disabled mode, a confirmed ownerless channel
    /// with no usable preference, or an availability fallback after that
    /// ownerless state was proven). The caller MUST run the turn locally.
    RanLocal { reason: RanLocalReason },
    /// Observe mode evaluated the same owner-aware placement path as Enforce,
    /// but did not mutate the outbox. The caller MUST still run locally.
    Observed { outcome: ObservedIntakeOutcome },
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
    /// A different message already owns the channel's single open outbox
    /// route. The producer must preserve/retry queued work and MUST NOT run it
    /// locally while the predecessor is open.
    DeferredOpenRoute { target_instance_id: String },
    /// Ownership or placement could not be proven safe. Caller MUST NOT run
    /// the local execution body.
    Blocked { reason: IntakeBlockedReason },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ObservedIntakeOutcome {
    WouldKeepLocalExistingOwner,
    WouldForwardLiveForeignOwner { target_instance_id: String },
    WouldAssignNoOwnerToTarget { target_instance_id: String },
    WouldKeepNoOwnerLocal { reason: RanLocalReason },
    WouldSkipDuplicate,
    WouldDeferOpenRoute { target_instance_id: String },
    WouldBlock { reason: IntakeBlockedReason },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IntakeBlockedReason {
    OwnerLookupFailed { detail: String },
    StaleSessionOwners { instance_ids: Vec<String> },
    ConflictingLiveSessionOwners { instance_ids: Vec<String> },
    OwnerProtocolIncompatible { instance_id: String },
    OverrideUnavailable { target_instance_id: String },
    NonPortableAttachment { owner_instance_id: String },
    RoutingDependencyFailed { detail: String },
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
    /// Channel has an explicit `/node` override, but the intake worker is only
    /// spawned in `Enforce` mode. Running locally avoids pending-row loss.
    NodeOverrideRoutingDisabled,
    /// This instance is the durable live owner for the session.
    LiveSessionOwnerIsLocal,
    /// A channel without an existing owner establishes its first placement
    /// locally when the payload contains node-local upload paths.
    NoOwnerWithNonPortableAttachment,
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
    pub preserve_on_cancel: bool,
    pub node_override_instance_id: Option<&'a str>,
    pub has_nonportable_uploads: bool,
}

fn worker_heartbeat_lease_secs() -> u64 {
    crate::config::load_graceful().cluster.lease_ttl_secs.max(1)
}

/// Run the leader-side placement hook. Enforce mode fails safe whenever a
/// local execution could split an existing session or duplicate an open route.
pub(crate) async fn try_route_intake(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
) -> IntakeRouterDecision {
    let node_override = ctx
        .node_override_instance_id
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if matches!(ctx.mode, IntakeRoutingMode::Disabled) {
        if node_override.is_some() {
            return IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideRoutingDisabled,
            };
        }
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

    // Observe and Enforce share the owner/open-route/attachment/placement path.
    // Only the final outbox mutation is mode-dependent.

    // Enforce precedence: live session owner -> explicit /node -> preferred
    // labels. The owner lookup must complete before any placement fallback.
    let owner = match session_owner::resolve_session_owner(
        pool,
        ctx.provider,
        ctx.channel_id,
        ctx.leader_instance_id,
        worker_heartbeat_lease_secs(),
        ctx.preserve_on_cancel,
    )
    .await
    {
        Ok(owner) => owner,
        Err(detail) => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::OwnerLookupFailed { detail },
                },
            );
        }
    };

    // Owner-safety failures outrank the open-route ordering fence. In
    // particular, a live attachment ingress must receive the explicit blocked
    // outcome instead of being silently deferred into a queue whose foreign
    // owner can never consume this gateway-local path.
    match &owner {
        SessionOwnerResolution::StaleOwners { instance_ids } => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::StaleSessionOwners {
                        instance_ids: instance_ids.clone(),
                    },
                },
            );
        }
        SessionOwnerResolution::ConflictingLiveOwners { instance_ids } => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::ConflictingLiveSessionOwners {
                        instance_ids: instance_ids.clone(),
                    },
                },
            );
        }
        SessionOwnerResolution::LiveForeignIncompatible { instance_id, .. } => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::OwnerProtocolIncompatible {
                        instance_id: instance_id.clone(),
                    },
                },
            );
        }
        SessionOwnerResolution::LiveForeign { instance_id, .. } if ctx.has_nonportable_uploads => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::NonPortableAttachment {
                        owner_instance_id: instance_id.clone(),
                    },
                },
            );
        }
        SessionOwnerResolution::NoOwner
        | SessionOwnerResolution::LiveLocal { .. }
        | SessionOwnerResolution::LiveForeign { .. } => {}
        SessionOwnerResolution::LiveForeignIncompatible { .. } => {
            unreachable!("incompatible live owner returns before the open-route fence")
        }
    }

    // The durable single-open-route fence surrounds every placement branch,
    // including a local live owner and attachment-first placement.
    match existing_open_route(pool, ctx.channel_id).await {
        Ok(Some((_, existing_user_msg_id))) if existing_user_msg_id == ctx.user_msg_id => {
            return apply_observe_mode(ctx.mode, IntakeRouterDecision::SkippedDuplicate);
        }
        Ok(Some((target_instance_id, _))) => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::DeferredOpenRoute { target_instance_id },
            );
        }
        Ok(None) => {}
        Err(error) => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::RoutingDependencyFailed {
                        detail: format!("open route lookup: {error}"),
                    },
                },
            );
        }
    }

    match owner {
        SessionOwnerResolution::LiveLocal {
            instance_id,
            stale_instance_ids,
        } => {
            log_shadowed_owner_state(ctx, &instance_id, node_override, &stale_instance_ids);
            apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::RanLocal {
                    reason: RanLocalReason::LiveSessionOwnerIsLocal,
                },
            )
        }
        SessionOwnerResolution::LiveForeign {
            instance_id,
            stale_instance_ids,
        } => {
            log_shadowed_owner_state(ctx, &instance_id, node_override, &stale_instance_ids);
            let agent_id = match agent_id_and_preferred_labels(pool, ctx.channel_id).await {
                Ok(Some((agent_id, _, _))) => agent_id,
                Ok(None) => String::new(),
                Err(error) => {
                    return apply_observe_mode(
                        ctx.mode,
                        IntakeRouterDecision::Blocked {
                            reason: IntakeBlockedReason::RoutingDependencyFailed {
                                detail: format!("agent lookup for live owner: {error}"),
                            },
                        },
                    );
                }
            };
            route_to_instance(
                pool,
                ctx,
                &instance_id,
                &[],
                &agent_id,
                ObserveTargetKind::LiveForeignOwner,
            )
            .await
        }
        SessionOwnerResolution::StaleOwners { .. }
        | SessionOwnerResolution::ConflictingLiveOwners { .. }
        | SessionOwnerResolution::LiveForeignIncompatible { .. } => {
            unreachable!("owner fail-safe outcomes return before the open-route fence")
        }
        SessionOwnerResolution::NoOwner => {
            if ctx.has_nonportable_uploads {
                return apply_observe_mode(
                    ctx.mode,
                    IntakeRouterDecision::RanLocal {
                        reason: RanLocalReason::NoOwnerWithNonPortableAttachment,
                    },
                );
            }
            if let Some(target) = node_override {
                route_node_override_without_owner(pool, ctx, target).await
            } else {
                route_by_preferred_labels(pool, ctx).await
            }
        }
    }
}

fn log_shadowed_owner_state(
    ctx: &IntakeRouterContext<'_>,
    owner_instance_id: &str,
    node_override: Option<&str>,
    stale_instance_ids: &[String],
) {
    if !stale_instance_ids.is_empty() {
        tracing::warn!(
            channel_id = ctx.channel_id,
            provider = ctx.provider,
            owner_instance_id,
            ?stale_instance_ids,
            "[intake_router] live session owner selected over stale duplicate owners"
        );
    }
    if node_override.is_some_and(|target| target != owner_instance_id) {
        tracing::info!(
            channel_id = ctx.channel_id,
            provider = ctx.provider,
            owner_instance_id,
            node_override_instance_id = node_override,
            "[intake_router] override_shadowed_by_live_owner"
        );
    }
}

fn preferred_label_dependency_fallback(detail: String) -> IntakeRouterDecision {
    IntakeRouterDecision::RanLocal {
        reason: RanLocalReason::DbErrorFellBackToLocal { detail },
    }
}

async fn route_by_preferred_labels(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
) -> IntakeRouterDecision {
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
                return apply_observe_mode(
                    ctx.mode,
                    IntakeRouterDecision::RanLocal {
                        reason: RanLocalReason::NoAgentForChannel,
                    },
                );
            }
            Err(error) => {
                return apply_observe_mode(
                    ctx.mode,
                    preferred_label_dependency_fallback(format!("agent lookup: {error}")),
                );
            }
        };

    if preferred_labels.is_empty() {
        return apply_observe_mode(
            ctx.mode,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::AgentHasNoPreference,
            },
        );
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
                    crate::services::cluster::node_registry::node_supports_intake_request(
                        node,
                        ctx.provider,
                        ctx.preserve_on_cancel,
                    )
                })
                .collect();
            candidates_from_worker_nodes_json(&eligible_nodes)
        }
        Err(error) => {
            return apply_observe_mode(
                ctx.mode,
                preferred_label_dependency_fallback(format!("list worker_nodes: {error}")),
            );
        }
    };

    let target = match pick_intake_target(&candidates, &preferred_labels, ctx.leader_instance_id) {
        IntakeRouteTarget::Worker { instance_id } => instance_id,
        IntakeRouteTarget::Local { reason } => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::RanLocal {
                    reason: match reason {
                        LocalRouteReason::NoEligibleWorker => RanLocalReason::NoEligibleWorker,
                        LocalRouteReason::LeaderIsOnlyEligible => {
                            RanLocalReason::LeaderIsOnlyEligible
                        }
                        LocalRouteReason::NoPreference => unreachable!(
                            "pick_intake_target cannot return no-preference after non-empty preference gate"
                        ),
                    },
                },
            );
        }
    };

    route_to_instance(
        pool,
        ctx,
        &target,
        &preferred_labels,
        &agent_id,
        ObserveTargetKind::NoOwnerPlacement,
    )
    .await
}

async fn route_node_override_without_owner(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
    target: &str,
) -> IntakeRouterDecision {
    // #4349: `agents.provider` is ignored here for the same reason as in
    // `try_route_intake` — the handling bot is `ctx.provider`.
    let (agent_id, _agent_provider, _) =
        match agent_id_and_preferred_labels(pool, ctx.channel_id).await {
            Ok(Some((agent_id, provider, labels))) => (agent_id, provider, labels),
            Ok(None) => (String::new(), String::new(), Vec::new()),
            Err(error) => {
                return apply_observe_mode(
                    ctx.mode,
                    IntakeRouterDecision::Blocked {
                        reason: IntakeBlockedReason::RoutingDependencyFailed {
                            detail: format!("agent lookup for node override: {error}"),
                        },
                    },
                );
            }
        };

    if target == ctx.leader_instance_id {
        return apply_observe_mode(
            ctx.mode,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideIsLeader,
            },
        );
    }

    let nodes = match crate::services::cluster::node_registry::list_worker_nodes(
        pool,
        worker_heartbeat_lease_secs(),
    )
    .await
    {
        Ok(nodes) => nodes,
        Err(_) => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::OverrideUnavailable {
                        target_instance_id: target.to_string(),
                    },
                },
            );
        }
    };
    let target_online = nodes.iter().any(|node| {
        node.get("instance_id").and_then(|value| value.as_str()) == Some(target)
            && node
                .get("status")
                .and_then(|value| value.as_str())
                .is_some_and(|status| status.eq_ignore_ascii_case("online"))
            && crate::services::cluster::node_registry::node_supports_intake_request(
                node,
                ctx.provider,
                ctx.preserve_on_cancel,
            )
    });
    if !target_online {
        return apply_observe_mode(
            ctx.mode,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::OverrideUnavailable {
                    target_instance_id: target.to_string(),
                },
            },
        );
    }

    let required_labels: Vec<String> = Vec::new();
    route_to_instance(
        pool,
        ctx,
        target,
        &required_labels,
        &agent_id,
        ObserveTargetKind::NoOwnerPlacement,
    )
    .await
}

#[derive(Clone, Copy)]
enum ObserveTargetKind {
    LiveForeignOwner,
    NoOwnerPlacement,
}

fn apply_observe_mode(
    mode: IntakeRoutingMode,
    decision: IntakeRouterDecision,
) -> IntakeRouterDecision {
    if !matches!(mode, IntakeRoutingMode::Observe) {
        return decision;
    }

    let outcome = match decision {
        IntakeRouterDecision::RanLocal {
            reason: RanLocalReason::LiveSessionOwnerIsLocal,
        } => ObservedIntakeOutcome::WouldKeepLocalExistingOwner,
        IntakeRouterDecision::RanLocal { reason } => {
            ObservedIntakeOutcome::WouldKeepNoOwnerLocal { reason }
        }
        IntakeRouterDecision::SkippedDuplicate => ObservedIntakeOutcome::WouldSkipDuplicate,
        IntakeRouterDecision::DeferredOpenRoute { target_instance_id } => {
            ObservedIntakeOutcome::WouldDeferOpenRoute { target_instance_id }
        }
        IntakeRouterDecision::Blocked { reason } => ObservedIntakeOutcome::WouldBlock { reason },
        IntakeRouterDecision::Observed { .. } | IntakeRouterDecision::Forwarded { .. } => {
            unreachable!("observe conversion accepts only a mutation-free routing decision")
        }
    };
    IntakeRouterDecision::Observed { outcome }
}

async fn existing_open_route(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT target_instance_id, user_msg_id
           FROM intake_outbox
          WHERE channel_id = $1
            AND status IN ('pending', 'claimed', 'accepted', 'spawned')
          ORDER BY id
          LIMIT 1",
    )
    .bind(channel_id)
    .fetch_optional(pool)
    .await
}

async fn route_to_instance(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
    target: &str,
    required_labels: &[String],
    agent_id: &str,
    observe_target_kind: ObserveTargetKind,
) -> IntakeRouterDecision {
    match existing_open_route(pool, ctx.channel_id).await {
        Ok(Some((_, existing_user_msg_id))) if existing_user_msg_id == ctx.user_msg_id => {
            return apply_observe_mode(ctx.mode, IntakeRouterDecision::SkippedDuplicate);
        }
        Ok(Some((existing_target, _))) => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::DeferredOpenRoute {
                    target_instance_id: existing_target,
                },
            );
        }
        Ok(None) => {}
        Err(error) => {
            return apply_observe_mode(
                ctx.mode,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::RoutingDependencyFailed {
                        detail: format!("open route lookup: {error}"),
                    },
                },
            );
        }
    }

    if matches!(ctx.mode, IntakeRoutingMode::Observe) {
        let outcome = match observe_target_kind {
            ObserveTargetKind::LiveForeignOwner => {
                ObservedIntakeOutcome::WouldForwardLiveForeignOwner {
                    target_instance_id: target.to_string(),
                }
            }
            ObserveTargetKind::NoOwnerPlacement => {
                ObservedIntakeOutcome::WouldAssignNoOwnerToTarget {
                    target_instance_id: target.to_string(),
                }
            }
        };
        tracing::info!(
            ?outcome,
            channel_id = ctx.channel_id,
            user_msg_id = ctx.user_msg_id,
            agent_id,
            "[intake_router] owner-aware observe decision"
        );
        return IntakeRouterDecision::Observed { outcome };
    }

    // Live ingress is always attempt 1. Retry-family allocation belongs only
    // to the failed-pre-accept worker recovery path.
    let payload = build_payload_for_insert(ctx, target, required_labels, agent_id);
    match insert_pending(pool, &payload, 1, None).await {
        Ok(outbox_id) => IntakeRouterDecision::Forwarded {
            target_instance_id: target.to_string(),
            outbox_id,
        },
        Err(error) => match classify_insert_pending_error(&error) {
            Some(IntakeInsertConflict::OpenRoutePerChannel) => {
                match existing_open_route(pool, ctx.channel_id).await {
                    Ok(Some((_, existing_user_msg_id)))
                        if existing_user_msg_id == ctx.user_msg_id =>
                    {
                        IntakeRouterDecision::SkippedDuplicate
                    }
                    Ok(Some((existing_target, _))) => IntakeRouterDecision::DeferredOpenRoute {
                        target_instance_id: existing_target,
                    },
                    Ok(None) | Err(_) => IntakeRouterDecision::DeferredOpenRoute {
                        target_instance_id: target.to_string(),
                    },
                }
            }
            Some(IntakeInsertConflict::DuplicateMessageAttempt) => {
                tracing::info!(
                    channel_id = ctx.channel_id,
                    user_msg_id = ctx.user_msg_id,
                    "[intake_router] duplicate Discord message (node override) — existing row already covers it; skipping local execution"
                );
                IntakeRouterDecision::SkippedDuplicate
            }
            None => IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::RoutingDependencyFailed {
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
        preserve_on_cancel: ctx.preserve_on_cancel,
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
            preserve_on_cancel: false,
            node_override_instance_id: None,
            has_nonportable_uploads: false,
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
                    "features": ["preserve_on_cancel_v1"],
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

    async fn seed_session_owner(
        pool: &PgPool,
        session_key: &str,
        provider: &str,
        channel_id: &str,
        instance_id: &str,
        status: &str,
    ) {
        sqlx::query(
            "INSERT INTO sessions (
                session_key, provider, channel_id, instance_id, status, last_heartbeat
             ) VALUES ($1, $2, $3, $4, $5, NOW())",
        )
        .bind(session_key)
        .bind(provider)
        .bind(channel_id)
        .bind(instance_id)
        .bind(status)
        .execute(pool)
        .await
        .expect("seed session owner");
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
            IntakeRouterDecision::Observed {
                outcome: ObservedIntakeOutcome::WouldAssignNoOwnerToTarget {
                    target_instance_id: "worker-mac".to_string()
                }
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
    async fn observe_mode_uses_live_local_owner_without_mutation() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_session_owner(
            &pool,
            "claude:leader-1:ch-observe-local",
            "claude",
            "ch-observe-local",
            "leader-1",
            "turn_active",
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Observe, "ch-observe-local"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::Observed {
                outcome: ObservedIntakeOutcome::WouldKeepLocalExistingOwner
            }
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-observe-local")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_mode_uses_live_foreign_owner_without_mutation() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_worker_node(&pool, "worker-owner", serde_json::json!([]), "online").await;
        seed_session_owner(
            &pool,
            "claude:worker-owner:ch-observe-foreign",
            "claude",
            "ch-observe-foreign",
            "worker-owner",
            "turn_active",
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Observe, "ch-observe-foreign"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::Observed {
                outcome: ObservedIntakeOutcome::WouldForwardLiveForeignOwner {
                    target_instance_id: "worker-owner".to_string()
                }
            }
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-observe-foreign")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_mode_reports_stale_owner_block_without_mutation() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_session_owner(
            &pool,
            "claude:stale-owner:ch-observe-stale",
            "claude",
            "ch-observe-stale",
            "stale-owner",
            "turn_active",
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Observe, "ch-observe-stale"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::Observed {
                outcome: ObservedIntakeOutcome::WouldBlock {
                    reason: IntakeBlockedReason::StaleSessionOwners {
                        instance_ids: vec!["stale-owner".to_string()]
                    }
                }
            }
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-observe-stale")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observe_mode_reports_foreign_attachment_block_without_mutation() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_worker_node(&pool, "worker-owner", serde_json::json!([]), "online").await;
        seed_session_owner(
            &pool,
            "claude:worker-owner:ch-observe-upload",
            "claude",
            "ch-observe-upload",
            "worker-owner",
            "turn_active",
        )
        .await;
        let mut ctx = ctx_for_channel(IntakeRoutingMode::Observe, "ch-observe-upload");
        ctx.has_nonportable_uploads = true;

        let decision = try_route_intake(&pool, &ctx).await;
        assert_eq!(
            decision,
            IntakeRouterDecision::Observed {
                outcome: ObservedIntakeOutcome::WouldBlock {
                    reason: IntakeBlockedReason::NonPortableAttachment {
                        owner_instance_id: "worker-owner".to_string()
                    }
                }
            }
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-observe-upload")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0);

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

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-enforce");
        ctx.preserve_on_cancel = true;
        let decision = try_route_intake(&pool, &ctx).await;
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

        let row: (String, String, String, String, String, i32, Option<bool>) = sqlx::query_as(
            "SELECT target_instance_id, channel_id, user_msg_id, agent_id,
                        status, attempt_no, preserve_on_cancel
                 FROM intake_outbox WHERE id = $1",
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
        assert_eq!(row.6, Some(true));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preserving_request_skips_legacy_preferred_worker_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent_with_preference(
            &pool,
            "agent-mixed-preferred",
            "ch-mixed-preferred",
            serde_json::json!(["preferred"]),
        )
        .await;
        seed_worker_node_with_capabilities(
            &pool,
            "worker-legacy",
            serde_json::json!(["preferred"]),
            "online",
            serde_json::json!({
                "intake_worker": { "enabled": true, "providers": ["claude"] }
            }),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-capable",
            serde_json::json!(["preferred"]),
            "online",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-mixed-preferred");
        ctx.preserve_on_cancel = true;
        let decision = try_route_intake(&pool, &ctx).await;
        assert!(matches!(
            decision,
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                ..
            } if target_instance_id == "worker-capable"
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_preserving_request_allows_legacy_preferred_worker_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent_with_preference(
            &pool,
            "agent-legacy-preferred",
            "ch-legacy-preferred",
            serde_json::json!(["legacy"]),
        )
        .await;
        seed_worker_node_with_capabilities(
            &pool,
            "worker-legacy",
            serde_json::json!(["legacy"]),
            "online",
            serde_json::json!({
                "intake_worker": { "enabled": true, "providers": ["claude"] }
            }),
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-legacy-preferred"),
        )
        .await;
        assert!(matches!(
            decision,
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                ..
            } if target_instance_id == "worker-legacy"
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn owner_beats_override_and_labels_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-owner-first",
            "ch-owner-first",
            serde_json::json!(["preferred"]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-override",
            serde_json::json!(["preferred"]),
            "online",
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-owner",
            serde_json::json!(["owner"]),
            "online",
        )
        .await;
        seed_session_owner(
            &pool,
            "claude:owner-first",
            "claude",
            "ch-owner-first",
            "worker-owner",
            "turn_active",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-owner-first");
        ctx.node_override_instance_id = Some("worker-override");
        let decision = try_route_intake(&pool, &ctx).await;
        let outbox_id = match decision {
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                outbox_id,
            } => {
                assert_eq!(target_instance_id, "worker-owner");
                outbox_id
            }
            other => panic!("live owner must beat /node and labels, got {other:?}"),
        };
        let required_labels: serde_json::Value =
            sqlx::query_scalar("SELECT required_labels FROM intake_outbox WHERE id = $1")
                .bind(outbox_id)
                .fetch_one(&pool)
                .await
                .expect("owner row");
        assert_eq!(required_labels, serde_json::json!([]));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gateway_moves_but_tmux_owner_stays_a_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_agent_with_preference(
            &pool,
            "agent-gateway-move",
            "ch-gateway-move",
            serde_json::json!(["gateway-b"]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-owner-a",
            serde_json::json!(["owner-a"]),
            "online",
        )
        .await;
        seed_worker_node(
            &pool,
            "gateway-b",
            serde_json::json!(["gateway-b"]),
            "online",
        )
        .await;
        seed_session_owner(
            &pool,
            "claude:gateway-move",
            "claude",
            "ch-gateway-move",
            "worker-owner-a",
            "idle",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-gateway-move");
        ctx.leader_instance_id = "gateway-b";
        let decision = try_route_intake(&pool, &ctx).await;
        assert!(matches!(
            decision,
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                ..
            } if target_instance_id == "worker-owner-a"
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_live_owner_executes_locally_without_outbox_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_session_owner(
            &pool,
            "claude:local-owner",
            "claude",
            "ch-local-owner",
            "leader-1",
            "idle",
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-local-owner"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::LiveSessionOwnerIsLocal
            }
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = 'ch-local-owner'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_session_rows_collapse_to_one_owner_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent_with_preference(
            &pool,
            "agent-owner-dupe",
            "ch-owner-dupe",
            serde_json::json!([]),
        )
        .await;
        seed_worker_node(&pool, "worker-one", serde_json::json!([]), "online").await;
        for session_key in ["legacy:owner-dupe", "namespaced:owner-dupe"] {
            seed_session_owner(
                &pool,
                session_key,
                "claude",
                "ch-owner-dupe",
                "worker-one",
                "turn_active",
            )
            .await;
        }
        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-owner-dupe"),
        )
        .await;
        assert!(matches!(
            decision,
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                ..
            } if target_instance_id == "worker-one"
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preserving_request_blocks_legacy_live_owner_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_worker_node_with_capabilities(
            &pool,
            "worker-legacy-owner",
            serde_json::json!([]),
            "online",
            serde_json::json!({
                "intake_worker": { "enabled": true, "providers": ["claude"] }
            }),
        )
        .await;
        seed_session_owner(
            &pool,
            "claude:legacy-owner",
            "claude",
            "ch-legacy-owner",
            "worker-legacy-owner",
            "turn_active",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-legacy-owner");
        ctx.preserve_on_cancel = true;
        assert_eq!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::OwnerProtocolIncompatible {
                    instance_id: "worker-legacy-owner".to_string(),
                }
            }
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preserving_request_forwards_to_capable_live_owner_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_worker_node(
            &pool,
            "worker-capable-owner",
            serde_json::json!([]),
            "online",
        )
        .await;
        seed_session_owner(
            &pool,
            "claude:capable-owner",
            "claude",
            "ch-capable-owner",
            "worker-capable-owner",
            "turn_active",
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-capable-owner");
        ctx.preserve_on_cancel = true;
        assert!(matches!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                ..
            } if target_instance_id == "worker-capable-owner"
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_owner_never_falls_to_preference_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent_with_preference(
            &pool,
            "agent-stale-owner",
            "ch-stale-owner",
            serde_json::json!(["preferred"]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-preferred",
            serde_json::json!(["preferred"]),
            "online",
        )
        .await;
        seed_session_owner(
            &pool,
            "claude:stale-owner",
            "claude",
            "ch-stale-owner",
            "worker-missing",
            "turn_active",
        )
        .await;

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-stale-owner"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::StaleSessionOwners {
                    instance_ids: vec!["worker-missing".to_string()]
                }
            }
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = 'ch-stale-owner'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expired_or_provider_incapable_owner_is_stale_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        seed_worker_node(&pool, "worker-expired", serde_json::json!([]), "online").await;
        sqlx::query(
            "UPDATE worker_nodes
                SET last_heartbeat_at = NOW() - INTERVAL '1 day'
              WHERE instance_id = 'worker-expired'",
        )
        .execute(&pool)
        .await
        .expect("expire worker");
        seed_worker_node_with_capabilities(
            &pool,
            "worker-no-provider",
            serde_json::json!([]),
            "online",
            serde_json::json!({}),
        )
        .await;

        for (channel_id, owner) in [
            ("ch-owner-expired", "worker-expired"),
            ("ch-owner-no-provider", "worker-no-provider"),
        ] {
            seed_session_owner(
                &pool,
                &format!("claude:{channel_id}"),
                "claude",
                channel_id,
                owner,
                "turn_active",
            )
            .await;
            let decision = try_route_intake(
                &pool,
                &ctx_for_channel(IntakeRoutingMode::Enforce, channel_id),
            )
            .await;
            assert_eq!(
                decision,
                IntakeRouterDecision::Blocked {
                    reason: IntakeBlockedReason::StaleSessionOwners {
                        instance_ids: vec![owner.to_string()]
                    }
                }
            );
        }

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_owner_recovers_when_worker_is_fresh_again_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_worker_node(&pool, "worker-recovering", serde_json::json!([]), "offline").await;
        seed_session_owner(
            &pool,
            "claude:recovering-owner",
            "claude",
            "ch-recovering-owner",
            "worker-recovering",
            "turn_active",
        )
        .await;
        let ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-recovering-owner");
        assert!(matches!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::StaleSessionOwners { .. }
            }
        ));

        sqlx::query(
            "UPDATE worker_nodes
                SET status = 'online', last_heartbeat_at = NOW()
              WHERE instance_id = 'worker-recovering'",
        )
        .execute(&pool)
        .await
        .expect("recover worker");
        assert!(matches!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                ..
            } if target_instance_id == "worker-recovering"
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn conflicting_live_owners_block_arbitrary_placement_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        for owner in ["worker-a", "worker-b"] {
            seed_worker_node(&pool, owner, serde_json::json!([]), "online").await;
            seed_session_owner(
                &pool,
                &format!("claude:{owner}"),
                "claude",
                "ch-owner-conflict",
                owner,
                "turn_active",
            )
            .await;
        }

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-owner-conflict"),
        )
        .await;
        assert_eq!(
            decision,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::ConflictingLiveSessionOwners {
                    instance_ids: vec!["worker-a".to_string(), "worker-b".to_string()]
                }
            }
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_live_owner_beats_stale_duplicate_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent_with_preference(
            &pool,
            "agent-live-stale",
            "ch-live-stale",
            serde_json::json!([]),
        )
        .await;
        seed_worker_node(&pool, "worker-live", serde_json::json!([]), "online").await;
        for (key, owner) in [
            ("claude:live-owner", "worker-live"),
            ("claude:stale-dupe", "worker-stale"),
        ] {
            seed_session_owner(&pool, key, "claude", "ch-live-stale", owner, "turn_active").await;
        }
        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-live-stale"),
        )
        .await;
        assert!(matches!(
            decision,
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                ..
            } if target_instance_id == "worker-live"
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn paired_provider_owner_and_outbox_use_current_bot_provider_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent_with_preference(
            &pool,
            "agent-paired-provider",
            "ch-paired-provider",
            serde_json::json!([]),
        )
        .await;
        seed_session_owner(
            &pool,
            "claude:paired",
            "claude",
            "ch-paired-provider",
            "worker-claude",
            "turn_active",
        )
        .await;
        seed_session_owner(
            &pool,
            "codex:paired",
            "codex",
            "ch-paired-provider",
            "worker-codex",
            "turn_active",
        )
        .await;
        seed_worker_node_with_capabilities(
            &pool,
            "worker-codex",
            serde_json::json!([]),
            "online",
            serde_json::json!({
                "intake_worker": { "enabled": true, "providers": ["codex"] }
            }),
        )
        .await;
        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-paired-provider");
        ctx.provider = "codex";
        let decision = try_route_intake(&pool, &ctx).await;
        let outbox_id = match decision {
            IntakeRouterDecision::Forwarded {
                target_instance_id,
                outbox_id,
            } => {
                assert_eq!(target_instance_id, "worker-codex");
                outbox_id
            }
            other => panic!("expected codex owner, got {other:?}"),
        };
        let provider: String =
            sqlx::query_scalar("SELECT provider FROM intake_outbox WHERE id = $1")
                .bind(outbox_id)
                .fetch_one(&pool)
                .await
                .expect("provider");
        assert_eq!(provider, "codex");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn foreign_owner_attachment_is_blocked_without_outbox_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_worker_node(
            &pool,
            "worker-upload-owner",
            serde_json::json!([]),
            "online",
        )
        .await;
        seed_session_owner(
            &pool,
            "claude:upload-owner",
            "claude",
            "ch-upload-owner",
            "worker-upload-owner",
            "turn_active",
        )
        .await;
        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-upload-owner");
        ctx.has_nonportable_uploads = true;
        let decision = try_route_intake(&pool, &ctx).await;
        assert_eq!(
            decision,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::NonPortableAttachment {
                    owner_instance_id: "worker-upload-owner".to_string()
                }
            }
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = 'ch-upload-owner'",
        )
        .fetch_one(&pool)
        .await
        .expect("count");
        assert_eq!(count, 0);

        sqlx::query(
            "INSERT INTO intake_outbox (
                target_instance_id, forwarded_by_instance_id, required_labels,
                channel_id, user_msg_id, request_owner_id, user_text,
                turn_kind, agent_id, status, attempt_no
             ) VALUES (
                'worker-upload-owner', 'leader-1', '[]'::JSONB,
                'ch-upload-owner', 'prior-upload-message', '50', 'prior',
                'foreground', '', 'pending', 1
             )",
        )
        .execute(&pool)
        .await
        .expect("seed prior open route");
        assert_eq!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::NonPortableAttachment {
                    owner_instance_id: "worker-upload-owner".to_string()
                }
            },
            "a prior open route must not hide a live ingress attachment block"
        );
        let count_with_prior: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = 'ch-upload-owner'",
        )
        .fetch_one(&pool)
        .await
        .expect("count prior route");
        assert_eq!(count_with_prior, 1);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_owner_attachment_establishes_local_first_placement_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-new-upload");
        ctx.has_nonportable_uploads = true;
        let decision = try_route_intake(&pool, &ctx).await;
        assert_eq!(
            decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NoOwnerWithNonPortableAttachment
            }
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn enforce_owner_lookup_error_blocks_local_execution_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query("DROP TABLE sessions")
            .execute(&pool)
            .await
            .expect("drop sessions for lookup error");

        let decision = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-owner-error"),
        )
        .await;
        assert!(matches!(
            decision,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::OwnerLookupFailed { .. }
            }
        ));

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_owner_agent_lookup_error_has_observe_enforce_parity_without_mutation_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        sqlx::query("DROP TABLE agents CASCADE")
            .execute(&pool)
            .await
            .expect("drop agents for preference lookup error");

        let enforce = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-preference-error"),
        )
        .await;
        let observe = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Observe, "ch-preference-error"),
        )
        .await;
        let IntakeRouterDecision::RanLocal { reason } = enforce else {
            panic!("expected enforce availability fallback, got {enforce:?}");
        };
        assert!(matches!(
            reason,
            RanLocalReason::DbErrorFellBackToLocal { .. }
        ));
        assert_eq!(
            observe,
            IntakeRouterDecision::Observed {
                outcome: ObservedIntakeOutcome::WouldKeepNoOwnerLocal { reason }
            }
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-preference-error")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "observe dependency fallback must not insert");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_owner_worker_lookup_error_has_observe_enforce_parity_without_mutation_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_agent_with_preference(
            &pool,
            "agent-worker-lookup-error",
            "ch-worker-lookup-error",
            serde_json::json!(["mini"]),
        )
        .await;
        sqlx::query("DROP TABLE worker_nodes CASCADE")
            .execute(&pool)
            .await
            .expect("drop worker_nodes for preference routing error");

        let enforce = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, "ch-worker-lookup-error"),
        )
        .await;
        let observe = try_route_intake(
            &pool,
            &ctx_for_channel(IntakeRoutingMode::Observe, "ch-worker-lookup-error"),
        )
        .await;
        let IntakeRouterDecision::RanLocal { reason } = enforce else {
            panic!("expected enforce availability fallback, got {enforce:?}");
        };
        assert!(matches!(
            reason,
            RanLocalReason::DbErrorFellBackToLocal { .. }
        ));
        assert_eq!(
            observe,
            IntakeRouterDecision::Observed {
                outcome: ObservedIntakeOutcome::WouldKeepNoOwnerLocal { reason }
            }
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
                .bind("ch-worker-lookup-error")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(count, 0, "observe dependency fallback must not insert");

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
            serde_json::json!(["preferred"]),
        )
        .await;
        seed_worker_node(
            &pool,
            "worker-preferred-but-not-selected",
            serde_json::json!(["preferred"]),
            "online",
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
    async fn preserving_node_override_rejects_legacy_worker_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        seed_worker_node_with_capabilities(
            &pool,
            "worker-legacy-selected",
            serde_json::json!([]),
            "online",
            serde_json::json!({
                "intake_worker": { "enabled": true, "providers": ["claude"] }
            }),
        )
        .await;

        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, "ch-legacy-override");
        ctx.node_override_instance_id = Some("worker-legacy-selected");
        ctx.preserve_on_cancel = true;
        assert_eq!(
            try_route_intake(&pool, &ctx).await,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::OverrideUnavailable {
                    target_instance_id: "worker-legacy-selected".to_string(),
                }
            }
        );

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
    async fn node_override_unavailable_blocks_without_inserting() {
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
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::OverrideUnavailable {
                    target_instance_id: "worker-offline".to_string()
                }
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
    async fn node_override_without_intake_capability_blocks_without_inserting() {
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
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::OverrideUnavailable {
                    target_instance_id: "worker-online-no-consumer".to_string()
                }
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
    async fn distinct_open_route_never_executes_local_pg() {
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
            IntakeRouterDecision::DeferredOpenRoute {
                target_instance_id: "worker-conflict".to_string()
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
