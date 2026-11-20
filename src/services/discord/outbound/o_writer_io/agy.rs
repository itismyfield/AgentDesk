use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Instant;

use super::{GatewayPort, confirm, http_status, refusal, serenity};
use crate::services::discord::{self, SharedData, inflight};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::writer::agy_delivery::{
    Effect, Evidence, Piece, Post, Projection, Transport, TurnLease, hash,
};
use serenity::{ChannelId, CreateEmbed, CreateEmbedFooter, CreateMessage, GetMessages, Nonce};

impl Transport for GatewayPort {
    fn bot(&self) -> NonZeroU64 {
        NonZeroU64::new(self.bot_id).expect("gateway port has a Discord bot id")
    }

    async fn post(&self, channel: u64, piece: &Piece) -> Post {
        let request = CreateMessage::new()
            .content(&piece.payload)
            .embed(CreateEmbed::new().footer(CreateEmbedFooter::new(&piece.correlation)))
            .nonce(Nonce::String(piece.correlation.chars().take(25).collect()))
            .enforce_nonce(true)
            .allowed_mentions(crate::services::discord::http::relay_allowed_mentions());
        match ChannelId::new(channel)
            .send_message(&*self.http, request)
            .await
        {
            Ok(message)
                if message.channel_id.get() == channel
                    && message.author.id.get() == self.bot_id
                    && correlation(&message) == Some(piece.correlation.as_str())
                    && hash(&message.content) == piece.payload_hash =>
            {
                Post::Created(NonZeroU64::from(message.id))
            }
            Err(error) if refusal(http_status(&error)).is_some() => Post::NotSent(format!(
                "agy Discord POST refused ({:?})",
                http_status(&error)
            )),
            _ => Post::Unknown,
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

impl Projection for Arc<SharedData> {
    async fn apply(&self, lease: TurnLease, effect: Effect) -> Result<(), String> {
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
                let Some(mailbox) = self.mailbox_peek(channel) else {
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
                    self, &provider, channel, lease.key.id.into(), Some(nonce.clone()), cutoff,
                    snapshot.cancel_token,
                ).await;
                if released.removed_token.is_none() || released.persistence_error.is_some() {
                    return Err("agy mailbox release did not confirm the turn".into());
                }
                discord::saturating_decrement_global_active(self);
                let key = discord::turn_finalizer::TurnKey::new(
                    channel,
                    id,
                    self.restart.current_generation,
                )
                .with_episode_nonce(Some(&nonce));
                self.turn_finalizer.note_mailbox_released(key, self.clone());
                Ok(())
            }
            Effect::WakeQueue => {
                discord::schedule_deferred_idle_queue_kickoff(
                    self.clone(),
                    provider,
                    channel,
                    "agy_done",
                );
                Ok(())
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
    use crate::services::provider::CancelToken;
    use crate::services::tui_o::writer::agy_delivery::TurnKey;
    use serenity::{MessageId, UserId};
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
            shared.apply(lease, Effect::ReleaseLease).await.unwrap();
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
            assert!(
                discord::mailbox_try_start_turn(&shared, channel, token_b.clone(), user, msg_b)
                    .await
            );
            discord::increment_global_active(&shared, "agy fixture b");
            shared.apply(lease, Effect::ReleaseLease).await.unwrap();
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
            let live = shared.mailbox_peek(channel).unwrap().snapshot().await;
            assert_eq!(live.active_user_message_id, Some(msg_b));
            assert!(Arc::ptr_eq(&live.cancel_token.unwrap(), &token_b));
        })
        .await;
    }
}
