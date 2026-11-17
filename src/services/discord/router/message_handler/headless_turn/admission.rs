//! Prepared Herdr delivery enters owner custody before a later host refusal.
#[cfg(unix)]
use super::*;
use crate::services::provider::ProviderKind;
use crate::services::turn_host::{self, HerdrRefusal};

pub(super) async fn refusal_before_start<F>(
    pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    channel: u64,
    key: impl FnOnce() -> F,
    metadata: Option<&serde_json::Value>,
) -> Option<HerdrRefusal>
where
    F: std::future::Future<Output = Option<String>>,
{
    // Routine and scheduled starts retain their existing lifecycle admission.
    let internal = metadata.is_some_and(|metadata| {
        metadata.get("routine_id").is_some() || metadata.get("scheduled_message_id").is_some()
    });
    if internal {
        turn_host::refusal_before_turn(pool, provider, channel, key).await
    } else {
        turn_host::intake_refusal_before_turn(pool, provider, channel, key).await
    }
}

#[cfg(unix)]
impl HeadlessTurnReservation {
    pub(in crate::services::discord) async fn start_prepared_human(
        self,
        (ctx, shared, token): (&serenity::Context, &Arc<SharedData>, &str),
        request: &crate::services::discord::health::HumanInputRequest,
        metadata: Option<serde_json::Value>,
    ) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
        let name = format!("{}:{}", request.source, request.author_id);
        Box::pin(super::start_reserved_headless_turn_admitted(
            ctx,
            request.channel_id,
            &request.text,
            &name,
            UserId::new(request.author_id),
            shared,
            token,
            Some(&request.source),
            metadata,
            request.channel_name_hint.clone(),
            None,
            None,
            self,
            None,
            true,
        ))
        .await
    }
}

/// A start whose target the owner takes is admitted as an original with its resolved provenance;
/// a routine or scheduled start already met its own owner seam and keeps the legacy path here.
#[cfg(unix)]
pub(super) async fn owner_start(
    (shared, channel, reservation): (&Arc<SharedData>, ChannelId, &HeadlessTurnReservation),
    prompt: &str,
    owner: (u64, &str),
    (source, metadata): (Option<&str>, Option<&serde_json::Value>),
    (channel_hint, label, dm): (Option<&str>, Option<&str>, Option<bool>),
) -> Option<Result<HeadlessTurnStartOutcome, HeadlessTurnStartError>> {
    use crate::services::discord::queue_io::input_ingress::Intake;
    use crate::services::discord::queue_io::input_owner::start::HeadlessProvenance;
    let seamed = |key| metadata.is_some_and(|metadata| metadata.get(key).is_some());
    if seamed("routine_id") || seamed("scheduled_message_id") {
        return None;
    }
    let turn_id = reservation.turn_id(channel);
    let metadata = metadata.cloned();
    let provenance = HeadlessProvenance {
        owner,
        source,
        metadata,
        channel_hint,
        label,
        dm,
    };
    let admit = super::super::provider_dispatch::owner_admit_start;
    let intake = admit(
        shared,
        channel,
        (&turn_id, prompt),
        provenance.json(&turn_id),
    )
    .await?;
    Some(match intake {
        Intake::Accepted => Ok(HeadlessTurnStartOutcome::accepted(turn_id)),
        Intake::Pending(pending) => Err(HeadlessTurnStartError::Conflict(pending.to_string())),
        Intake::Rejected(reason) => Err(HeadlessTurnStartError::Conflict(reason)),
    })
}
