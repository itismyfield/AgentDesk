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
    line: Option<&str>,
) -> RouteResponse {
    let channel = request.channel_id;
    let identity = format!("guid:{}", input.origin_id);
    if let Err(error) =
        preflight_human_input(registry, channel, &request.provider, input.author_id).await
    {
        super::super::imessage_input_notice::not_echoed(
            line,
            channel.get(),
            &identity,
            "preflight_error",
        );
        return delivery_response(channel.get(), Err(error));
    }
    if let Some(response) = admission::replay_existing(pool, input, "external").await {
        super::super::imessage_input_notice::not_echoed(line, channel.get(), &identity, "replay");
        return response;
    }
    #[cfg(all(test, unix))]
    super::super::imessage_input_notice_tests::before_legacy_prepare().await;
    let prepared = match registry.prepare_external_human_input(request).await {
        Ok(prepared) => prepared,
        Err(error) => {
            // A concurrent winner still owns its response if readiness changed during preparation.
            super::super::imessage_input_notice::not_echoed(
                line,
                channel.get(),
                &identity,
                "race_replay_or_refusal",
            );
            return admission::replay_existing(pool, input, "external")
                .await
                .unwrap_or_else(|| delivery_response(channel.get(), Err(error)));
        }
    };
    let mut executed = false;
    let ran = &mut executed;
    let response = admission::deliver_once(
        pool,
        input,
        "external",
        || async { Ok(()) },
        || async {
            *ran = true;
            super::super::imessage_input_notice::delivered(
                pool,
                channel.get(),
                &identity,
                line,
                observed_execution(prepared).await,
            )
            .await
        },
        |result| delivery_response(channel.get(), result),
    )
    .await;
    if !executed {
        super::super::imessage_input_notice::not_echoed(
            line,
            channel.get(),
            &identity,
            response.0.as_str(),
        );
    }
    response
}
