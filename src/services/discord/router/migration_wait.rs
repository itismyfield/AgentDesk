//! Staged input custody for the first Claude migration; production has no capability yet.
use super::message_handler::{IntakeDeps, IntakeRequest};
use crate::db::intake_outbox::migration_wait::WaitIntent;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::Intervention;
use poise::serenity_prelude as serenity;
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
) {
    if waiting.original != *request {
        tracing::warn!(
            channel_id = request.channel_id.get(),
            "migration wait request changed; custody remains held"
        );
        return;
    }
    let (Some(pool), Some(proof)) = (deps.shared.pg_pool.as_ref(), proof) else {
        tracing::warn!(
            channel_id = request.channel_id.get(),
            "migration wait has no exact worker custody proof"
        );
        return;
    };
    let result =
        crate::db::intake_outbox::migration_wait::park_before_effects(pool, proof, &waiting.intent)
            .await;
    tracing::info!(channel_id = request.channel_id.get(), reason = ?waiting.reason, ?result, "migration wait retains the original worker input");
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

fn holds_original(item: &Intervention, original: &Intervention) -> bool {
    let sources = original.source_text_segments();
    let observed = item.source_text_segments();
    let uploads = &original.pending_uploads;
    let exact_sources = item.source_message_ids.len() == original.source_message_ids.len()
        && item
            .source_message_ids
            .iter()
            .all(|id| original.source_message_ids.contains(id));
    let retained_uploads = if exact_sources {
        item.pending_uploads == *uploads
    } else {
        uploads.is_empty()
            || item
                .pending_uploads
                .windows(uploads.len())
                .any(|u| u == uploads)
    };
    let owners = item.source_message_queued_generations();
    let expected_owners = original.source_message_queued_generations();
    item.author_id == original.author_id
        && item.author_is_bot == original.author_is_bot
        && item.reply_context == original.reply_context
        && item.has_reply_boundary == original.has_reply_boundary
        && original
            .source_message_ids
            .iter()
            .all(|id| item.source_message_ids.contains(id))
        && sources.iter().all(|s| {
            observed
                .iter()
                .any(|o| o.message_id == s.message_id && o.text == s.text)
        })
        && expected_owners.iter().all(|s| {
            owners.iter().any(|o| {
                o.message_id == s.message_id
                    && o.preserve_on_cancel == s.preserve_on_cancel
                    && o.route == s.route
                    && s.origin
                        .as_ref()
                        .is_none_or(|origin| o.origin.as_ref() == Some(origin))
            })
        })
        && retained_uploads
        && original
            .voice_announcement
            .as_ref()
            .is_none_or(|v| item.voice_announcement.as_ref() == Some(v))
}

async fn durable_original(
    deps: &IntakeDeps<'_>,
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    original: &Intervention,
) -> bool {
    let provider = provider.clone();
    let token = deps.shared.token_hash.clone();
    tokio::task::spawn_blocking(move || {
        crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &provider, &token, channel,
        )
    })
    .await
    .ok()
    .and_then(Result::ok)
    .is_some_and(|(items, _)| items.iter().any(|item| holds_original(item, original)))
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
    let (provider, channel, original) = super::intake_dispatch::deferred_input(submission, true);
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
        let acknowledged =
            super::intake_dispatch::defer_original(deps, &provider, channel, original.clone())
                .await;
        if acknowledged && durable_original(deps, &provider, channel, &original).await {
            return Ok(super::IntakeExecution::Deferred);
        }
        tracing::warn!(channel_id=channel.get(), reason=?waiting.reason, "migration retains its original input until durable queue custody is verified");
        tokio::time::sleep(super::super::queue_io::DEFERRED_IDLE_QUEUE_BACKSTOP_DELAY).await;
    }
}

#[test]
fn no_capability_keeps_migration_unselected() {
    let _ = std::mem::size_of::<Waiting>();
    assert!(CAPABILITY.try_with(|_| ()).is_err());
}
