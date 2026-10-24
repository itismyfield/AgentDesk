//! Input placement keeps the home/router fences ahead of fresh writer admission.

use super::*;

pub(super) async fn route(pool: &PgPool, ctx: &IntakeRouterContext<'_>) -> IntakeRouterDecision {
    if let Some(reason) = ctx
        .channel_id
        .parse()
        .ok()
        .and_then(super::super::home_availability::refusal)
    {
        return required_block(format!("delegation unavailable: {reason}"));
    }
    // A channel's home row, read on every intake in every mode, outranks all other placement.
    match o_channel_homes::read_home(pool, ctx.channel_id).await {
        Ok(None) => {}
        Ok(Some(home)) => return route_to_home(pool, ctx, home).await,
        Err(error) => {
            let detail = error.to_string();
            return home_block(HomeBlock::Unreadable { detail });
        }
    }
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
