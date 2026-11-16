//! Configured Herdr input prepares before admission; other hosts retain lazy delivery.
use super::{
    HealthRegistry, HumanInputRequest, RouteResponse, delivery_response, observed_delivery,
    observed_execution, preflight_human_input,
};
use crate::services::imsg_reply::{AdmissionInput, ReplyObligation, admission};
use sqlx::PgPool;

pub(super) async fn deliver(
    pool: &PgPool,
    registry: &HealthRegistry,
    input: &AdmissionInput<'_>,
    request: HumanInputRequest,
    line: Option<&str>,
    reply: ReplyObligation,
) -> RouteResponse {
    let channel = request.channel_id;
    if !crate::config::session_hosts::has_herdr_endpoint(channel.get()) {
        return existing_delivery(pool, registry, input, request, line, reply).await;
    }
    let admission_source = "imessage";
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
    if let Some(response) = admission::replay_existing(pool, input, admission_source).await {
        super::super::imessage_input_notice::not_echoed(line, channel.get(), &identity, "replay");
        return response;
    }
    let prepared = match registry
        .prepare_external_human_input(request, Some(reply))
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            // A concurrent winner still owns its response if readiness changed during preparation.
            super::super::imessage_input_notice::not_echoed(
                line,
                channel.get(),
                &identity,
                "race_replay_or_refusal",
            );
            return admission::replay_existing(pool, input, admission_source)
                .await
                .unwrap_or_else(|| delivery_response(channel.get(), Err(error)));
        }
    };
    let mut executed = false;
    let ran = &mut executed;
    let deliver = || async {
        *ran = true;
        super::super::imessage_input_notice::delivered(
            pool,
            channel.get(),
            &identity,
            line,
            observed_execution(prepared).await,
        )
        .await
    };
    let authorize = || async { Ok(()) };
    let serialize = |result| delivery_response(channel.get(), result);
    let response =
        crate::services::imsg_reply::deliver_once(pool, input, authorize, deliver, serialize).await;
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

async fn existing_delivery(
    pool: &PgPool,
    registry: &HealthRegistry,
    input: &AdmissionInput<'_>,
    request: HumanInputRequest,
    line: Option<&str>,
    reply: ReplyObligation,
) -> RouteResponse {
    let channel = request.channel_id;
    let provider = request.provider.clone();
    let identity = format!("guid:{}", input.origin_id);
    let mut executed = false;
    let ran = &mut executed;
    let response = crate::services::imsg_reply::deliver_once(
        pool,
        input,
        || preflight_human_input(registry, channel, &provider, input.author_id),
        || async {
            *ran = true;
            super::super::imessage_input_notice::delivered(
                pool,
                channel.get(),
                &identity,
                line,
                observed_delivery(registry, request, Some(reply)).await,
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
