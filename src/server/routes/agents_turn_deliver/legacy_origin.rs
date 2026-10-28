//! Origin protection for Legacy HTTP input without an iMessage reply obligation.

use sqlx::PgPool;

use super::{
    HealthRegistry, HumanInputRequest, RouteResponse, delivery_response, observed_execution,
    preflight_human_input,
};
use crate::services::imsg_reply::{AdmissionInput, admission};

pub(super) async fn deliver(
    pool: &PgPool,
    registry: &HealthRegistry,
    input: &AdmissionInput<'_>,
    request: HumanInputRequest,
) -> RouteResponse {
    let channel = request.channel_id;
    if let Err(error) =
        preflight_human_input(registry, channel, &request.provider, input.author_id).await
    {
        return delivery_response(channel.get(), Err(error));
    }
    if let Some(response) = admission::replay_existing(pool, input, "external").await {
        return response;
    }
    let prepared = match registry.prepare_external_human_input(request).await {
        Ok(prepared) => prepared,
        Err(error) => {
            // A concurrent winner still owns its response if readiness changed during preparation.
            return admission::replay_existing(pool, input, "external")
                .await
                .unwrap_or_else(|| delivery_response(channel.get(), Err(error)));
        }
    };
    admission::deliver_once(
        pool,
        input,
        "external",
        || async { Ok(()) },
        || observed_execution(prepared),
        |result| delivery_response(channel.get(), result),
    )
    .await
}
