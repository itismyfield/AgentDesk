//! A root recovery still held refuses the owner's front before any claim: the front waits on the
//! grant it held, for its due or for the restart reader retiring that recovery.
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::discord::herdr_terminate::root_replaced::preclaim;
use crate::services::discord::queue_io::input_owner::drive::Front;
use crate::services::turn_orchestrator::RetryWaitCause;

/// Checks the channel's root recovery before `front` is claimed; a held one spends `front`'s
/// grant into its wait and refuses.
pub(super) async fn before_claim(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    front: &Front,
) -> Result<(), String> {
    let mailbox = shared.mailbox(channel);
    let granted = front.clone();
    let grant = mailbox.v2_request(move |slot| {
        Ok(slot
            .as_ref()
            .and_then(|owner| owner.grant_number_of(&granted)))
    });
    let grant = grant.await?;
    let Err(error) = preclaim::before_idle(shared, &shared.provider, channel).await else {
        return Ok(());
    };
    if error == preclaim::HELD
        && let Some(grant) = grant
    {
        let (front, now) = (
            front.clone(),
            chrono::Utc::now().timestamp_millis().max(0) as u64,
        );
        let entered = mailbox.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("V2 not adopted")?;
            let held = RetryWaitCause::RootRecoveryHeld;
            owner.enter_retry_wait_granted((&front, grant), held, now)
        });
        if let Err(wait) = entered.await {
            tracing::warn!(channel_id = channel.get(), %wait, "retry wait not recorded");
        }
    }
    Err(error)
}
