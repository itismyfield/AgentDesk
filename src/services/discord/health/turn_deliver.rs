//! Human input entry: start a turn when the mailbox is idle, otherwise queue
//! the input on the channel mailbox with the reason it could not start.

mod inject;
#[cfg(all(test, unix))]
pub(crate) mod inject_tests;

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
use inject::InjectMode;
pub(crate) use inject::{
    InjectAttempt, Origin as InjectOrigin, SOURCE_OWNED, attempt as inject_human_input,
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
    Started {
        turn_id: String,
    },
    /// `inject_veto` names why a busy turn could not take the input; `None` when none was asked.
    Queued {
        turn_id: String,
        reason: String,
        inject_veto: Option<String>,
    },
    /// The busy turn's transcript recorded the input; `turn_id` is null unless the turn's durable
    /// row names a Discord message.
    Injected {
        turn_id: Option<String>,
    },
    /// A paste was attempted but not confirmed; nothing was queued.
    Unconfirmed {
        turn_id: Option<String>,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HumanInputError {
    AuthorNotAllowed,
    RuntimeUnavailable(String),
    QueueRefused(String),
    InvalidTarget(String),
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
    InvalidTarget(String),
}

/// What holds the mailbox slot when a start was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MailboxHolder {
    Nothing,
    Turn,
    BackgroundTurn,
}

/// Queue reason when a turn that never claimed the mailbox holds the channel.
pub const EXTERNAL_TURN_ACTIVE: &str = "external_turn_active";

/// A TUI-direct, adopted or monitor turn owns the channel through its durable row alone; a
/// headless start there could not create its own row and would lose the input.
pub fn external_turn_holds_channel(provider: &ProviderKind, channel_id: u64) -> bool {
    crate::services::discord::inflight::load_inflight_state_read_only(provider, channel_id)
        .is_some_and(|row| {
            row.turn_source != crate::services::discord::inflight::TurnSource::Managed
        })
}

#[async_trait]
trait DeliveryPorts: Send + Sync {
    async fn external_turn_active(&self) -> bool;
    async fn try_start(&self) -> StartAttempt;
    async fn mailbox_holder(&self) -> MailboxHolder;
    async fn enqueue(&self) -> Result<String, String>;
    fn inject_mode(&self) -> InjectMode;
    async fn try_inject(&self) -> InjectAttempt;
}

async fn deliver_with_ports<P: DeliveryPorts>(
    ports: &P,
) -> Result<HumanInputDelivery, HumanInputError> {
    // A busy TUI pane takes the input whoever holds it; a veto keeps the start-or-queue below.
    let mut inject_veto = None;
    if ports.inject_mode() != InjectMode::Off {
        match ports.try_inject().await {
            InjectAttempt::NotSent(veto) => inject_veto = Some(veto.to_string()),
            InjectAttempt::Injected { turn_id } => {
                return Ok(HumanInputDelivery::Injected { turn_id });
            }
            InjectAttempt::Unconfirmed { turn_id, detail } => {
                let detail = detail.to_string();
                return Ok(HumanInputDelivery::Unconfirmed { turn_id, detail });
            }
            InjectAttempt::OwnerFailed { turn_id } => {
                let detail = "executor_failed".to_string();
                return Ok(HumanInputDelivery::Unconfirmed { turn_id, detail });
            }
            // Ahead of anything sent after the reservation; only a written queue front is Queued.
            InjectAttempt::HandedBack { turn_id, veto } => {
                let (reason, inject_veto) = ("handed_back".to_string(), Some(veto.to_string()));
                return Ok(HumanInputDelivery::Queued {
                    turn_id,
                    reason,
                    inject_veto,
                });
            }
            InjectAttempt::HandbackFailed(reason) => {
                return Err(HumanInputError::QueueRefused(reason.to_string()));
            }
        }
    }
    if !ports.external_turn_active().await {
        match ports.try_start().await {
            StartAttempt::Started(turn_id) => return Ok(HumanInputDelivery::Started { turn_id }),
            StartAttempt::Unavailable(error) => {
                return Err(HumanInputError::RuntimeUnavailable(error));
            }
            StartAttempt::InvalidTarget(error) => {
                return Err(HumanInputError::InvalidTarget(error));
            }
            StartAttempt::Busy => {}
        }
    }
    let reason = match ports.mailbox_holder().await {
        MailboxHolder::Turn => "turn_active",
        MailboxHolder::BackgroundTurn => "background_turn",
        MailboxHolder::Nothing if ports.external_turn_active().await => EXTERNAL_TURN_ACTIVE,
        // A refused start with an empty slot is a session transition or a turn
        // that just ended; one more start attempt avoids queueing behind nothing.
        MailboxHolder::Nothing => match ports.try_start().await {
            StartAttempt::Started(turn_id) => return Ok(HumanInputDelivery::Started { turn_id }),
            StartAttempt::Unavailable(error) => {
                return Err(HumanInputError::RuntimeUnavailable(error));
            }
            StartAttempt::InvalidTarget(error) => {
                return Err(HumanInputError::InvalidTarget(error));
            }
            StartAttempt::Busy => "session_transition",
        },
    };
    match ports.enqueue().await {
        Ok(turn_id) => Ok(HumanInputDelivery::Queued {
            turn_id,
            reason: reason.to_string(),
            inject_veto,
        }),
        Err(refusal) => Err(HumanInputError::QueueRefused(refusal)),
    }
}

struct LivePorts {
    shared: Arc<SharedData>,
    /// Gateway context and bot token; only a start needs them, a queued delivery does not.
    runtime: Result<(serenity::Context, String), String>,
    request: HumanInputRequest,
}

#[async_trait]
impl DeliveryPorts for LivePorts {
    async fn external_turn_active(&self) -> bool {
        external_turn_holds_channel(&self.request.provider, self.request.channel_id.get())
    }

    async fn try_start(&self) -> StartAttempt {
        // With injection on, human input claims behind input queued or reserved before it.
        let reservation = match self.inject_mode() {
            InjectMode::Off => router::reserve_headless_turn(),
            InjectMode::External | InjectMode::All => {
                router::reserve_headless_turn().behind_queue()
            }
        };
        #[cfg(test)]
        if let Some(attempt) = inject::test_hook::start(&self.shared, &self.request, &reservation) {
            return attempt.await;
        }
        let (ctx, token) = match &self.runtime {
            Ok(runtime) => runtime,
            Err(error) => return StartAttempt::Unavailable(error.clone()),
        };
        let request = &self.request;
        let result = router::start_reserved_headless_turn_with_owner(
            ctx,
            request.channel_id,
            &request.text,
            &format!("{}:{}", request.source, request.author_id),
            UserId::new(request.author_id),
            &self.shared,
            token,
            Some(request.source.as_str()),
            request.metadata.clone(),
            request.channel_name_hint.clone(),
            None,
            None,
            reservation,
        )
        .await;
        match result {
            Ok(outcome) => StartAttempt::Started(outcome.turn_id),
            Err(router::HeadlessTurnStartError::Conflict(_)) => StartAttempt::Busy,
            Err(router::HeadlessTurnStartError::InvalidTarget(error)) => {
                StartAttempt::InvalidTarget(error)
            }
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
        let request = &self.request;
        let (intervention, turn_id) = request.queue_entry();
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
        Ok(turn_id)
    }

    fn inject_mode(&self) -> InjectMode {
        inject::mode(self.request.channel_id.get())
    }

    async fn try_inject(&self) -> InjectAttempt {
        inject::attempt(&self.shared, &self.request, inject::Origin::External).await
    }
}

impl HumanInputRequest {
    /// The queue entry this input becomes, under a fresh headless message id, and its turn id.
    fn queue_entry(&self) -> (Intervention, String) {
        let reservation = router::reserve_headless_turn();
        let message_id = reservation.user_msg_id();
        let generation = crate::services::discord::runtime_store::process_generation();
        let intervention = Intervention {
            author_id: UserId::new(self.author_id),
            author_is_bot: false,
            message_id,
            queued_generation: generation,
            source_message_ids: vec![message_id],
            source_message_queued_generations: vec![
                SourceMessageQueuedGeneration::user_instruction(message_id, generation),
            ],
            source_text_segments: Vec::new(),
            text: self.text.clone(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        (intervention, reservation.turn_id(self.channel_id))
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
    let runtime = match shared.http.cached_serenity_ctx.get().cloned() {
        None => Err("provider runtime is not ready".to_string()),
        Some(ctx) => shared
            .http
            .cached_bot_token
            .get()
            .cloned()
            .or_else(|| crate::services::discord::resolve_discord_token_by_hash(&shared.token_hash))
            .map(|token| (ctx, token))
            .ok_or_else(|| "provider token unavailable".to_string()),
    };
    let ports = LivePorts {
        shared,
        runtime,
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

/// Persists the durable row of a TUI-direct turn that never claimed the mailbox.
#[cfg(test)]
pub(crate) fn seed_external_turn_row_for_tests(provider: &ProviderKind, channel_id: u64) {
    use crate::services::discord::inflight::{InflightTurnState, TurnSource};
    let mut row = InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        0,
        0,
        0,
        "subagent report".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_source = TurnSource::ExternalInput;
    crate::services::discord::inflight::save_inflight_state_create_new(&row)
        .expect("external turn row");
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FakePorts {
        externals: Mutex<VecDeque<bool>>,
        starts: Mutex<VecDeque<StartAttempt>>,
        start_calls: AtomicUsize,
        holder: MailboxHolder,
        enqueue: Result<String, String>,
        enqueues: AtomicUsize,
        mode: InjectMode,
        inject: InjectAttempt,
        injects: AtomicUsize,
    }

    fn ports(starts: Vec<StartAttempt>, holder: MailboxHolder) -> FakePorts {
        FakePorts {
            externals: Mutex::new(VecDeque::new()),
            starts: Mutex::new(starts.into()),
            start_calls: AtomicUsize::new(0),
            holder,
            enqueue: Ok("discord:7:900".to_string()),
            enqueues: AtomicUsize::new(0),
            mode: InjectMode::Off,
            inject: InjectAttempt::Injected { turn_id: None },
            injects: AtomicUsize::new(0),
        }
    }

    #[async_trait]
    impl DeliveryPorts for FakePorts {
        async fn external_turn_active(&self) -> bool {
            self.externals.lock().unwrap().pop_front().unwrap_or(false)
        }
        async fn try_start(&self) -> StartAttempt {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
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
        fn inject_mode(&self) -> InjectMode {
            self.mode
        }
        async fn try_inject(&self) -> InjectAttempt {
            self.injects.fetch_add(1, Ordering::SeqCst);
            self.inject.clone()
        }
    }

    /// `<delivery> <turn_id> [reason] [veto=..] enqueue=N` for compact expectations.
    async fn run(fake: &FakePorts) -> String {
        let outcome = match deliver_with_ports(fake).await {
            Ok(HumanInputDelivery::Started { turn_id }) => format!("started {turn_id}"),
            Ok(HumanInputDelivery::Queued {
                turn_id,
                reason,
                inject_veto,
            }) => {
                let veto = inject_veto.map(|veto| format!(" veto={veto}"));
                format!("queued {turn_id} {reason}{}", veto.unwrap_or_default())
            }
            Ok(HumanInputDelivery::Injected { turn_id }) => format!("injected {turn_id:?}"),
            Ok(HumanInputDelivery::Unconfirmed { turn_id, detail }) => {
                format!("unconfirmed {turn_id:?} {detail}")
            }
            Err(error) => format!("{error:?}"),
        };
        format!("{outcome} enqueue={}", fake.enqueues.load(Ordering::SeqCst))
    }

    /// PR1's mailbox table: each state reaches exactly one commit point.
    fn mailbox_cases() -> Vec<(FakePorts, &'static str)> {
        use MailboxHolder::{BackgroundTurn, Nothing, Turn};
        let started = || StartAttempt::Started("discord:7:1".into());
        let mut refused = ports(vec![StartAttempt::Busy], Turn);
        refused.enqueue = Err("LastItemDedup".to_string());
        #[rustfmt::skip]
        let cases = vec![
            (ports(vec![started()], Turn), "started discord:7:1 enqueue=0"),
            (ports(vec![StartAttempt::Busy], Turn), "queued discord:7:900 turn_active enqueue=1"),
            (ports(vec![StartAttempt::Busy], BackgroundTurn), "queued discord:7:900 background_turn enqueue=1"),
            // An empty slot after a refused start gets one more start before queueing.
            (ports(vec![StartAttempt::Busy, started()], Nothing), "started discord:7:1 enqueue=0"),
            (ports(vec![StartAttempt::Busy, StartAttempt::Busy], Nothing), "queued discord:7:900 session_transition enqueue=1"),
            (ports(vec![StartAttempt::Unavailable("no ctx".into())], Turn), "RuntimeUnavailable(\"no ctx\") enqueue=0"),
            (ports(vec![StartAttempt::InvalidTarget("provider mismatch".into())], Turn), "InvalidTarget(\"provider mismatch\") enqueue=0"),
            (ports(vec![StartAttempt::Busy, StartAttempt::InvalidTarget("provider mismatch".into())], Nothing), "InvalidTarget(\"provider mismatch\") enqueue=0"),
            (refused, "QueueRefused(\"LastItemDedup\") enqueue=1"),
        ];
        cases
    }

    /// PR1's external-row table, with the start count.
    fn external_cases() -> Vec<(FakePorts, &'static str)> {
        use MailboxHolder::{BackgroundTurn, Nothing, Turn};
        let started = || StartAttempt::Started("discord:7:1".into());
        let external = |answers: Vec<bool>, starts: Vec<StartAttempt>, holder| {
            let fake = ports(starts, holder);
            *fake.externals.lock().unwrap() = answers.into();
            fake
        };
        #[rustfmt::skip]
        let cases = vec![
            // The start a free mailbox would grant is never attempted.
            (external(vec![true, true], vec![started()], Nothing), "queued discord:7:900 external_turn_active enqueue=1 starts=0"),
            (external(vec![true], vec![started()], Turn), "queued discord:7:900 turn_active enqueue=1 starts=0"),
            (external(vec![true], vec![started()], BackgroundTurn), "queued discord:7:900 background_turn enqueue=1 starts=0"),
            // A row that appears after a refused start labels the queue and stops the retry.
            (external(vec![false, true], vec![StartAttempt::Busy, started()], Nothing), "queued discord:7:900 external_turn_active enqueue=1 starts=1"),
        ];
        cases
    }

    async fn run_counting_starts(fake: &FakePorts) -> String {
        let outcome = run(fake).await;
        format!(
            "{outcome} starts={}",
            fake.start_calls.load(Ordering::SeqCst)
        )
    }

    #[tokio::test]
    async fn each_mailbox_state_reaches_exactly_one_commit_point() {
        for (fake, expected) in mailbox_cases() {
            assert_eq!(run(&fake).await, expected);
        }
    }

    #[tokio::test]
    async fn an_external_turn_row_queues_without_a_start_and_keeps_mailbox_reasons() {
        for (fake, expected) in external_cases() {
            assert_eq!(run_counting_starts(&fake).await, expected);
        }
    }

    /// Off answers exactly as PR1 and never asks the pane, even with an injection on offer.
    #[tokio::test]
    async fn the_switch_off_keeps_every_pr1_answer_and_never_asks_for_an_injection() {
        let mut cases: Vec<_> = mailbox_cases()
            .into_iter()
            .map(|(fake, expected)| (fake, expected.to_string(), false))
            .collect();
        cases.extend(
            external_cases()
                .into_iter()
                .map(|(fake, expected)| (fake, expected.to_string(), true)),
        );
        for (fake, expected, starts) in cases {
            assert_eq!(fake.mode, InjectMode::Off);
            let observed = if starts {
                run_counting_starts(&fake).await
            } else {
                run(&fake).await
            };
            let asked = fake.injects.load(Ordering::SeqCst);
            assert_eq!((observed, asked), (expected, 0));
        }
    }

    /// On, every delivery asks once before any start; a veto keeps PR1's answer with the veto named,
    /// a written handback is the only Queued answer after a reservation, and a failed one refuses.
    #[tokio::test]
    async fn the_switch_on_asks_once_before_any_start_whoever_holds_the_channel() {
        let answers = [
            InjectAttempt::NotSent("not_busy"),
            InjectAttempt::Injected { turn_id: None },
            InjectAttempt::Unconfirmed {
                turn_id: Some("discord:7:5".into()),
                detail: "not_observed",
            },
            InjectAttempt::HandedBack {
                turn_id: "discord:7:42".into(),
                veto: "draft",
            },
            InjectAttempt::HandbackFailed("handback_persistence"),
            InjectAttempt::HandbackFailed("handback_unknown"),
            InjectAttempt::OwnerFailed {
                turn_id: Some("discord:7:6".into()),
            },
        ];
        let mut observed = Vec::new();
        for mode in [InjectMode::External, InjectMode::All] {
            for answer in &answers {
                for (mut fake, _) in mailbox_cases().into_iter().chain(external_cases()) {
                    (fake.mode, fake.inject) = (mode, answer.clone());
                    let outcome = run_counting_starts(&fake).await;
                    let asked = fake.injects.load(Ordering::SeqCst);
                    observed.push(format!("{outcome} asked={asked}"));
                }
            }
        }
        observed.sort();
        observed.dedup();
        #[rustfmt::skip]
        let expected = [
            "InvalidTarget(\"provider mismatch\") enqueue=0 starts=1 asked=1",
            "InvalidTarget(\"provider mismatch\") enqueue=0 starts=2 asked=1",
            "QueueRefused(\"LastItemDedup\") enqueue=1 starts=1 asked=1",
            "QueueRefused(\"handback_persistence\") enqueue=0 starts=0 asked=1",
            "QueueRefused(\"handback_unknown\") enqueue=0 starts=0 asked=1",
            "RuntimeUnavailable(\"no ctx\") enqueue=0 starts=1 asked=1",
            "injected None enqueue=0 starts=0 asked=1",
            "queued discord:7:42 handed_back veto=draft enqueue=0 starts=0 asked=1",
            "queued discord:7:900 background_turn veto=not_busy enqueue=1 starts=0 asked=1",
            "queued discord:7:900 background_turn veto=not_busy enqueue=1 starts=1 asked=1",
            "queued discord:7:900 external_turn_active veto=not_busy enqueue=1 starts=0 asked=1",
            "queued discord:7:900 external_turn_active veto=not_busy enqueue=1 starts=1 asked=1",
            "queued discord:7:900 session_transition veto=not_busy enqueue=1 starts=2 asked=1",
            "queued discord:7:900 turn_active veto=not_busy enqueue=1 starts=0 asked=1",
            "queued discord:7:900 turn_active veto=not_busy enqueue=1 starts=1 asked=1",
            "started discord:7:1 enqueue=0 starts=1 asked=1",
            "started discord:7:1 enqueue=0 starts=2 asked=1",
            "unconfirmed Some(\"discord:7:5\") not_observed enqueue=0 starts=0 asked=1",
            "unconfirmed Some(\"discord:7:6\") executor_failed enqueue=0 starts=0 asked=1",
        ];
        assert_eq!(observed, expected);
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
