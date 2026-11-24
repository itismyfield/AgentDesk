//! Approved Decision events for tests, inserted as the service that approves one will.
use super::EventKind;
use sqlx::PgPool;
use uuid::Uuid;

/// Inserts an approved event of `text` and its workflow row in `state`.
pub(crate) async fn insert_event(
    pool: &PgPool,
    (kind, channel, actor): (EventKind, u64, u64),
    text: &str,
    state: &str,
) -> Uuid {
    let event_id = Uuid::new_v4();
    let (kind, origin) = match kind {
        EventKind::Selection => ("selection", format!("decision:{event_id}")),
        EventKind::Opinion => ("opinion", format!("decision:{event_id}:opinion:77")),
    };
    sqlx::query(
        "INSERT INTO decision_input_events (event_id, kind, source_key, origin,
             requester_agent_id, logical_channel_id, actor_user_id, text_sha256)
         VALUES ($1, $2, $3, $3, 'decision-agent', $4, $5, $6)",
    )
    .bind(event_id)
    .bind(kind)
    .bind(origin)
    .bind(channel.to_string())
    .bind(actor.to_string())
    .bind(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(
        text.as_bytes(),
    )))
    .execute(pool)
    .await
    .unwrap();
    // A started workflow is held by the node whose run moved it: here, this one.
    let holder = (state != "not_started")
        .then(crate::services::cluster::node_registry::resolve_self_instance_id_without_config);
    sqlx::query(
        "INSERT INTO decision_delivery_attempts (event_id, state, holder_instance_id)
         VALUES ($1, $2, $3)",
    )
    .bind(event_id)
    .bind(state)
    .bind(holder)
    .execute(pool)
    .await
    .unwrap();
    event_id
}

/// The workflow row: state, whether normalization was used, the stored ticket, turn and detail.
pub(crate) async fn row(
    pool: &PgPool,
    event: Uuid,
) -> (String, bool, Option<String>, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT state, normalization_used, workflow_ticket, attempt_turn_id, detail
         FROM decision_delivery_attempts WHERE event_id = $1",
    )
    .bind(event)
    .fetch_one(pool)
    .await
    .unwrap()
}
