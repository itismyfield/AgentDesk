//! Staged input custody for the first Claude migration; production has no capability yet.
use super::message_handler::{IntakeDeps, IntakeRequest};
use crate::db::intake_outbox::migration_wait::WaitIntent;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{Intervention, QueuePersistenceContext};
use poise::serenity_prelude as serenity;
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub(super) enum WaitReason {
    OutputAdoption,
}

#[derive(Clone, Debug)]
pub(crate) struct Waiting {
    pub(super) original: IntakeRequest,
    pub(super) intent: Arc<WaitIntent>,
    pub(super) reason: WaitReason,
}

pub(super) struct Capability {
    waiting: Waiting,
    pub(super) enabled: Arc<AtomicBool>,
    pub(super) observations: Arc<AtomicUsize>,
    pub(super) custody_attempts: Arc<AtomicUsize>,
    pub(super) retained: Arc<AtomicBool>,
    pub(super) original_observed: Arc<std::sync::Mutex<Option<Intervention>>>,
    pub(super) persistence_failures: Arc<AtomicUsize>,
    pub(super) persistence_successes: Arc<AtomicUsize>,
    pub(super) retry_after_failure: Option<Arc<tokio::sync::Notify>>,
    pub(super) original_created_at: Option<std::time::Instant>,
    pub(super) witnesses: Arc<std::sync::Mutex<Vec<Arc<LocalCustodyWitness>>>>,
}

impl Capability {
    pub(super) fn new(original: IntakeRequest, intent: Arc<WaitIntent>) -> Self {
        Self {
            waiting: Waiting {
                original,
                intent,
                reason: WaitReason::OutputAdoption,
            },
            enabled: Arc::new(AtomicBool::new(true)),
            observations: Arc::new(AtomicUsize::new(0)),
            custody_attempts: Arc::new(AtomicUsize::new(0)),
            retained: Arc::new(AtomicBool::new(false)),
            original_observed: Arc::new(std::sync::Mutex::new(None)),
            persistence_failures: Arc::new(AtomicUsize::new(0)),
            persistence_successes: Arc::new(AtomicUsize::new(0)),
            retry_after_failure: None,
            original_created_at: None,
            witnesses: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

tokio::task_local! { static CAPABILITY: Capability; }

pub(super) async fn scope<T>(
    capability: Capability,
    future: impl std::future::Future<Output = T>,
) -> T {
    CAPABILITY.scope(capability, future).await
}

pub(super) fn waiting(provider: &ProviderKind, request: &IntakeRequest) -> Option<Waiting> {
    if *provider != ProviderKind::Claude {
        return None;
    }
    CAPABILITY
        .try_with(|capability| {
            if !capability.enabled.load(Ordering::SeqCst)
                || capability.waiting.original.channel_id != request.channel_id
            {
                return None;
            }
            capability.observations.fetch_add(1, Ordering::SeqCst);
            Some(capability.waiting.clone())
        })
        .ok()
        .flatten()
}

pub(super) fn channel_selected(channel: serenity::ChannelId) -> bool {
    CAPABILITY
        .try_with(|capability| capability.waiting.original.channel_id == channel)
        .unwrap_or(false)
}

pub(super) async fn park_worker(
    deps: &IntakeDeps<'_>,
    request: &IntakeRequest,
    waiting: Waiting,
    proof: Option<crate::services::cluster::intake_worker::BeforeEffects>,
) -> crate::db::intake_outbox::migration_wait::ParkOutcome {
    use crate::db::intake_outbox::migration_wait::ParkOutcome;
    if waiting.original != *request {
        tracing::warn!(
            channel_id = request.channel_id.get(),
            "migration wait request changed; custody remains held"
        );
        return ParkOutcome::Blocked("migration request changed".into());
    }
    let (Some(pool), Some(proof)) = (deps.shared.pg_pool.as_ref(), proof) else {
        tracing::warn!(
            channel_id = request.channel_id.get(),
            "migration wait has no exact worker custody proof"
        );
        return ParkOutcome::Blocked("migration has no exact before-effects proof".into());
    };
    let result =
        crate::db::intake_outbox::migration_wait::park_before_effects(pool, proof, &waiting.intent)
            .await;
    tracing::info!(channel_id = request.channel_id.get(), reason = ?waiting.reason, ?result, "migration wait retains the original worker input");
    result.unwrap_or_else(ParkOutcome::Unknown)
}

pub(crate) struct LocalCustodyWitness {
    intent: Arc<WaitIntent>,
    sources: Vec<serenity::MessageId>,
    digest: [u8; 32],
    created_at: std::time::Instant,
    role: Option<u64>,
}

fn original_digest(item: &Intervention) -> Option<[u8; 32]> {
    if item.source_message_ids.is_empty()
        || item.source_message_queued_generations.len() != item.source_message_ids.len()
        || item
            .source_message_ids
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != item.source_message_ids.len()
        || item.source_message_ids.iter().any(|id| {
            item.source_message_queued_generations
                .iter()
                .filter(|s| s.message_id == *id)
                .count()
                != 1
        })
        || item.source_message_queued_generations.iter().any(|s| {
            !item.source_message_ids.contains(&s.message_id)
                || s.enqueued_at_epoch_us.is_none()
                || s.queued_generation == 0
                || s.origin.as_deref().is_none_or(str::is_empty)
                || (s.message_id == item.message_id
                    && s.queued_generation != item.queued_generation)
        })
    {
        return None;
    }
    let owners: Vec<_> = item
        .source_message_queued_generations()
        .into_iter()
        .map(|s| {
            serde_json::json!([
                s.message_id.get(),
                s.queued_generation,
                s.enqueued_at_epoch_us,
                s.preserve_on_cancel,
                s.route,
                s.origin
            ])
        })
        .collect();
    let text: Vec<_> = item
        .source_text_segments()
        .into_iter()
        .map(|s| (s.message_id.get(), s.text))
        .collect();
    let value = serde_json::json!([
        item.author_id.get(),
        item.author_is_bot,
        item.message_id.get(),
        item.queued_generation,
        item.source_message_ids
            .iter()
            .map(|id| id.get())
            .collect::<Vec<_>>(),
        owners,
        text,
        item.text,
        format!("{:?}", item.mode),
        item.reply_context,
        item.has_reply_boundary,
        item.merge_consecutive,
        item.pending_uploads,
        item.voice_announcement
    ]);
    Some(Sha256::digest(serde_json::to_vec(&value).ok()?).into())
}

impl LocalCustodyWitness {
    fn from_ack(
        waiting: &Waiting,
        original: &Intervention,
        persistence: &QueuePersistenceContext,
    ) -> Option<Self> {
        if original
            .pending_uploads
            .iter()
            .any(|upload| upload.is_local())
        {
            return None;
        }
        let witness = Self {
            intent: waiting.intent.clone(),
            sources: original.source_message_ids.clone(),
            digest: original_digest(original)?,
            created_at: original.created_at,
            role: persistence.dispatch_role_override,
        };
        witness
            .namespace_matches(persistence, waiting.original.channel_id)
            .then_some(witness)
    }
    pub(crate) fn intent_matches(&self, intent: &WaitIntent) -> bool {
        self.intent.as_ref() == intent
    }
    pub(crate) fn matches_exact(&self, item: &Intervention) -> bool {
        self.created_at == item.created_at && original_digest(item) == Some(self.digest)
    }
    pub(crate) fn sources(&self) -> &[serenity::MessageId] {
        &self.sources
    }
    pub(crate) fn namespace_matches(
        &self,
        persistence: &QueuePersistenceContext,
        channel: serenity::ChannelId,
    ) -> bool {
        let authority = &self.intent.authority;
        authority.provider == persistence.provider.as_str()
            && authority.token_hash == persistence.token_hash
            && authority.channel_id == channel.get()
            && self.role == persistence.dispatch_role_override
            && crate::config::runtime_root()
                .and_then(|root| root.canonicalize().ok())
                .as_ref()
                == Some(&authority.runtime_root)
    }
}

#[derive(Debug)]
pub(crate) struct WaitingAdmission {
    pub(super) waiting: Waiting,
    pub(super) permit: super::intake_dispatch::LocalAdmissionPermit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Eligibility {
    Unsubmitted,
    Protected,
    Unknown,
}

pub(super) async fn replay_eligibility(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    request: &IntakeRequest,
) -> Eligibility {
    use super::intake_dispatch::replay_admission::{
        self as replay, ReplayAdmissionContext, ReplayPreflight,
    };
    let Some(pool) = deps.shared.pg_pool.as_ref() else {
        return Eligibility::Unknown;
    };
    let input = replay::discord_input(
        provider.as_str(),
        request.channel_id.get(),
        request.user_msg_id.get(),
        request.source_message_ids.iter().map(|id| id.get()),
        &request.user_text,
    );
    let instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let channel = request.channel_id.get().to_string();
    let context = ReplayAdmissionContext::production(pool, &instance, replay::incarnation())
        .for_channel(&channel);
    match replay::preflight_source(&context, &input).await {
        ReplayPreflight::Admit(_) | ReplayPreflight::LegacyUnprotected => Eligibility::Unsubmitted,
        ReplayPreflight::ConsumeProtected(_) => Eligibility::Protected,
        ReplayPreflight::Defer(_) => Eligibility::Unknown,
    }
}

#[derive(Debug)]
pub(super) struct QueuedProtected;
impl std::fmt::Display for QueuedProtected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("queued migration source is already protected by its original request")
    }
}
impl std::error::Error for QueuedProtected {}

#[derive(Debug)]
pub(super) struct QueuedWaiting(pub(super) Waiting);
impl std::fmt::Display for QueuedWaiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("migration awaits original queue head restoration")
    }
}
impl std::error::Error for QueuedWaiting {}

struct Retained;
impl Retained {
    fn new() -> Self {
        let _ = CAPABILITY.try_with(|cap| cap.retained.store(true, Ordering::SeqCst));
        Self
    }
}
impl Drop for Retained {
    fn drop(&mut self) {
        let _ = CAPABILITY.try_with(|cap| cap.retained.store(false, Ordering::SeqCst));
    }
}

async fn durable_original(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    original: &Intervention,
) -> bool {
    let provider = provider.clone();
    let token = deps.shared.token_hash.clone();
    let expected = original_digest(original);
    expected.is_some()
        && tokio::task::spawn_blocking(move || {
            crate::services::turn_orchestrator::load_channel_pending_queue_checked(
                &provider, &token, channel,
            )
        })
        .await
        .ok()
        .and_then(Result::ok)
        .is_some_and(|(items, _)| items.iter().any(|item| original_digest(item) == expected))
}

fn remember_ack(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    waiting: &Waiting,
    original: &Intervention,
) {
    let persistence =
        super::super::queue_persistence_context(deps.shared, provider, waiting.original.channel_id);
    let Some(witness) = LocalCustodyWitness::from_ack(waiting, original, &persistence) else {
        return;
    };
    let _ = CAPABILITY.try_with(|cap| {
        let mut witnesses = cap.witnesses.lock().unwrap();
        if !witnesses
            .iter()
            .any(|w| w.intent_matches(&waiting.intent) && w.matches_exact(original))
        {
            witnesses.push(Arc::new(witness));
        }
    });
}

pub(super) async fn restored_ack(
    deps: &IntakeDeps<'_>,
    waiting: &Waiting,
    provider: &ProviderKind,
    original: &Intervention,
) {
    let request = &waiting.original;
    let mut expected = request.source_message_ids.clone();
    expected.push(request.user_msg_id);
    expected.sort_unstable();
    expected.dedup();
    let mut actual = original.source_message_ids.clone();
    actual.sort_unstable();
    actual.dedup();
    if request.user_msg_id != original.message_id
        || request.request_owner != original.author_id
        || request.user_text != original.text
        || request.reply_context != original.reply_context
        || request.has_reply_boundary != original.has_reply_boundary
        || expected != actual
    {
        return;
    }
    if replay_eligibility(deps, provider, request).await == Eligibility::Unsubmitted
        && durable_original(deps, provider, request.channel_id, original).await
    {
        remember_ack(deps, provider, waiting, original);
    }
}

async fn enqueue_original(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    original: Intervention,
) -> crate::services::turn_orchestrator::EnqueueInterventionResult {
    let result = deps
        .shared
        .mailboxes
        .enqueue_migration_with_closed_retry(
            channel,
            original,
            super::super::queue_persistence_context(deps.shared, provider, channel),
        )
        .await;
    super::super::apply_queue_exit_feedback(deps.shared, channel, &result.queue_exit_events).await;
    super::super::arm_slow_idle_queue_backstop_if_queue_nonempty(
        deps.shared,
        provider,
        channel,
        "migration_custody",
    )
    .await;
    result
}

async fn after_failed_persistence() {
    let retry = CAPABILITY
        .try_with(|cap| cap.retry_after_failure.clone())
        .ok()
        .flatten();
    match retry {
        Some(retry) => retry.notified().await,
        None => {
            tokio::time::sleep(super::super::queue_io::DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await
        }
    }
}

pub(super) async fn hold_live(
    deps: &IntakeDeps<'_>,
    mut submission: super::intake_dispatch::IntakeSubmission,
    admission: WaitingAdmission,
) -> Result<super::IntakeExecution, crate::services::discord::Error> {
    let WaitingAdmission { waiting, permit } = admission;
    if !permit.permits_submission(&submission) || waiting.original != submission.request {
        return Err("migration wait request changed; original source remains unread".into());
    }
    if matches!(
        submission.origin,
        super::intake_dispatch::IntakeOrigin::QueuedDrain(_)
    ) {
        return Err(Box::new(QueuedWaiting(waiting)));
    }
    permit.transfer_waiting_uploads(&mut submission);
    let uploads = super::intake_dispatch::turn_mode_uploads(
        deps,
        &submission,
        &submission.request.channel_id.get().to_string(),
    )
    .await?;
    submission.preloaded_uploads.extend(uploads);
    submission.attachments.clear();
    let (provider, channel, mut original) =
        super::intake_dispatch::deferred_input(submission, true);
    if let Ok(Some(created_at)) = CAPABILITY.try_with(|cap| cap.original_created_at) {
        original.created_at = created_at;
    }
    let _ = CAPABILITY.try_with(|cap| {
        let mut observed = cap.original_observed.lock().unwrap();
        if observed.is_none() {
            *observed = Some(original.clone());
        }
    });
    let _retained = Retained::new();
    loop {
        let _ = CAPABILITY.try_with(|cap| cap.custody_attempts.fetch_add(1, Ordering::SeqCst));
        match replay_eligibility(deps, &provider, &waiting.original).await {
            Eligibility::Protected => return Ok(super::IntakeExecution::ConsumedProtected),
            Eligibility::Unknown => {
                tokio::time::sleep(super::super::queue_io::DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY)
                    .await;
                continue;
            }
            Eligibility::Unsubmitted => {}
        }
        let outcome = enqueue_original(deps, &provider, channel, original.clone()).await;
        if outcome.persistence_error.is_some() {
            let _ =
                CAPABILITY.try_with(|cap| cap.persistence_failures.fetch_add(1, Ordering::SeqCst));
        } else if (outcome.enqueued
            || outcome.merged
            || outcome.refusal_reason
                == Some(
                    crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued,
                ))
            && durable_original(deps, &provider, channel, &original).await
        {
            if outcome.enqueued {
                let _ = CAPABILITY
                    .try_with(|cap| cap.persistence_successes.fetch_add(1, Ordering::SeqCst));
            }
            remember_ack(deps, &provider, &waiting, &original);
            return Ok(super::IntakeExecution::Deferred);
        }
        tracing::warn!(channel_id=channel.get(), reason=?waiting.reason, "migration retains its original input until durable queue custody is verified");
        after_failed_persistence().await;
    }
}

#[test]
fn no_capability_keeps_migration_unselected() {
    let _ = std::mem::size_of::<Waiting>();
    assert!(CAPABILITY.try_with(|_| ()).is_err());
}
