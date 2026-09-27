//! Human input entry: start a turn when the mailbox is idle, otherwise queue
//! the input on the channel mailbox with the reason it could not start.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, UserId};

use super::HealthRegistry;
use super::runtime_resolve::resolve_direct_meeting_shared;
use crate::services::discord::{SharedData, router};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, SourceMessageQueuedGeneration,
};

pub struct HumanInputRequest {
    pub channel_id: ChannelId,
    pub provider: ProviderKind,
    pub text: String,
    pub author_id: u64,
    pub source: String,
    pub metadata: Option<serde_json::Value>,
    pub channel_name_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputDelivery {
    Started { turn_id: String },
    Queued { turn_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputError {
    AuthorNotAllowed,
    RuntimeUnavailable(String),
    QueueRefused(String),
}

/// Stricter than Discord intake auth: an explicit owner is required and
/// `allow_all_users` is never honored for remote input.
pub(crate) fn author_allowed_for_human_input(
    owner_user_id: Option<u64>,
    allowed_user_ids: &[u64],
    author_id: u64,
) -> bool {
    author_id != 0
        && owner_user_id.is_some()
        && (owner_user_id == Some(author_id) || allowed_user_ids.contains(&author_id))
}

enum StartAttempt {
    Started(String),
    Busy,
    Unavailable(String),
}

/// What holds the mailbox slot when a start was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MailboxHolder {
    Nothing,
    Turn,
    BackgroundTurn,
}

#[async_trait]
trait DeliveryPorts: Send + Sync {
    async fn try_start(&self) -> StartAttempt;
    async fn mailbox_holder(&self) -> MailboxHolder;
    async fn enqueue(&self) -> Result<String, String>;
}

async fn deliver_with_ports<P: DeliveryPorts>(
    ports: &P,
) -> Result<HumanInputDelivery, HumanInputError> {
    match ports.try_start().await {
        StartAttempt::Started(turn_id) => return Ok(HumanInputDelivery::Started { turn_id }),
        StartAttempt::Unavailable(error) => return Err(HumanInputError::RuntimeUnavailable(error)),
        StartAttempt::Busy => {}
    }
    let reason = match ports.mailbox_holder().await {
        MailboxHolder::Turn => "turn_active",
        MailboxHolder::BackgroundTurn => "background_turn",
        // A refused start with an empty slot is a session transition or a turn
        // that just ended; one more start attempt avoids queueing behind nothing.
        MailboxHolder::Nothing => match ports.try_start().await {
            StartAttempt::Started(turn_id) => return Ok(HumanInputDelivery::Started { turn_id }),
            StartAttempt::Unavailable(error) => {
                return Err(HumanInputError::RuntimeUnavailable(error));
            }
            StartAttempt::Busy => "session_transition",
        },
    };
    match ports.enqueue().await {
        Ok(turn_id) => Ok(HumanInputDelivery::Queued {
            turn_id,
            reason: reason.to_string(),
        }),
        Err(refusal) => Err(HumanInputError::QueueRefused(refusal)),
    }
}

struct LivePorts {
    shared: Arc<SharedData>,
    ctx: serenity::Context,
    token: String,
    request: HumanInputRequest,
}

#[async_trait]
impl DeliveryPorts for LivePorts {
    async fn try_start(&self) -> StartAttempt {
        let request = &self.request;
        let result = router::start_reserved_headless_turn_with_owner(
            &self.ctx,
            request.channel_id,
            &request.text,
            &format!("{}:{}", request.source, request.author_id),
            UserId::new(request.author_id),
            &self.shared,
            &self.token,
            Some(request.source.as_str()),
            request.metadata.clone(),
            request.channel_name_hint.clone(),
            None,
            None,
            router::reserve_headless_turn(),
        )
        .await;
        match result {
            Ok(outcome) => StartAttempt::Started(outcome.turn_id),
            Err(router::HeadlessTurnStartError::Conflict(_)) => StartAttempt::Busy,
            Err(router::HeadlessTurnStartError::Internal(error)) => {
                StartAttempt::Unavailable(error)
            }
        }
    }

    async fn mailbox_holder(&self) -> MailboxHolder {
        let snapshot = super::super::mailbox_snapshot(&self.shared, self.request.channel_id).await;
        match snapshot.cancel_token {
            None => MailboxHolder::Nothing,
            Some(_) if snapshot.active_turn_kind.is_background() => MailboxHolder::BackgroundTurn,
            Some(_) => MailboxHolder::Turn,
        }
    }

    async fn enqueue(&self) -> Result<String, String> {
        let reservation = router::reserve_headless_turn();
        let message_id = reservation.user_msg_id();
        let generation = crate::services::discord::runtime_store::process_generation();
        let request = &self.request;
        let intervention = Intervention {
            author_id: UserId::new(request.author_id),
            author_is_bot: false,
            message_id,
            queued_generation: generation,
            source_message_ids: vec![message_id],
            source_message_queued_generations: vec![
                SourceMessageQueuedGeneration::user_instruction(message_id, generation),
            ],
            source_text_segments: Vec::new(),
            text: request.text.clone(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        let outcome = super::super::mailbox_enqueue_intervention(
            &self.shared,
            &request.provider,
            request.channel_id,
            intervention,
        )
        .await;
        if !outcome.enqueued {
            return Err(outcome
                .refusal_reason
                .map(|reason| format!("{reason:?}"))
                .unwrap_or_else(|| "not_enqueued".to_string()));
        }
        Ok(reservation.turn_id(request.channel_id))
    }
}

pub async fn deliver_human_input(
    registry: &HealthRegistry,
    request: HumanInputRequest,
) -> Result<HumanInputDelivery, HumanInputError> {
    let shared = resolve_direct_meeting_shared(registry, request.channel_id, &request.provider)
        .await
        .map_err(HumanInputError::RuntimeUnavailable)?;
    let allowed = {
        let settings = shared.settings.read().await;
        author_allowed_for_human_input(
            settings.owner_user_id,
            &settings.allowed_user_ids,
            request.author_id,
        )
    };
    if !allowed {
        return Err(HumanInputError::AuthorNotAllowed);
    }
    let ctx = shared
        .http
        .cached_serenity_ctx
        .get()
        .cloned()
        .ok_or_else(|| {
            HumanInputError::RuntimeUnavailable("provider runtime is not ready".to_string())
        })?;
    let token = shared
        .http
        .cached_bot_token
        .get()
        .cloned()
        .or_else(|| crate::services::discord::resolve_discord_token_by_hash(&shared.token_hash))
        .ok_or_else(|| {
            HumanInputError::RuntimeUnavailable("provider token unavailable".to_string())
        })?;
    let ports = LivePorts {
        shared,
        ctx,
        token,
        request,
    };
    deliver_with_ports(&ports).await
}

/// Registers a bot runtime bound to `channel_id` with the given auth settings.
#[cfg(test)]
pub(crate) async fn register_bot_auth_for_tests(
    registry: &HealthRegistry,
    provider: &str,
    channel_id: u64,
    owner_user_id: Option<u64>,
    allowed_user_ids: Vec<u64>,
    allow_all_users: bool,
) {
    let shared = crate::services::discord::make_shared_data_for_tests();
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = owner_user_id;
        settings.allowed_user_ids = allowed_user_ids;
        settings.allow_all_users = allow_all_users;
        settings.allowed_channel_ids = vec![channel_id];
    }
    registry.register(provider.to_string(), shared).await;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FakePorts {
        starts: Mutex<VecDeque<StartAttempt>>,
        holder: MailboxHolder,
        enqueue: Result<String, String>,
        enqueues: AtomicUsize,
    }

    fn ports(starts: Vec<StartAttempt>, holder: MailboxHolder) -> FakePorts {
        FakePorts {
            starts: Mutex::new(starts.into()),
            holder,
            enqueue: Ok("discord:7:900".to_string()),
            enqueues: AtomicUsize::new(0),
        }
    }

    #[async_trait]
    impl DeliveryPorts for FakePorts {
        async fn try_start(&self) -> StartAttempt {
            let next = self.starts.lock().unwrap().pop_front();
            next.unwrap_or(StartAttempt::Busy)
        }
        async fn mailbox_holder(&self) -> MailboxHolder {
            self.holder
        }
        async fn enqueue(&self) -> Result<String, String> {
            self.enqueues.fetch_add(1, Ordering::SeqCst);
            self.enqueue.clone()
        }
    }

    /// `<delivery> <turn_id> [reason] enqueue=N` for compact expectations.
    async fn run(fake: &FakePorts) -> String {
        let outcome = match deliver_with_ports(fake).await {
            Ok(HumanInputDelivery::Started { turn_id }) => format!("started {turn_id}"),
            Ok(HumanInputDelivery::Queued { turn_id, reason }) => {
                format!("queued {turn_id} {reason}")
            }
            Err(error) => format!("{error:?}"),
        };
        format!("{outcome} enqueue={}", fake.enqueues.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn each_mailbox_state_reaches_exactly_one_commit_point() {
        use MailboxHolder::{BackgroundTurn, Nothing, Turn};
        let started = || StartAttempt::Started("discord:7:1".into());
        let mut refused = ports(vec![StartAttempt::Busy], Turn);
        refused.enqueue = Err("LastItemDedup".to_string());
        #[rustfmt::skip]
        let cases = [
            (ports(vec![started()], Turn), "started discord:7:1 enqueue=0"),
            (ports(vec![StartAttempt::Busy], Turn), "queued discord:7:900 turn_active enqueue=1"),
            (ports(vec![StartAttempt::Busy], BackgroundTurn), "queued discord:7:900 background_turn enqueue=1"),
            // An empty slot after a refused start gets one more start before queueing.
            (ports(vec![StartAttempt::Busy, started()], Nothing), "started discord:7:1 enqueue=0"),
            (ports(vec![StartAttempt::Busy, StartAttempt::Busy], Nothing), "queued discord:7:900 session_transition enqueue=1"),
            (ports(vec![StartAttempt::Unavailable("no ctx".into())], Turn), "RuntimeUnavailable(\"no ctx\") enqueue=0"),
            (refused, "QueueRefused(\"LastItemDedup\") enqueue=1"),
        ];
        for (fake, expected) in cases {
            assert_eq!(run(&fake).await, expected);
        }
    }

    async fn deliver_as(registry: &HealthRegistry, author_id: u64) -> String {
        let request = HumanInputRequest {
            channel_id: ChannelId::new(6_245_001),
            provider: ProviderKind::Claude,
            text: "status?".to_string(),
            author_id,
            source: "imessage".to_string(),
            metadata: None,
            channel_name_hint: None,
        };
        match deliver_human_input(registry, request).await {
            Err(HumanInputError::RuntimeUnavailable(_)) => "past auth".to_string(),
            other => format!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn only_listed_authors_of_an_owned_bot_pass_even_when_allow_all_is_on() {
        let (open_bot, ownerless) = (HealthRegistry::new(), HealthRegistry::new());
        register_bot_auth_for_tests(&open_bot, "claude", 6_245_001, Some(100), vec![200], true)
            .await;
        register_bot_auth_for_tests(&ownerless, "claude", 6_245_001, None, vec![200], true).await;
        // Allowed authors stop only at the missing gateway context.
        #[rustfmt::skip]
        let cases = [(&open_bot, 300, "Err(AuthorNotAllowed)"), (&ownerless, 200, "Err(AuthorNotAllowed)"),
            (&open_bot, 100, "past auth"), (&open_bot, 200, "past auth")];
        for (registry, author, expected) in cases {
            assert_eq!(
                deliver_as(registry, author).await,
                expected,
                "author {author}"
            );
        }
    }
}
