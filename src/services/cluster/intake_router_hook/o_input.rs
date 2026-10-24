//! Input placement preserves router fences ahead of fresh writer admission.

use super::*;

pub(super) async fn route(pool: &PgPool, ctx: &IntakeRouterContext<'_>) -> IntakeRouterDecision {
    // The actual Discord destination decides O ownership, never the policy channel.
    match intake_route::text_for_input_placement(ctx.provider, ctx.channel_id).await {
        IntakeRoute::Unselected => route_intake(pool, ctx).await,
        IntakeRoute::Hold(detail) => required_block(detail),
        IntakeRoute::Gateway => match route_intake(pool, ctx).await {
            IntakeRouterDecision::DeferredOpenRoute {
                target_instance_id, ..
            } if target_instance_id != ctx.leader_instance_id => required_block(format!(
                "O channel has an open route on {target_instance_id}, off its gateway"
            )),
            decision => decision,
        },
    }
}
