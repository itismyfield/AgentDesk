//! Ownerless placement constrained by hard execution policy.
use super::*;

fn preferred_label_dependency_fallback(detail: String) -> IntakeRouterDecision {
    IntakeRouterDecision::RanLocal {
        reason: RanLocalReason::DbErrorFellBackToLocal { detail },
    }
}

pub(super) async fn route_by_preferred_labels(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
    requirements: &ExecutionRequirements,
) -> IntakeRouterDecision {
    let capacity_aware = crate::services::cluster::execution_capacity::automatic_enabled();
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
                if !requirements.is_empty() {
                    return required_block(
                        "agent disappeared while validating execution requirements".into(),
                    );
                }
                return apply_observe_mode(
                    ctx.mode,
                    IntakeRouterDecision::RanLocal {
                        reason: RanLocalReason::NoAgentForChannel,
                    },
                );
            }
            Err(error) => {
                if !requirements.is_empty() {
                    return required_block(error.to_string());
                }
                return apply_observe_mode(
                    ctx.mode,
                    preferred_label_dependency_fallback(format!("agent lookup: {error}")),
                );
            }
        };

    if preferred_labels.is_empty() && requirements.is_empty() && !capacity_aware {
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
    let mut candidates = match crate::services::cluster::node_registry::list_worker_nodes(
        pool,
        worker_heartbeat_lease_secs(),
    )
    .await
    {
        Ok(nodes) => {
            let mut eligible_nodes: Vec<_> = nodes
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
                        && required_node_reasons(node, requirements).is_empty()
                        && (ctx.attachment_refs.is_empty()
                            || crate::services::cluster::attachment_transfer::supports(node))
                })
                .collect();
            if capacity_aware {
                crate::services::cluster::execution_capacity::rank(&mut eligible_nodes);
            }
            candidates_from_worker_nodes_json(&eligible_nodes)
        }
        Err(error) => {
            if !requirements.is_empty() || capacity_aware {
                return required_block(error);
            }
            return apply_observe_mode(
                ctx.mode,
                preferred_label_dependency_fallback(format!("list worker_nodes: {error}")),
            );
        }
    };

    loop {
        let selection = if requirements.is_empty() && !capacity_aware {
            pick_intake_target(&candidates, &preferred_labels, ctx.leader_instance_id)
        } else {
            crate::services::cluster::intake_routing::pick_required_intake_target(
                &candidates,
                &preferred_labels,
                ctx.leader_instance_id,
            )
        };
        let target = match selection {
            IntakeRouteTarget::Worker { instance_id } => instance_id,
            IntakeRouteTarget::Local { reason } => {
                if (!requirements.is_empty() || capacity_aware)
                    && reason == LocalRouteReason::NoEligibleWorker
                {
                    return required_block("no ready worker has capacity and satisfies the execution requirements; retry when capacity is available".into());
                }
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

        let decision = route_to_instance(
            pool,
            ctx,
            &target,
            if requirements.is_empty() && !capacity_aware {
                &preferred_labels
            } else {
                &[]
            },
            &agent_id,
            ObserveTargetKind::PreferredLabels,
            requirements,
        )
        .await;
        if capacity_aware
            && matches!(&decision, IntakeRouterDecision::Blocked {
        reason: IntakeBlockedReason::RoutingDependencyFailed { detail }
    } if detail == crate::services::cluster::execution_capacity::EXHAUSTED)
        {
            candidates.retain(|candidate| candidate.instance_id != target);
            continue;
        }
        return decision;
    }
}

pub(super) async fn route_node_override_without_owner(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
    target: &str,
    requirements: &ExecutionRequirements,
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

    if let Some(blocked) = check_required_target(pool, ctx, target, requirements).await {
        return blocked;
    }
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
        requirements,
    )
    .await
}

#[derive(Clone, Copy)]
pub(super) enum ObserveTargetKind {
    LiveForeignOwner,
    NodeOverride,
    PreferredLabels,
}
