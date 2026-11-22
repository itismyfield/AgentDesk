use axum::http::StatusCode;
use sqlx::PgPool;

use crate::services::imsg_reply::admission;
use crate::services::imsg_reply::agy_input::{self, ADMITTED_DELIVERY, Admitted, WriterGate};
use crate::services::tui_o::store::agy_spool::ImessageSource;

use super::*;

pub(in crate::server::routes) fn start_redirect(
    target: &AgentTurnTarget,
    source: Option<&str>,
    metadata: Option<&Value>,
    dm_requested: bool,
) -> Option<RouteResponse> {
    if dm_requested
        || source.map(str::trim) != Some("imessage")
        || metadata.is_some_and(|value| {
            value.get("routine_id").is_some() || value.get("scheduled_message_id").is_some()
        })
        || agy_input::selected(&target.provider, target.channel_id).is_none()
    {
        return None;
    }
    Some(failure(StatusCode::CONFLICT, "imessage_deliver_required"))
}

pub(super) async fn deliver(
    pool: &PgPool,
    (registry, target): (&HealthRegistry, &AgentTurnTarget),
    gate: WriterGate,
    request: HumanInputRequest,
    (agent, origin, notice): (&str, Option<&str>, Option<&str>),
    obligations_enabled: bool,
) -> RouteResponse {
    if request.source != "imessage" {
        return failure(StatusCode::UNPROCESSABLE_ENTITY, "unsupported_input_origin");
    }
    let Some(origin) = origin.filter(|origin| !origin.trim().is_empty()) else {
        return bad_request("origin_id_required");
    };
    let Some(source) = ImessageSource::from_guid(origin) else {
        return bad_request("invalid_origin");
    };
    let destination = target.channel_id;
    let logical_channel = target.logical_channel();
    let thread = target.thread.as_ref();
    if let Err(error) = preflight_delivery(registry, thread, &request).await {
        return delivery_response(destination, Err(error));
    }
    let text = request.text.clone();
    let input = imsg_reply::AdmissionInput {
        origin_id: origin,
        agent_id: agent,
        provider: target.provider.as_str(),
        channel_id: destination,
        author_id: request.author_id,
        source: "imessage",
        text: &text,
    };
    let storage_source = if obligations_enabled {
        "imessage"
    } else {
        "external"
    };
    let hash = agy_input::request_hash(&input, logical_channel);
    if let Some(response) = admission::replay_with_hash(pool, &input, storage_source, &hash).await {
        return response;
    }
    let Some(selection) = gate.ready else {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "agy_writer_unavailable");
    };
    if selection.logical_channel != logical_channel {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "agy_writer_target_changed");
    }
    let binding = imsg_reply::ReplyObligation::new(pool.clone(), origin);
    let author = request.author_id;
    let prepared = match registry
        .prepare_external_human_input_on(
            request,
            obligations_enabled.then(|| binding.clone()),
            thread,
        )
        .await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            return admission::replay_with_hash(pool, &input, storage_source, &hash)
                .await
                .unwrap_or_else(|| delivery_response(destination, Err(error)));
        }
    };
    admission::deliver_with_hash(
        pool,
        (&input, storage_source, &hash),
        || async { Ok(()) },
        || async {
            let admitted = Admitted::new(selection, source, origin.to_owned(), author, binding);
            let result = ADMITTED_DELIVERY
                .scope(admitted, observed_execution(prepared))
                .await;
            super::super::imessage_input_notice::delivered(
                pool,
                destination,
                &format!("guid:{origin}"),
                notice,
                result,
            )
            .await
        },
        |result| delivery_response(destination, result),
    )
    .await
}
