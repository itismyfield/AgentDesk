//! Ownerless placement: per-agent primary, compatible fallback, and bounded capacity.
use super::*;
use crate::services::cluster::intake_routing::{
    IntakeRouteTarget, LocalRouteReason, candidates_from_worker_nodes_json, pick_intake_target,
};
use crate::services::cluster::{
    attachment_transfer, execution_capacity, intake_routing, readiness,
};

fn preferred_label_dependency_fallback(detail: String) -> IntakeRouterDecision {
    IntakeRouterDecision::RanLocal {
        reason: RanLocalReason::DbErrorFellBackToLocal { detail },
    }
}

pub(super) async fn route_by_preference(
    pool: &PgPool,
    ctx: &IntakeRouterContext<'_>,
    requirements: &ExecutionRequirements,
    preferred_node: Option<&str>,
) -> IntakeRouterDecision {
    // A per-agent primary opts only this agent into bounded compatible fallback.
    // Agents without it retain the existing global/label routing policy.
    let capacity_aware = preferred_node.is_some() || execution_capacity::automatic_enabled();
    // Resolve agent + preference. NoAgentForChannel is NOT an error —
    // many channels (DMs, ad-hoc cross-bot) have no agent row.
    //
    // #4349: the agent's own `provider` column is deliberately ignored for
    // routing. It is a single value shared by the agent's cc and cdx
    // channels, so on a paired agent it disagrees with the bot that is
    // actually handling this message. `ctx.provider` is that bot.
    let (agent_id, _agent_provider, preferred_labels) =
        match agent_id_and_preferred_labels(pool, ctx.policy_channel_id).await {
            Ok(Some((agent_id, provider, labels))) => (agent_id, provider, labels),
            Ok(None) => {
                if !requirements.is_empty() || preferred_node.is_some() {
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
                if !requirements.is_empty() || preferred_node.is_some() {
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

    let auth_profile = readiness::expected_auth_profile(ctx.provider, ctx.channel_id, &agent_id);
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
                    ) && readiness::evaluate_declared(node, ctx.provider, &auth_profile).eligible
                        && required_node_reasons(node, requirements).is_empty()
                        && (preferred_node.is_none()
                            || readiness::evaluate(
                                node,
                                ctx.provider,
                                &auth_profile,
                                chrono::Utc::now().timestamp_millis(),
                            )
                            .eligible)
                        && (ctx.attachment_refs.is_empty() || attachment_transfer::supports(node))
                })
                .collect();
            if capacity_aware {
                execution_capacity::rank(&mut eligible_nodes);
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
        let selection = if let Some(primary) = preferred_node {
            intake_routing::pick_preferred_node_target(
                &candidates,
                primary,
                &preferred_labels,
                ctx.leader_instance_id,
            )
        } else if requirements.is_empty() && !capacity_aware {
            pick_intake_target(&candidates, &preferred_labels, ctx.leader_instance_id)
        } else {
            intake_routing::pick_required_intake_target(
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
                            LocalRouteReason::PreferredNodeIsLeader => {
                                RanLocalReason::AgentDefaultIsLeader
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
            if preferred_node.is_some() {
                ObserveTargetKind::AgentDefault
            } else {
                ObserveTargetKind::PreferredLabels
            },
            requirements,
        )
        .await;
        if capacity_aware
            && matches!(&decision, IntakeRouterDecision::Blocked {
        reason: IntakeBlockedReason::RoutingDependencyFailed { detail }
    } if detail == execution_capacity::EXHAUSTED)
        {
            candidates.retain(|candidate| candidate.instance_id != target);
            continue;
        }
        return decision;
    }
}
