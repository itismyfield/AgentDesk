//! Shared replay preflight and start preparation for every provider input effect.

use crate::db::replay_disposition::receipt::{self, Disposition, ReceiptSnapshot};
pub(crate) use crate::db::replay_disposition::write::CanonicalInput;
use crate::db::replay_disposition::write::{self, BeginFrom, EffectProjection, WriteError};
use crate::services::discord::replay_policy::live::LiveAttempt;
use crate::services::discord::replay_policy::permit::{
    EffectTarget, PermitRefusal, PreparedRetryStart, RetryPermit, StartPermit,
};
use crate::services::provider::CancelToken;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// This process's owner incarnation for the attempts it begins.
static INCARNATION: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| uuid::Uuid::new_v4().to_string());

pub(crate) fn incarnation() -> &'static str {
    &INCARNATION
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProducerMode {
    Dormant,
    Active,
}

pub(crate) struct ReplayAdmissionContext<'a> {
    pub(crate) pool: &'a PgPool,
    pub(crate) instance: &'a str,
    pub(crate) incarnation: &'a str,
    pub(crate) producers: ProducerMode,
}

impl<'a> ReplayAdmissionContext<'a> {
    /// No activation supplier is connected yet, so production stays dormant whatever the environment says.
    pub(crate) fn production(pool: &'a PgPool, instance: &'a str, incarnation: &'a str) -> Self {
        Self {
            pool,
            instance,
            incarnation,
            producers: ProducerMode::Dormant,
        }
    }

    /// Producers turn on only where an activation supplier connected; none exists outside tests.
    pub(crate) fn for_channel(mut self, channel: &str) -> Self {
        self.producers = producers_for(channel);
        self
    }
}

fn producers_for(channel: &str) -> ProducerMode {
    match activation_supplied(channel) {
        true => ProducerMode::Active,
        false => ProducerMode::Dormant,
    }
}

/// No production activation supplier is connected, so no environment or config turns it on.
#[cfg(not(test))]
fn activation_supplied(_channel: &str) -> bool {
    false
}

#[cfg(test)]
fn activation_supplied(channel: &str) -> bool {
    test_activation::active(channel)
}

/// The start of a dormant build: the legacy path, with no receipt and no permit.
pub(crate) fn dormant_start(channel: &str) -> Option<ReplayStart> {
    (producers_for(channel) == ProducerMode::Dormant).then_some(ReplayStart(None))
}

/// Hash of the prompt a start actually submits.
pub(crate) fn input_hash(prompt: &str) -> String {
    hex::encode(Sha256::digest(prompt.as_bytes()))
}

/// The canonical identity of a Discord request: its first message keys it, every absorbed source
/// stays with it, and a later re-entry keeps the same key.
pub(crate) fn discord_input(
    provider: &str,
    channel: u64,
    user_msg_id: u64,
    sources: impl IntoIterator<Item = u64>,
    original_text: &str,
) -> CanonicalInput {
    let mut sources: Vec<String> = std::iter::once(user_msg_id)
        .chain(sources)
        .map(|id| id.to_string())
        .collect();
    sources.sort_unstable();
    sources.dedup();
    CanonicalInput {
        request_key: format!(
            "discord:{}:{channel}:{user_msg_id}",
            provider.to_lowercase()
        ),
        provider: provider.to_string(),
        channel: channel.to_string(),
        sources,
        original_text: original_text.to_string(),
        owner_id: String::new(),
        agent_id: String::new(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    }
}

/// The right to make a provider start's first input: a dormant build's legacy start, or one
/// sealed permit. Only `dormant_start` and `admit_start` make one.
#[derive(Debug)]
pub(crate) struct ReplayStart(Option<StartPermit>);

impl ReplayStart {
    pub(crate) fn receipt_id(&self) -> Option<i64> {
        self.0.as_ref().map(|permit| permit.attempt().receipt_id())
    }

    /// An exact retry's start: the approved local reset runs only now, after its begin committed.
    pub(crate) fn exact_reset(self, reset: impl FnOnce()) -> Result<Self, PermitRefusal> {
        let permit = self.0.ok_or(PermitRefusal::NotARetry)?;
        let prepared = PreparedRetryStart::new((), permit)?;
        Ok(prepared.launch(|_| reset(), |(), permit| Self(Some(permit))))
    }

    /// Spends the start right before the first input; a permit binds its attempt to the token so
    /// the reader records it, and a mismatch refuses the start.
    pub(crate) fn spend(
        self,
        target: &EffectTarget,
        prompt: &str,
        token: &CancelToken,
    ) -> Result<(), PermitRefusal> {
        let Some(permit) = self.0 else {
            return Ok(());
        };
        let live =
            |attempt| token.attach_replay_attempt(std::sync::Arc::new(LiveAttempt::new(attempt)));
        match permit.consume(target, &input_hash(prompt), live)? {
            true => Ok(()),
            false => Err(PermitRefusal::Spent),
        }
    }
}

/// Execution and replay custody remain separate outcomes through queued and worker settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntakeExecution {
    Executed,
    Deferred,
    ConsumedProtected,
}

impl IntakeExecution {
    pub(crate) fn deferred(channel: poise::serenity_prelude::ChannelId) -> Self {
        if super::checkpoint_waits_for_admission(channel) {
            Self::Deferred
        } else {
            Self::Executed
        }
    }
}

/// Why a start did not happen; the request keeps its custody either way.
#[derive(Debug)]
pub(crate) enum StartStop {
    /// A held or finished request covers the input; nothing reruns it.
    Consumed,
    /// The start is undecided; the input waits in its existing custody.
    Deferred(String),
}

/// The replay check of one provider start: a dormant build keeps the legacy start without IO; an
/// active one begins the request, or its exact continuation, and seals the permit.
pub(crate) async fn admit_start(
    ctx: &ReplayAdmissionContext<'_>,
    input: &CanonicalInput,
    projection: PreparedEffectProjection,
    continuation: Option<RetryPermit>,
) -> Result<ReplayStart, StartStop> {
    let candidate = match (preflight_source(ctx, input).await, continuation.as_ref()) {
        (ReplayPreflight::Admit(candidate), None) => candidate,
        (ReplayPreflight::ConsumeProtected(held), Some(retry)) => {
            held.resume(retry).map_err(|_| StartStop::Consumed)?
        }
        (ReplayPreflight::ConsumeProtected(_), None) => return Err(StartStop::Consumed),
        (other, _) => return Err(StartStop::Deferred(format!("{other:?}"))),
    };
    match prepare_effect_start(ctx, input, candidate, projection, continuation).await {
        Ok(permit) => Ok(ReplayStart(Some(permit))),
        Err(StartRefusal::Protected(receipt) | StartRefusal::AlreadyClassified(receipt)) => {
            tracing::info!(receipt, "the request's receipt already started or finished");
            Err(StartStop::Consumed)
        }
        Err(StartRefusal::Write(error)) => Err(StartStop::Deferred(error.to_string())),
        Err(refusal) => Err(StartStop::Deferred(format!("{refusal:?}"))),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReplayDefer {
    IdentityUnknown,
    LookupFailed,
    UnknownDisposition,
    MixedSources,
    ProducerDormant,
}

/// Lookup result that lets a caller register and begin; it is not itself a start.
#[derive(Debug)]
pub(crate) struct ReplayCandidate {
    request_key: String,
    retry_receipt: Option<i64>,
}

/// Every blocking receipt that covers the input's sources.
#[derive(Debug)]
pub(crate) struct BlockingReceiptSet {
    pub(crate) receipts: Vec<ReceiptSnapshot>,
}

impl BlockingReceiptSet {
    /// Only the holder of the exact retry continuation may re-enter its own no-effect receipt.
    pub(crate) fn resume(self, retry: &RetryPermit) -> Result<ReplayCandidate, Self> {
        match self.receipts.as_slice() {
            [receipt]
                if receipt.id == retry.receipt_id()
                    && receipt.disposition == Disposition::StartupFailedNoEffect
                    && receipt.episode_nonce.as_deref() == Some(retry.previous_nonce()) =>
            {
                Ok(ReplayCandidate {
                    request_key: receipt.request_key.clone().unwrap_or_default(),
                    retry_receipt: Some(receipt.id),
                })
            }
            _ => Err(self),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ReplayPreflight {
    LegacyUnprotected,
    Admit(ReplayCandidate),
    ConsumeProtected(BlockingReceiptSet),
    Defer(ReplayDefer),
}

/// Classifies the input against every receipt that names its key or any of its sources.
pub(crate) async fn preflight_source(
    ctx: &ReplayAdmissionContext<'_>,
    input: &CanonicalInput,
) -> ReplayPreflight {
    let named = [&input.request_key, &input.provider, &input.channel];
    if named
        .into_iter()
        .chain(&input.sources)
        .any(|value| value.trim().is_empty())
        || input.sources.is_empty()
    {
        return ReplayPreflight::Defer(ReplayDefer::IdentityUnknown);
    }
    let key = Some(input.request_key.as_str());
    let Ok(receipts) = receipt::receipts_for_request(
        ctx.pool,
        &input.provider,
        &input.channel,
        &input.sources,
        key,
    )
    .await
    else {
        return ReplayPreflight::Defer(ReplayDefer::LookupFailed);
    };
    if receipts
        .iter()
        .any(|r| matches!(r.disposition, Disposition::Unknown(_)))
    {
        return ReplayPreflight::Defer(ReplayDefer::UnknownDisposition);
    }
    let registered = receipts
        .iter()
        .any(|r| r.disposition == Disposition::RegisteredNotStarted);
    let blocking: Vec<_> = receipts
        .into_iter()
        .filter(|r| r.disposition.blocks_rerun())
        .collect();
    if !blocking.is_empty() {
        let covered = input
            .sources
            .iter()
            .all(|s| blocking.iter().any(|r| r.sources.contains(s)));
        return match covered {
            true => ReplayPreflight::ConsumeProtected(BlockingReceiptSet { receipts: blocking }),
            false => ReplayPreflight::Defer(ReplayDefer::MixedSources),
        };
    }
    match (ctx.producers, registered) {
        (ProducerMode::Active, _) => ReplayPreflight::Admit(ReplayCandidate {
            request_key: input.request_key.clone(),
            retry_receipt: None,
        }),
        (ProducerMode::Dormant, true) => ReplayPreflight::Defer(ReplayDefer::ProducerDormant),
        (ProducerMode::Dormant, false) => ReplayPreflight::LegacyUnprotected,
    }
}

/// The effect a start prepares: its target, the prepared input hash and the owner binding to store.
pub(crate) struct PreparedEffectProjection {
    pub(crate) target: EffectTarget,
    pub(crate) input_hash: String,
    pub(crate) binding: serde_json::Value,
    pub(crate) session_key: Option<String>,
}

#[derive(Debug)]
pub(crate) enum StartRefusal {
    ProducerDormant,
    CandidateMismatch,
    Protected(i64),
    AlreadyClassified(i64),
    SessionUnbound,
    BusySessionRebind,
    Write(WriteError),
    PermitMismatch,
}

/// Registers or reuses the receipt, commits the begin with its projection and seals one permit.
pub(crate) async fn prepare_effect_start(
    ctx: &ReplayAdmissionContext<'_>,
    input: &CanonicalInput,
    candidate: ReplayCandidate,
    projection: PreparedEffectProjection,
    continuation: Option<RetryPermit>,
) -> Result<StartPermit, StartRefusal> {
    if ctx.producers == ProducerMode::Dormant {
        return Err(StartRefusal::ProducerDormant);
    }
    if candidate.request_key != input.request_key {
        return Err(StartRefusal::CandidateMismatch);
    }
    let target_key = projection.target.key();
    let effect = EffectProjection {
        effect_target: &target_key,
        input_hash: &projection.input_hash,
        binding: &projection.binding,
    };
    if effect.requires_prior_ack() && projection.session_key.is_some() {
        return Err(StartRefusal::BusySessionRebind);
    }
    let ack = match (continuation, candidate.retry_receipt) {
        (None, None) => {
            let receipt = write::register_or_reuse(ctx.pool, input, ctx.instance)
                .await
                .map_err(StartRefusal::Write)?;
            match receipt.disposition {
                Disposition::RegisteredNotStarted => {}
                Disposition::ClassifiedNormal => {
                    return Err(StartRefusal::AlreadyClassified(receipt.id));
                }
                _ => return Err(StartRefusal::Protected(receipt.id)),
            }
            let nonce = receipt.episode_nonce.as_deref().unwrap_or_default();
            if effect.requires_prior_ack() {
                let saved = effect.save(ctx.pool, receipt.id, nonce).await;
                #[cfg(test)]
                let saved =
                    test_activation::lose_ack(&format!("projection:{}", input.channel), saved);
                saved.map_err(StartRefusal::Write)?;
            }
            if let Some(session_key) = projection.session_key.as_deref() {
                write::bind_session(ctx.pool, session_key, receipt.id, nonce)
                    .await
                    .map_err(StartRefusal::Write)?;
            }
            let from = BeginFrom::Registered { nonce };
            write::begin(ctx.pool, receipt.id, from, &effect, ctx.incarnation).await
        }
        (Some(retry), Some(receipt_id)) if retry.receipt_id() == receipt_id => {
            write::ensure_same_request(ctx.pool, receipt_id, input)
                .await
                .map_err(StartRefusal::Write)?;
            let from = BeginFrom::NoEffect {
                previous: retry.previous_nonce(),
                next: retry.next_nonce(),
                session_key: projection
                    .session_key
                    .as_deref()
                    .ok_or(StartRefusal::SessionUnbound)?,
            };
            write::begin(ctx.pool, receipt_id, from, &effect, ctx.incarnation).await
        }
        _ => return Err(StartRefusal::CandidateMismatch),
    };
    #[cfg(test)]
    let ack = test_activation::lose_ack(&input.channel, ack);
    let ack = ack.map_err(StartRefusal::Write)?;
    StartPermit::seal(ack, &projection.target).map_err(|_| StartRefusal::PermitMismatch)
}

/// The activation supplier of tests: producers turn on only for the channels a guard names.
#[cfg(test)]
pub(crate) mod test_activation {
    static ACTIVE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    pub(super) fn active(channel: &str) -> bool {
        ACTIVE
            .lock()
            .unwrap()
            .iter()
            .any(|active| active == channel)
    }

    pub(crate) struct Activated(String);

    pub(crate) fn activate(channel: u64) -> Activated {
        ACTIVE.lock().unwrap().push(channel.to_string());
        Activated(channel.to_string())
    }

    impl Drop for Activated {
        fn drop(&mut self) {
            ACTIVE.lock().unwrap().retain(|active| active != &self.0);
        }
    }

    static ACK_LOST: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// The channel's next committed begin loses its acknowledgement.
    pub(crate) fn lose_next_ack(channel: u64) {
        ACK_LOST.lock().unwrap().push(channel.to_string());
    }

    /// The next committed busy projection loses its acknowledgement.
    pub(crate) fn lose_next_projection_ack(channel: u64) {
        ACK_LOST
            .lock()
            .unwrap()
            .push(format!("projection:{channel}"));
    }

    pub(super) fn lose_ack<T>(
        channel: &str,
        ack: Result<T, super::WriteError>,
    ) -> Result<T, super::WriteError> {
        let mut lost = ACK_LOST.lock().unwrap();
        match lost.iter().position(|lost| lost == channel) {
            Some(index) if ack.is_ok() => {
                lost.remove(index);
                Err(super::WriteError::AckUnknown(sqlx::Error::PoolTimedOut))
            }
            _ => ack,
        }
    }
}
