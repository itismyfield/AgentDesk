//! A headless start's host judgement; a prepared Herdr delivery enters owner custody before a
//! later host refusal.
#[cfg(unix)]
use super::*;
use crate::services::provider::ProviderKind;
use crate::services::turn_host::{self, HerdrRefusal};

/// How a headless start meets its channel's host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Standing {
    /// The channel's own key: an external start, or a routine or scheduled one its owner holds.
    Channel,
    /// A routine or scheduled start with a key or pane of its own, or one no owner holds.
    Apart,
}

impl Standing {
    pub(super) fn of(
        metadata: Option<&serde_json::Value>,
        label: Option<&str>,
        owned: bool,
    ) -> Self {
        let has = |key| metadata.is_some_and(|metadata| metadata.get(key).is_some());
        if !has("routine_id") && !has("scheduled_message_id") {
            return Self::Channel;
        }
        let own = label.is_some() || has("scheduled_snapshot_session_label");
        if owned && !own {
            Self::Channel
        } else {
            Self::Apart
        }
    }
}

pub(super) async fn refusal_before_start<F>(
    pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    channel: u64,
    key: impl FnOnce() -> F,
    standing: Standing,
) -> Option<HerdrRefusal>
where
    F: std::future::Future<Output = Option<String>>,
{
    match standing {
        Standing::Channel => {
            turn_host::intake_refusal_before_turn(pool, provider, channel, key).await
        }
        // Such a start keeps its typed refusal on a Herdr channel.
        Standing::Apart => turn_host::refusal_before_turn(pool, provider, channel, key).await,
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
