//! Raw custody at the existing ingress seams; an installed actor owns admission, never a callback.
use super::admission::{Admission, AdmissionTicket};
use crate::services::discord::router::IntakeExecution;
use crate::services::discord::{Intervention, MailboxEnqueueOutcome, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::json;
use sha2::{Digest, Sha256};

tokio::task_local! { static EXISTING_RETRY: bool; }

pub(crate) async fn existing_retry<F: std::future::Future>(future: F) -> F::Output {
    EXISTING_RETRY.scope(true, future).await
}

/// The one Pending contract; code, tests and the report state it in these words.
pub(crate) const PENDING_CONTRACT: &str = "Pending은 Accepted·Consumed·재접수 가능한 일반 오류가 아니다. 재호출·복구에서 원 입력과 identity를 유지한다.";

/// The owner has not confirmed this ticket's admission; see [`PENDING_CONTRACT`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OwnerPending(pub(crate) AdmissionTicket);

impl std::fmt::Display for OwnerPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "owner admission pending ({}): {PENDING_CONTRACT}",
            self.0.as_str()
        )
    }
}

impl std::error::Error for OwnerPending {}

/// What an ingress seam got from the owner; Pending is its own case, never an acceptance or refusal.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Intake {
    Accepted,
    Pending(OwnerPending),
    Rejected(String),
}

type Error = Box<dyn std::error::Error + Send + Sync>;

impl Intake {
    /// A live arrival: Pending stays typed and keeps the checkpoint, so catch-up re-enters the
    /// same message under the same derived ticket.
    pub(crate) fn live(self) -> Result<IntakeExecution, Error> {
        match self {
            Self::Accepted => Ok(IntakeExecution::Executed),
            Self::Pending(pending) => Err(Box::new(pending)),
            Self::Rejected(reason) => Err(reason.into()),
        }
    }

    /// A claimed worker row: Pending holds the row's custody unsettled; it is not a failure.
    pub(crate) fn worker(self) -> Result<IntakeExecution, Error> {
        match self {
            Self::Accepted => Ok(IntakeExecution::Executed),
            Self::Pending(_) => Ok(IntakeExecution::Deferred),
            Self::Rejected(reason) => Err(reason.into()),
        }
    }
}

/// Why an arrival waits in the queue instead of joining the running input; only these notify.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Hold {
    SessionSwitch,
    RestartRecovery,
    Uploads,
    ScreenMissing,
    Modal,
    ScreenContention,
    OwnerChange,
}

impl Hold {
    pub(crate) const ALL: [Self; 7] = [
        Self::SessionSwitch,
        Self::RestartRecovery,
        Self::Uploads,
        Self::ScreenMissing,
        Self::Modal,
        Self::ScreenContention,
        Self::OwnerChange,
    ];

    fn reason(self) -> &'static str {
        match self {
            Self::SessionSwitch => "세션 전환(/clear·/resume·헤드리스 시작) 처리",
            Self::RestartRecovery => "재시작 직후 복구 대기",
            Self::Uploads => "첨부 업로드 미완료·실패",
            Self::ScreenMissing => "TUI 화면·대화 기록 확인 불가",
            Self::Modal => "모달 창 표시",
            Self::ScreenContention => "같은 화면 주입 충돌",
            Self::OwnerChange => "턴 주인 변경 충돌",
        }
    }

    pub(crate) fn notice(self) -> String {
        format!(
            "-# {}로 즉시주입 하지 못하고 메시지큐에 들어갔습니다",
            self.reason()
        )
    }
}

/// The holds an ingress seam can see itself; the rest come from the owner's preparation.
fn ingress_hold(shared: &SharedData, uploads_pending: bool) -> Option<Hold> {
    use std::sync::atomic::Ordering::Relaxed;
    let restart = &shared.restart;
    if !restart.reconcile_done.load(Relaxed) || restart.restart_pending.load(Relaxed) {
        return Some(Hold::RestartRecovery);
    }
    uploads_pending.then_some(Hold::Uploads)
}

/// Notices a hold once per original and hold, and only while the queue durably holds the original
/// unattempted; an immediate unit or a Pending admission never notices.
pub(crate) async fn held(
    shared: &SharedData,
    handle: &ChannelMailboxHandle,
    channel: ChannelId,
    original: &Original,
    hold: Hold,
) -> bool {
    use super::admission::TurnBind;
    static NOTICED: std::sync::Mutex<Vec<(String, Hold)>> = std::sync::Mutex::new(Vec::new());
    let exact = super::admission::exact_turn(handle, &original.ticket, &original.item).await;
    if exact != Ok(TurnBind::NotArmed) {
        return false;
    }
    let key = (original.ticket.as_str().to_string(), hold);
    {
        let mut noticed = NOTICED.lock().unwrap_or_else(|e| e.into_inner());
        if noticed.contains(&key) {
            return false;
        }
        noticed.push(key);
    }
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return false;
    };
    crate::services::discord::discord_io::rate_limit_wait(shared, channel).await;
    let message = MessageId::new(original.item.message_id);
    let send = crate::services::discord::http::send_channel_message_with_reference;
    send(&http, channel, &hold.notice(), channel, message)
        .await
        .is_ok()
}

/// The caller keeps this exact original and ticket across Pending and restart.
#[derive(Clone)]
pub(crate) struct Original {
    pub(crate) item: PendingQueueItem,
    pub(crate) ticket: AdmissionTicket,
}

impl Original {
    pub(crate) fn with_ticket(item: PendingQueueItem, ticket: AdmissionTicket) -> Self {
        Self { item, ticket }
    }

    pub(crate) async fn admit(&self, handle: &ChannelMailboxHandle) -> Admission {
        super::admission::admit(handle, &self.item, self.ticket.clone()).await
    }

    /// Admits under the stable ticket on every call; a ticket the owner already holds, even as an
    /// unconfirmed candidate, resumes the stored original. True when this call first sees it committed.
    async fn admit_first(&mut self, handle: &ChannelMailboxHandle) -> (Intake, bool) {
        use super::admission::AdmissionState;
        let before = super::admission::status(handle, &self.ticket).await;
        let first = before.is_ok_and(|state| state != AdmissionState::Committed);
        let ticket = self.ticket.as_str().to_string();
        match handle
            .v2_resume_ticket(self.item.clone(), ticket, same_input)
            .await
        {
            Ok((_, stored)) => {
                self.item = stored;
                (Intake::Accepted, first)
            }
            Err(error) if error.is_pending() => {
                (Intake::Pending(OwnerPending(self.ticket.clone())), false)
            }
            Err(error) => (Intake::Rejected(error.into()), false),
        }
    }
}

/// A re-entry of the same message, however its route re-rendered it, against the stored original.
fn same_input(stored: &PendingQueueItem, offered: &PendingQueueItem) -> bool {
    stored.author_id == offered.author_id
        && stored.message_id == offered.message_id
        && stored.source_message_ids == offered.source_message_ids
        && stored.text == offered.text
}

pub(crate) async fn installed(handle: &ChannelMailboxHandle) -> Result<bool, String> {
    handle.v2_request(|owner| Ok(owner.is_some())).await
}

/// The live and worker seam: the owner admits the exact original, never a callback.
pub(crate) async fn intake(
    shared: &SharedData,
    provider: &ProviderKind,
    request: &crate::services::discord::IntakeRequest,
    uploads: crate::services::cluster::attachment_transfer::uploads::PendingUploads,
    voice: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
    durable_context: bool,
    uploads_pending: bool,
) -> Option<Intake> {
    if EXISTING_RETRY.try_with(|value| *value).unwrap_or(false) {
        return None;
    }
    let bot = request.turn_kind.is_background_trigger()
        || voice.is_some()
        || shared
            .settings
            .read()
            .await
            .allowed_bot_ids
            .contains(&request.request_owner.get());
    let raw = crate::services::discord::router::input_payload::owned_original(
        request,
        bot,
        uploads,
        voice,
        durable_context,
    );
    admit_raw(shared, provider, request.channel_id, &raw, uploads_pending).await
}

async fn admit_raw(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    raw: &Intervention,
    uploads_pending: bool,
) -> Option<Intake> {
    let handle = shared.mailbox(channel);
    match installed(&handle).await {
        Ok(false) => return None,
        Ok(true) => {}
        Err(error) => return Some(Intake::Rejected(error)),
    }
    let role = shared
        .dispatch
        .role_overrides
        .get(&channel)
        .map(|value| value.get());
    let mut original = match original(provider, channel, role, raw) {
        Ok(original) => original,
        Err(error) => return Some(Intake::Rejected(error)),
    };
    let (intake, first) = original.admit_first(&handle).await;
    if first && let Some(hold) = ingress_hold(shared, uploads_pending) {
        held(shared, &handle, channel, &original, hold).await;
    }
    Some(intake)
}

/// The ticket a Discord original derives from its provider, channel and sources, so every
/// re-entry of the same message, after Pending or a restart, names the same admission.
pub(crate) fn discord_ticket(
    provider: &ProviderKind,
    channel: ChannelId,
    message: u64,
    ids: &[u64],
) -> AdmissionTicket {
    let identity = json!([provider.as_str(), channel.get(), message, ids]);
    let digest = Sha256::digest(identity.to_string().as_bytes());
    AdmissionTicket::stored(&format!("discord-{}", hex::encode(&digest[..16])))
}

/// Copies the queue's provenance, including per-source cancellation and routing rights.
pub(crate) fn original(
    provider: &ProviderKind,
    channel: ChannelId,
    role: Option<u64>,
    intervention: &Intervention,
) -> Result<Original, String> {
    let ids: Vec<_> = if intervention.source_message_ids.is_empty() {
        vec![intervention.message_id.get()]
    } else {
        intervention
            .source_message_ids
            .iter()
            .map(|id| id.get())
            .collect()
    };
    if ids
        .iter()
        .any(|id| !crate::services::tui_input::input_key::is_discord_key(*id))
    {
        return Err("synthetic input requires its caller's stored admission ticket".into());
    }
    let generations: Vec<_> = intervention.source_message_queued_generations().into_iter().map(|source| json!({
        "message_id":source.message_id.get(), "queued_generation":source.queued_generation,
        "enqueued_at_epoch_us":source.enqueued_at_epoch_us,"preserve_on_cancel":source.preserve_on_cancel,
        "route":source.route,"origin":source.origin,
    })).collect();
    let segments: Vec<_> = intervention
        .source_text_segments()
        .into_iter()
        .map(|source| {
            json!({
                "message_id":source.message_id.get(),"text":source.text,
            })
        })
        .collect();
    let ticket = discord_ticket(provider, channel, intervention.message_id.get(), &ids);
    let age = u64::try_from(intervention.created_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let item = serde_json::from_value(json!({
        "author_id":intervention.author_id.get(),"author_is_bot":intervention.author_is_bot,
        "message_id":intervention.message_id.get(),"queued_generation":intervention.queued_generation,
        "created_at_wall_time_ms":now.saturating_sub(age),
        "source_message_ids":ids,"source_message_queued_generations":generations,
        "source_text_segments": if segments.len()>1 {segments} else {vec![]},
        "text":intervention.text,"reply_context":intervention.reply_context,
        "has_reply_boundary":intervention.has_reply_boundary,"merge_consecutive":intervention.merge_consecutive,
        "pending_uploads":intervention.pending_uploads,"voice_announcement":intervention.voice_announcement,
        "channel_id":channel.get(),"override_channel_id":role,
    })).map_err(|error| error.to_string())?;
    Ok(Original::with_ticket(item, ticket))
}

/// The common queue seam. Its outcome type has no Pending case, so Pending is reported as the
/// typed [`OwnerPending`] text; every caller keeps its custody on it and re-enters the same ticket.
pub(crate) async fn enqueue(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    intervention: &Intervention,
) -> Option<MailboxEnqueueOutcome> {
    let intake = admit_raw(shared, provider, channel, intervention, false).await?;
    let (enqueued, persistence_error) = match intake {
        Intake::Accepted => (true, None),
        Intake::Pending(pending) => (false, Some(pending.to_string())),
        Intake::Rejected(reason) => (false, Some(reason)),
    };
    Some(MailboxEnqueueOutcome {
        enqueued,
        merged: false,
        refusal_reason: None,
        persistence_error,
    })
}

#[cfg(all(test, unix))]
#[path = "ingress_tests.rs"]
mod tests;
