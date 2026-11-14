//! Explicit producer intent survives cold launch; the scope observes keys before it unwinds.
use super::*;
use crate::services::discord::turn_finalizer::{TurnKey, native_prepare};
use crate::services::tui_input::human_frame::history::v2::native::{Episode, Intent};

pub(super) struct Producer {
    shared: Shared,
    key: TurnKey,
    actor: Option<std::sync::Arc<CancelToken>>,
    intent: Intent,
}

impl Producer {
    pub(super) fn at(
        shared: Shared,
        mut key: TurnKey,
        actor: Option<std::sync::Arc<CancelToken>>,
    ) -> Self {
        let intent = match &actor {
            Some(actor) => {
                let nonce = actor.turn_nonce().unwrap_or_default().to_owned();
                key = key.with_episode_nonce(Some(&nonce));
                Intent::Primary(Episode {
                    user_message_id: key.user_msg_id,
                    turn_nonce: nonce,
                    generation: key.generation,
                    extra: Default::default(),
                })
            }
            None => Intent::Follower,
        };
        Self {
            shared,
            key,
            actor,
            intent,
        }
    }

    pub(super) fn wrap(self, leaf: effect::Leaf) -> effect::Leaf {
        Box::new(move |frame| {
            let (history, namespace, ticket) =
                crate::services::tui_input::human_frame::history::v2::native::scoped_ticket()?;
            let handle = tokio::runtime::Handle::current()
                .block_on(native_prepare::Handle::prepare(
                    &self.shared,
                    self.key,
                    self.actor,
                    history,
                    namespace,
                    ticket,
                ))
                .map_err(|error| {
                    scope::vetoed(&error);
                    effect::unsent(error)
                })?;
            let result = leaf(frame);
            handle.finish()?;
            result
        })
    }
}

pub(super) fn for_front(
    turn: &StreamingTurn<'_>,
    claim: &FrontClaim,
    primary: bool,
) -> Option<Producer> {
    if !native_prepare::enabled()
        || *turn.provider != ProviderKind::Claude
        || !crate::services::tui_o::writer::host::channel_accepts(turn.channel_id)
    {
        return None;
    }
    let shared = turn.shared?.clone();
    let key = TurnKey::new(
        ChannelId::new(turn.channel_id),
        claim.front().1.message_id,
        shared.restart.current_generation,
    );
    Some(Producer::at(
        shared,
        key,
        primary.then(|| turn.cancel.clone()),
    ))
}

pub(super) fn submit(
    (mailbox, clock): (&ChannelMailboxHandle, Clock),
    front: Option<FrontClaim>,
    prepared: PreparedPayload,
    leaf: effect::Leaf,
    native: Option<Producer>,
) -> Result<(), String> {
    let completion = match native {
        Some(native) => {
            let intent = native.intent.clone();
            tokio::runtime::Handle::current().block_on(pump::submit_native_front(
                mailbox,
                front,
                prepared,
                intent,
                clock,
                native.wrap(leaf),
            ))?
        }
        None => tokio::runtime::Handle::current()
            .block_on(pump::submit_front(mailbox, front, prepared, clock, leaf))?,
    };
    match completion {
        Completion::Submitted => Ok(()),
        other => Err(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests;
