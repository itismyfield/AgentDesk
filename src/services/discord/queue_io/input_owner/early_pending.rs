//! #8398: an idle owner puts ⏳ on a person's input as soon as it accepts it (RED stub).

use std::future::Future;
use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId};

use crate::services::discord::turn_view_reconciler::TurnViewTarget;
use crate::services::discord::{Intervention, SharedData};

pub(crate) fn eligible(raw: &Intervention) -> bool {
    let _ = raw;
    false
}

pub(crate) async fn note(shared: &SharedData, channel: ChannelId, message: MessageId) -> bool {
    let _ = (
        shared,
        TurnViewTarget::intake_user_message(channel, message),
    );
    false
}

pub(crate) async fn after_handoff<F>(
    runtime: Option<Arc<SharedData>>,
    channel: u64,
    message: u64,
    handoff: F,
) -> Result<(), String>
where
    F: Future<Output = Result<(), String>>,
{
    let _ = (runtime, channel, message);
    handoff.await
}

#[cfg(all(test, unix))]
#[path = "early_pending_tests.rs"]
mod tests;
