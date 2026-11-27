use std::num::NonZeroU64;
use std::sync::Arc;

use super::{DiscordPort, GatewayPort, PostOutcome, confirm, serenity};
use crate::services::tui_o::writer::agy_delivery::{Evidence, Piece, Post, Transport, hash};
use serenity::{ChannelId, CreateEmbed, CreateEmbedFooter, CreateMessage, GetMessages, Nonce};

#[derive(Clone)]
pub(super) struct Correlation {
    token: String,
    payload_hash: [u8; 32],
}

impl Correlation {
    pub(super) fn attach(&self, message: CreateMessage) -> CreateMessage {
        message
            .embed(CreateEmbed::new().footer(CreateEmbedFooter::new(&self.token)))
            .nonce(Nonce::String(self.token.chars().take(25).collect()))
            .enforce_nonce(true)
    }

    pub(super) fn matches(&self, message: &serenity::Message, channel: u64, bot: u64) -> bool {
        message.channel_id.get() == channel
            && message.author.id.get() == bot
            && correlation(message) == Some(self.token.as_str())
            && hash(&message.content) == self.payload_hash
    }
}

impl Transport for GatewayPort {
    fn bot(&self) -> Option<NonZeroU64> {
        NonZeroU64::new(self.bot_id)
    }

    async fn post(&self, channel: u64, piece: &Piece) -> Post {
        let mut port = Self::new(Arc::clone(&self.http), self.bot_id);
        port.agy_correlation = Some(Correlation {
            token: piece.correlation.clone(),
            payload_hash: piece.payload_hash,
        });
        match DiscordPort::post(&port, channel, piece.payload.clone()).await {
            PostOutcome::Created(message) => {
                NonZeroU64::new(message.id).map_or(Post::Unknown, Post::Created)
            }
            PostOutcome::Refused(status) => {
                Post::NotSent(format!("agy Discord POST refused ({status})"))
            }
            PostOutcome::Withheld(reason) => Post::NotSent(reason),
            PostOutcome::Uncertain(_) => Post::Unknown,
        }
    }

    async fn history_before(
        &self,
        channel: u64,
        before: Option<NonZeroU64>,
    ) -> Result<Vec<Evidence>, String> {
        let mut page = GetMessages::new().limit(confirm::HISTORY_PAGE as u8);
        if let Some(before) = before {
            page = page.before(before);
        }
        let messages = ChannelId::new(channel)
            .messages(&*self.http, page)
            .await
            .map_err(|error| error.to_string())?;
        Ok(messages
            .into_iter()
            .map(|message| {
                let correlation = correlation(&message).map(str::to_owned);
                Evidence {
                    id: NonZeroU64::from(message.id),
                    channel: message.channel_id.get(),
                    bot: message.author.id.get(),
                    content: message.content,
                    correlation,
                }
            })
            .collect())
    }
}

fn correlation(message: &serenity::Message) -> Option<&str> {
    let footer = message.embeds.first()?.footer.as_ref()?;
    Some(&footer.text)
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) use projection::AgyProjection;

// No production caller drives the dormant agy delivery yet; only its tests build the projection.
#[cfg(any(test, feature = "test-support"))]
mod projection {
    use std::sync::Arc;
    use std::time::Instant;

    use super::serenity::ChannelId;
    use crate::services::discord::{self, SharedData, inflight};
    use crate::services::provider::ProviderKind;
    use crate::services::tui_o::writer::agy_delivery::{Effect, Projection, TurnLease};

    /// SharedData as the dormant agy delivery projection; a local wrapper because the trait lives in the runtime crate.
    pub(crate) struct AgyProjection(pub(crate) Arc<SharedData>);

    impl Projection for AgyProjection {
        async fn apply(&self, lease: TurnLease, effect: Effect) -> Result<(), String> {
            let shared = &self.0;
            let channel = ChannelId::from(lease.key.destination);
            let id = lease.key.id.get();
            let nonce = lease.key.nonce.to_string();
            let provider = ProviderKind::Antigravity;
            match effect {
                Effect::ClearInflight => {
                    use inflight::GuardedClearOutcome::*;
                    let Some(row) =
                        inflight::load_inflight_state_read_only_result(&provider, channel.get())?
                    else {
                        return Ok(());
                    };
                    if row.effective_finalizer_turn_id() != id
                        || row.turn_nonce.as_deref() != Some(&nonce)
                    {
                        return Ok(());
                    }
                    let result = inflight::clear_inflight_state_for_captured_episode(
                        &provider,
                        channel.get(),
                        &inflight::InflightTurnIdentity::from_state(&row),
                        Some(&nonce),
                    );
                    match result {
                        Cleared | Missing | UserMsgMismatch => Ok(()),
                        other => Err(format!("agy inflight projection held ({other:?})")),
                    }
                }
                Effect::ReleaseLease => {
                    let cutoff = Instant::now();
                    let Some(mailbox) = shared.mailbox_peek(channel) else {
                        return Ok(());
                    };
                    let snapshot = mailbox.snapshot().await;
                    if snapshot.active_user_message_id.map(|id| id.get()) != Some(id)
                        || snapshot.active_turn_nonce.as_deref() != Some(&nonce)
                        || snapshot.cancel_token.is_none()
                    {
                        return Ok(());
                    }
                    let released = discord::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
                        shared, &provider, channel, lease.key.id.into(), Some(nonce.clone()), cutoff,
                        snapshot.cancel_token,
                    ).await;
                    if released.removed_token.is_none() || released.persistence_error.is_some() {
                        return Err("agy mailbox release did not confirm the turn".into());
                    }
                    discord::saturating_decrement_global_active(shared);
                    let key = discord::turn_finalizer::TurnKey::new(
                        channel,
                        id,
                        shared.restart.current_generation,
                    )
                    .with_episode_nonce(Some(&nonce));
                    shared
                        .turn_finalizer
                        .note_mailbox_released(key, shared.clone());
                    Ok(())
                }
                Effect::WakeQueue => {
                    discord::schedule_deferred_idle_queue_kickoff(
                        shared.clone(),
                        provider,
                        channel,
                        "agy_done",
                    );
                    Ok(())
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::discord;
    use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
    use crate::services::provider::CancelToken;
    use crate::services::tui_o::store::agy_delivery::TurnKey;
    use crate::services::tui_o::writer::agy_delivery::{Effect, Projection, TurnLease};
    use poise::serenity_prelude::{MessageId, UserId};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn agy_delivery_projects_one_release_and_preserves_the_next_episode() {
        with_isolated_runtime_root(|| async {
            let shared = discord::make_shared_data_for_tests();
            let channel = ChannelId::new(827_403_001);
            let token_a = Arc::new(CancelToken::new());
            let token_b = Arc::new(CancelToken::new());
            let msg_a = MessageId::new(827_403_002);
            let msg_b = MessageId::new(827_403_003);
            let user = UserId::new(827_403_004);
            assert!(
                discord::mailbox_try_start_turn(&shared, channel, token_a.clone(), user, msg_a)
                    .await
            );
            discord::increment_global_active(&shared, "agy fixture a");
            let lease = TurnLease {
                revision: NonZeroU64::MIN,
                key: TurnKey {
                    destination: channel.into(),
                    id: msg_a.into(),
                    nonce: token_a.turn_nonce().unwrap().parse().unwrap(),
                },
            };
            AgyProjection(shared.clone())
                .apply(lease, Effect::ReleaseLease)
                .await
                .unwrap();
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
            assert!(
                discord::mailbox_try_start_turn(&shared, channel, token_b.clone(), user, msg_b)
                    .await
            );
            discord::increment_global_active(&shared, "agy fixture b");
            AgyProjection(shared.clone())
                .apply(lease, Effect::ReleaseLease)
                .await
                .unwrap();
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
            let live = shared.mailbox_peek(channel).unwrap().snapshot().await;
            assert_eq!(live.active_user_message_id, Some(msg_b));
            assert!(Arc::ptr_eq(&live.cancel_token.unwrap(), &token_b));
        })
        .await;
    }
}
