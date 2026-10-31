//! Historical routing notices keep their original outbox pointer after any later owner change.
use super::{OwnerIdentity, custody, fallback::notice};
use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx,
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

pub(crate) const SOURCE: &str = "routing_transition";
/// Notices post as the notify utility bot, which carries non-turn provenance.
pub(crate) const BOT: &str = "notify";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Event {
    pub generation: i64,
    pub from: String,
    pub to: String,
    pub home: String,
    pub target: String,
    pub session_key: String,
    pub reason_code: String,
    pub nonce: String,
    pub content: String,
    pub bot: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryState {
    Pending,
    Posting,
    Sent,
    Uncertain,
}
impl DeliveryState {
    fn read(state: Option<&str>) -> Result<Self, sqlx::Error> {
        match state {
            Some("pending") => Ok(Self::Pending),
            Some("posting") => Ok(Self::Posting),
            Some("sent") => Ok(Self::Sent),
            Some("uncertain") => Ok(Self::Uncertain),
            _ => Err(invalid("unknown notice delivery state")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reservation {
    pub outbox_id: i64,
    pub event: Event,
    pub state: DeliveryState,
    pub post_started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub message_id: Option<String>,
}

#[derive(sqlx::FromRow)]
struct StoredNotice {
    owner_instance_id: String,
    status: String,
    routing_notice: Option<sqlx::types::Json<Event>>,
    routing_notice_outbox_id: Option<i64>,
    routing_notice_delivery_state: Option<String>,
    routing_notice_started_at: Option<chrono::DateTime<chrono::Utc>>,
    routing_notice_message_id: Option<String>,
}

pub(crate) struct Description<'a> {
    pub home: &'a str,
    pub home_label: &'a str,
    pub target_label: &'a str,
    pub new_session: bool,
    pub home_changed: bool,
    pub bot: &'a str,
}
fn invalid(error: impl ToString) -> sqlx::Error {
    sqlx::Error::Protocol(error.to_string())
}

/// The caller commits this together with owner CAS and admission, or rolls all three back.
pub(crate) async fn reserve_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    generation: i64,
    description: Description<'_>,
) -> Result<Option<Reservation>, sqlx::Error> {
    custody::lock(tx, id).await?;
    let stored: Option<StoredNotice> = sqlx::query_as("SELECT owner_instance_id,status,routing_notice,routing_notice_outbox_id,routing_notice_delivery_state,routing_notice_started_at,routing_notice_message_id FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3 FOR UPDATE")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(generation).fetch_optional(&mut **tx).await?;
    let Some(StoredNotice {
        owner_instance_id: to,
        status,
        routing_notice: event,
        routing_notice_outbox_id: pointer,
        routing_notice_delivery_state,
        routing_notice_started_at,
        routing_notice_message_id,
    }) = stored
    else {
        return Ok(None);
    };
    #[cfg(test)]
    let skip_pointer = super::cleanup::tests::mutant("notice-pointer");
    #[cfg(not(test))]
    let skip_pointer = false;
    if !skip_pointer && let Some(pointer) = pointer {
        let event = event
            .ok_or_else(|| invalid("notice pointer has no event"))?
            .0;
        if event.generation != generation
            || event.to != to
            || event.target != format!("channel:{}", id.raw_channel_id())
        {
            return Err(invalid("historical event identity changed"));
        }
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM message_outbox WHERE id=$1 AND target=$2 AND session_key=$3 AND reason_code=$4 AND source=$5 AND content=$6 AND bot=$7)")
            .bind(pointer).bind(&event.target).bind(&event.session_key).bind(&event.reason_code).bind(SOURCE).bind(&event.content).bind(&event.bot).fetch_one(&mut **tx).await?;
        if !exists {
            return Err(invalid("historical notice pointer cannot be verified"));
        }
        return Ok(Some(Reservation {
            outbox_id: pointer,
            event,
            state: DeliveryState::read(routing_notice_delivery_state.as_deref())?,
            post_started_at: routing_notice_started_at,
            message_id: routing_notice_message_id,
        }));
    }
    if status != "active" {
        return Ok(None);
    }
    let previous: Option<String> = sqlx::query_scalar("SELECT owner_instance_id FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND generation<$3 ORDER BY generation DESC LIMIT 1")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(generation).fetch_optional(&mut **tx).await?;
    let from = previous.unwrap_or_else(|| description.home.into());
    if from == to {
        return Ok(None);
    }
    let Some(prepared) = notice::prepare(
        &notice::Transition {
            identity: id,
            generation,
            from: &from,
            to: &to,
            home: description.home,
        },
        notice::Labels {
            home: description.home_label,
            to: description.target_label,
        },
        description.new_session,
        description.home_changed,
    )
    .map_err(invalid)?
    else {
        return Ok(None);
    };
    let event = Event {
        generation,
        from,
        to,
        home: description.home.into(),
        target: prepared.target,
        session_key: prepared.session_key,
        reason_code: prepared.reason_code.into(),
        nonce: prepared.nonce,
        content: prepared.content,
        bot: description.bot.into(),
        created_at: sqlx::query_scalar("SELECT NOW()")
            .fetch_one(&mut **tx)
            .await?,
    };
    let outbox_id = enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
        tx,
        OutboxMessage {
            target: &event.target,
            content: &event.content,
            bot: description.bot,
            source: SOURCE,
            reason_code: Some(&event.reason_code),
            session_key: Some(&event.session_key),
        },
    )
    .await
    .map_err(|e| match e {
        crate::services::message_outbox::OutboxEnqueueError::Database(e) => e,
        other => invalid(format!("{other:?}")),
    })?;
    let changed = sqlx::query("UPDATE intake_session_owners SET routing_notice=$4,routing_notice_outbox_id=$5,routing_notice_delivery_state='pending' WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(generation).bind(sqlx::types::Json(&event))
        .bind(outbox_id).execute(&mut **tx).await?.rows_affected();
    if changed != 1 {
        return Err(invalid(
            "owner history disappeared during notice reservation",
        ));
    }
    Ok(Some(Reservation {
        outbox_id,
        event,
        state: DeliveryState::Pending,
        post_started_at: None,
        message_id: None,
    }))
}

/// Discord honours an enforced nonce only briefly; an older Posting attempt is never reposted.
const NONCE_RETRY_SECONDS: f64 = 120.0;
/// A dead worker's routing claim and a failed POST both come back inside that window.
pub(crate) const RECLAIM_SECONDS: i64 = 45;
pub(crate) const RETRY_SECONDS: i64 = 15;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Post(Box<Event>),
    Sent(String),
    Uncertain,
    Unowned,
}

#[derive(sqlx::FromRow)]
struct Cursor {
    routing_notice: sqlx::types::Json<Event>,
    routing_notice_delivery_state: String,
    routing_notice_message_id: Option<String>,
    fresh: bool,
    matches: bool,
}

/// The claimed outbox row posts only through its owner pointer; Posting is durable before the POST.
pub(crate) async fn begin_post(pool: &sqlx::PgPool, outbox_id: i64) -> Result<Step, sqlx::Error> {
    let owner: Option<(String, String)> = sqlx::query_as(
        "SELECT provider,raw_channel_id FROM intake_session_owners WHERE routing_notice_outbox_id=$1",
    )
    .bind(outbox_id)
    .fetch_optional(pool)
    .await?;
    let Some((provider, channel)) = owner else {
        return Ok(Step::Unowned);
    };
    let mut tx = pool.begin().await?;
    custody::lock(&mut tx, &OwnerIdentity::new(&provider, &channel)).await?;
    let stored: Option<Cursor> = sqlx::query_as("SELECT o.routing_notice,o.routing_notice_delivery_state,o.routing_notice_message_id,COALESCE(o.routing_notice_started_at>NOW()-make_interval(secs=>$2) AND o.routing_notice_started_at<=NOW()+INTERVAL '5 seconds',FALSE) AS fresh,EXISTS(SELECT 1 FROM message_outbox m WHERE m.id=o.routing_notice_outbox_id AND m.source=$3 AND m.target=o.routing_notice->>'target' AND m.session_key=o.routing_notice->>'session_key' AND m.reason_code=o.routing_notice->>'reason_code' AND m.content=o.routing_notice->>'content' AND m.bot=o.routing_notice->>'bot') AS matches FROM intake_session_owners o WHERE o.routing_notice_outbox_id=$1 FOR UPDATE")
        .bind(outbox_id).bind(NONCE_RETRY_SECONDS).bind(SOURCE).fetch_optional(&mut *tx).await?;
    let Some(Cursor {
        routing_notice: event,
        routing_notice_delivery_state: state,
        routing_notice_message_id: message_id,
        fresh,
        matches,
    }) = stored
    else {
        return Ok(Step::Unowned);
    };
    if !matches {
        return Err(invalid("routing notice outbox row differs from its event"));
    }
    let step = match DeliveryState::read(Some(&state))? {
        DeliveryState::Sent => Step::Sent(message_id.unwrap_or_default()),
        DeliveryState::Uncertain => Step::Uncertain,
        DeliveryState::Posting if fresh => Step::Post(Box::new(event.0)),
        DeliveryState::Posting => {
            set_state(&mut tx, outbox_id, "posting", "uncertain").await?;
            Step::Uncertain
        }
        DeliveryState::Pending => {
            set_state(&mut tx, outbox_id, "pending", "posting").await?;
            Step::Post(Box::new(event.0))
        }
    };
    tx.commit().await?;
    Ok(step)
}

async fn set_state(
    tx: &mut Transaction<'_, Postgres>,
    outbox_id: i64,
    from: &str,
    to: &str,
) -> Result<(), sqlx::Error> {
    let changed = sqlx::query("UPDATE intake_session_owners SET routing_notice_delivery_state=$3,routing_notice_started_at=COALESCE(routing_notice_started_at,NOW()) WHERE routing_notice_outbox_id=$1 AND routing_notice_delivery_state=$2")
        .bind(outbox_id).bind(from).bind(to).execute(&mut **tx).await?.rows_affected();
    if changed != 1 {
        return Err(invalid("routing notice cursor changed under its lock"));
    }
    Ok(())
}

/// Only the stored nonce's own POST may settle the cursor, and only with a real receipt.
pub(crate) async fn record_sent(
    pool: &sqlx::PgPool,
    outbox_id: i64,
    nonce: &str,
    message_id: &str,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("UPDATE intake_session_owners SET routing_notice_delivery_state='sent',routing_notice_message_id=$3 WHERE routing_notice_outbox_id=$1 AND routing_notice->>'nonce'=$2 AND routing_notice_delivery_state='posting' AND btrim($3)<>''")
        .bind(outbox_id).bind(nonce).bind(message_id).execute(pool).await?.rows_affected() == 1)
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
