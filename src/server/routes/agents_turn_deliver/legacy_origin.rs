//! Origin protection for Legacy HTTP input without an iMessage reply obligation.

use sqlx::PgPool;

use super::{
    HealthRegistry, HumanInputRequest, RouteResponse, delivery_response, observed_delivery,
    preflight_human_input,
};
use crate::services::imsg_reply::{AdmissionInput, admission};

pub(super) async fn deliver(
    pool: &PgPool,
    registry: &HealthRegistry,
    input: &AdmissionInput<'_>,
    request: HumanInputRequest,
) -> RouteResponse {
    let provider = request.provider.clone();
    let channel = request.channel_id;
    admission::deliver_once(
        pool,
        input,
        "external",
        || preflight_human_input(registry, channel, &provider, input.author_id),
        || observed_delivery(registry, request, None),
        |result| delivery_response(channel.get(), result),
    )
    .await
}
