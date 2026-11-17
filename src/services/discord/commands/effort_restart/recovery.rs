//! The steps a restart left unfinished, carried on by a restarted server or the owner's next wake.
use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::platform::tmux::PaneLiveness;

/// Carries the channel's unfinished restart on: cancelled when its old TUI still runs, else its end
/// is recorded and its continuation admitted under the stored ticket, so a repeat adds nothing.
pub(in crate::services::discord) async fn resume_owed(
    shared: &Arc<SharedData>,
    channel: ChannelId,
) {
    // A cancel opens the queue front; the drive is woken once the transition guard is released.
    if resolve(shared, channel).await {
        crate::services::discord::router::message_handler::owner_wake(shared, channel);
    }
}

/// One pass over the owed steps; `true` when it opened the queue front, by settling an unconfirmed
/// commit or by cancelling the intent.
async fn resolve(shared: &Arc<SharedData>, channel: ChannelId) -> bool {
    let (intent, released) = super::tmux::owed(shared, channel).await;
    if intent.is_none() {
        return released;
    }
    resolve_held(shared, channel).await || released
}

/// The owed steps proper, re-read once the transition guard is held; `true` when it cancelled.
async fn resolve_held(shared: &Arc<SharedData>, channel: ChannelId) -> bool {
    let Ok(_transition) = shared.acquire_session_transition(channel).await else {
        return false;
    };
    let (intent, released) = super::tmux::owed(shared, channel).await;
    let Some(intent) = intent else {
        return released;
    };
    let provider = &shared.provider;
    if !intent.ready {
        // Without the process it was to end, nothing tells whether that end happened.
        let Some(cut) = &intent.cut else {
            return false;
        };
        let liveness = crate::services::tmux_diagnostics::tmux_session_pane_liveness(&cut.tmux);
        // A live pane still bound to the process the restart named, or to one it could not name.
        let same = || {
            let bound = super::witness::bound(provider, channel.get(), &cut.tmux);
            let now = bound.ok().flatten().map(|bound| bound.nonce().to_string());
            cut.bind_nonce.is_none() || now == cut.bind_nonce
        };
        match liveness {
            PaneLiveness::ProbeError => return false,
            PaneLiveness::Live if same() => {
                // The old TUI never ended: the cut turn still runs and owes nothing.
                return super::tmux::cancel(shared, channel, &intent.operation).await;
            }
            PaneLiveness::Live | PaneLiveness::DeadOrAbsent => {}
        }
        super::super::control::consume_reflected(shared, provider, channel).await;
    }
    let step = super::tmux::advance(shared, provider, channel, &intent.operation).await;
    if let super::tmux::Step::Unproven(error) | super::tmux::Step::Refused(error) = step {
        tracing::warn!(channel_id = channel.get(), %error, "owed effort continuation not admitted");
    }
    false
}

/// Starts [`resume_owed`] off the caller's task; the owner drive calls this each time it runs out
/// of fronts.
pub(in crate::services::discord) fn kick(shared: &Arc<SharedData>, channel: ChannelId) {
    let shared = shared.clone();
    tokio::spawn(async move { resume_owed(&shared, channel).await });
}
