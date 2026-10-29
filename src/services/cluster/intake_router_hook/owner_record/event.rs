//! Historical routing notices keep their original outbox pointer after any later owner change.
use super::{OwnerIdentity, custody, fallback::notice};
use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx,
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

pub(crate) const SOURCE: &str = "routing_transition";
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

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
