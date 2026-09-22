//! Ownerless intake placement by operator preference and explicit target.
use super::*;

fn preferred_label_dependency_fallback(detail: String) -> IntakeRouterDecision {
    IntakeRouterDecision::RanLocal {
        reason: RanLocalReason::DbErrorFellBackToLocal { detail },
    }
}

pub(super) async fn route_by_preferred_labels(
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

    let auth_profile = crate::services::cluster::readiness::expected_auth_profile(
        ctx.provider,
        ctx.channel_id,
        &agent_id,
    );
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
                    ) && crate::services::cluster::readiness::evaluate_declared(
                        node,
                        ctx.provider,
                        &auth_profile,
                    )
                    .eligible
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

    if ctx.has_nonportable_uploads {
        return apply_observe_mode(
            ctx.mode,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::NonPortableAttachmentRoutedTarget {
                    target_instance_id: target,
                },
            },
        );
    }

    route_to_instance(
        pool,
        ctx,
        &target,
        &preferred_labels,
        &agent_id,
        ObserveTargetKind::PreferredLabels,
    )
    .await
}

pub(super) async fn route_node_override_without_owner(
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

    if ctx.has_nonportable_uploads {
        return apply_observe_mode(
            ctx.mode,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::NonPortableAttachmentRoutedTarget {
                    target_instance_id: target.to_string(),
                },
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
        ObserveTargetKind::NodeOverride,
    )
    .await
}

#[derive(Clone, Copy)]
pub(super) enum ObserveTargetKind {
    LiveForeignOwner,
    NodeOverride,
    PreferredLabels,
}
