//! Shared replay preflight and start preparation for every provider input effect.

use crate::db::replay_disposition::receipt::{self, Disposition, ReceiptSnapshot};
pub(crate) use crate::db::replay_disposition::write::CanonicalInput;
use crate::db::replay_disposition::write::{self, BeginFrom, EffectProjection, WriteError};
use crate::services::discord::replay_policy::permit::{EffectTarget, RetryPermit, StartPermit};
use sqlx::PgPool;

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
            let from = BeginFrom::Registered { nonce };
            write::begin(ctx.pool, receipt.id, from, &effect, ctx.incarnation).await
        }
        (Some(retry), Some(receipt_id)) if retry.receipt_id() == receipt_id => {
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
    }
    .map_err(StartRefusal::Write)?;
    StartPermit::seal(ack, &projection.target).map_err(|_| StartRefusal::PermitMismatch)
}
