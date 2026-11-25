//! Explicit producer intent survives cold launch; the scope observes keys before it unwinds.
use super::*;
use crate::services::discord::turn_finalizer::{TurnKey, native_prepare};
use crate::services::tui_input::human_frame::history::v2::native::{Episode, Intent};

pub(super) struct Producer {
    shared: Shared,
    key: TurnKey,
    actor: Option<std::sync::Arc<CancelToken>>,
    intent: Intent,
    started_before: std::time::Instant,
    #[cfg(test)]
    binding_root: Option<std::path::PathBuf>,
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
            started_before: std::time::Instant::now(),
            #[cfg(test)]
            binding_root: crate::services::tui_prompt_dedupe::binding_events::test_root(),
        }
    }

    fn unwind(&self) {
        let Some(actor) = &self.actor else {
            return;
        };
        if self.key.generation != self.shared.restart.current_generation {
            return;
        }
        tokio::runtime::Handle::current().block_on(async {
            let finish=crate::services::discord::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
                &self.shared,&self.shared.provider,self.key.channel_id,self.key.user_msg_id.into(),
                actor.turn_nonce().map(str::to_owned),self.started_before,Some(actor.clone()),
            ).await;
            if let Some(removed)=finish.removed_token {
                removed.mark_completion_cleanup();
                removed.cancelled.store(true,std::sync::atomic::Ordering::Relaxed);
                crate::services::discord::saturating_decrement_global_active(&self.shared);
                if finish.has_pending {
                    crate::services::discord::schedule_deferred_idle_queue_kickoff(
                        self.shared.clone(),self.shared.provider.clone(),self.key.channel_id,"native_pre_key_unwind",
                    );
                }
            }
        });
    }

    pub(super) fn wrap(self, leaf: effect::Leaf) -> effect::Leaf {
        #[cfg(test)]
        let binding_root = self
            .binding_root
            .clone()
            .or_else(crate::services::tui_prompt_dedupe::binding_events::test_root);
        Box::new(move |frame| {
            #[cfg(test)]
            let _binding_root = native_prepare::BindingRoot::enter(binding_root.as_deref());
            let prepared = (|| {
                let (history, namespace, ticket) =
                    crate::services::tui_input::human_frame::history::v2::native::scoped_ticket()?;
                let handle =
                    tokio::runtime::Handle::current().block_on(native_prepare::Handle::prepare(
                        &self.shared,
                        self.key,
                        self.actor.clone(),
                        history.clone(),
                        namespace.clone(),
                        ticket.clone(),
                    ))?;
                Ok::<_, String>((handle, history, namespace, ticket))
            })();
            let (handle, history, namespace, ticket) = match prepared {
                Ok(handle) => handle,
                Err(error) => {
                    scope::vetoed(&error);
                    self.unwind();
                    return Err(effect::unsent(error));
                }
            };
            let result = match tokio::runtime::Handle::current().block_on(
                native_prepare::pin_source(&self.shared, &namespace, &ticket),
            ) {
                Ok(()) => leaf(frame),
                Err(error) => {
                    scope::vetoed(&error);
                    Err(effect::unsent(error))
                }
            };
            let unkeyed = scope::key_attempted() == Some(false);
            let finished = handle.finish();
            if !unkeyed {
                return finished.and(result);
            }
            // No key left, so the turn unwinds even when its no-key record or source ack failed.
            let released = finished.and_then(|()| {
                tokio::runtime::Handle::current().block_on(native_prepare::release_unkeyed_source(
                    &self.shared,
                    &history,
                    &namespace,
                    &ticket,
                ))
            });
            self.unwind();
            released.and(result)
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

/// A restarted owner re-adopts the channel's V2 queue, settles a head whose window passed,
/// projects history and posts owed notices, then wakes its drive.
pub(super) async fn recover(shared: &Shared, channel: ChannelId) {
    use crate::services::discord::queue_io::input_owner::recovery;
    use crate::services::tui_input::human_frame::history::v2::Registry;
    let persistence = crate::services::discord::queue_dispatch::persistence_context;
    let queue = crate::services::turn_orchestrator::v2_queue;
    let queue = queue(&persistence(shared, &shared.provider, channel), channel);
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let recovered = match (queue, Registry::runtime()) {
        (Ok(queue), Ok(history)) => {
            let at = (shared.provider.as_str(), channel.get(), now);
            let pool = shared.pg_pool.as_ref();
            let namespace = queue.1.clone();
            (recovery::recover(&shared.mailbox(channel), queue, history.clone(), pool, at).await)
                .map(|_| (history, namespace))
        }
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    match recovered {
        Ok((history, namespace)) => {
            // An early ⏳ whose input boot no longer holds never reaches a handoff to settle it.
            crate::services::discord::queue_io::input_owner::early_pending::sweep_boot(
                shared, channel,
            )
            .await;
            release_unkeyed_sources(shared, channel, history, namespace).await;
            claude_handoff::recover(shared.clone(), channel);
            crate::services::discord::commands::effort_restart::resume_owed(shared, channel).await;
            if native_prepare::boot_ready(shared, channel).await {
                wake(shared, channel);
            }
        }
        Err(error) => tracing::warn!(channel_id = channel.get(), %error, "owner recovery held"),
    }
}

/// A cold source pin whose attempt boot settled as no-key gets the ACK its leaf could not give;
/// a pin without that history proof stays owed.
async fn release_unkeyed_sources(
    shared: &Shared,
    channel: ChannelId,
    history: crate::services::tui_input::human_frame::history::v2::Registry,
    namespace: String,
) {
    let mailbox = shared.mailbox(channel);
    let tickets = mailbox.v2_request(|slot| {
        Ok(slot
            .as_ref()
            .map(|owner| owner.source_binding_tickets())
            .unwrap_or_default())
    });
    for ticket in tickets.await.unwrap_or_default() {
        let (shared, history, namespace) = (shared.clone(), history.clone(), namespace.clone());
        let _ = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(native_prepare::release_unkeyed_source(
                &shared, &history, &namespace, &ticket,
            ))
        })
        .await;
    }
}
