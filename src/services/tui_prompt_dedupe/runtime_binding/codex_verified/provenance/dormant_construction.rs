//! Live submit receipts bind exact episode evidence; historical boot construction stays gated.

#[cfg(test)]
use super::super::permission::NativeSubmitFloor;
use super::super::{CodexEpisodeSpan, ExecutionProofRef, TurnEpisodeRef};
use crate::services::codex_tui::rollout_tail::provenance::NativeTurnAnchor;
#[cfg(test)]
use crate::services::codex_tui::rollout_tail::provenance::{scan_anchor, terminal_end};
use crate::services::provider::CancelToken;
#[cfg(test)]
use crate::services::tui_o::shadow::capture::SourceCapture;
#[cfg(test)]
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, MAX_READ_BYTES};
#[cfg(test)]
use crate::services::tui_o::store::ChannelStore;
#[cfg(test)]
use crate::services::tui_o::store::codex_provenance::Sidecar;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use std::sync::Arc;

pub(crate) struct EpisodeEvidenceSnapshot {
    token: Arc<CancelToken>,
    episode: TurnEpisodeRef,
}

impl EpisodeEvidenceSnapshot {
    pub(crate) fn capture(
        token: Arc<CancelToken>,
        episode: TurnEpisodeRef,
    ) -> Result<Self, &'static str> {
        if token.turn_nonce() != Some(episode.turn_nonce.as_str())
            || episode.channel_id == 0
            || episode.user_message_id == 0
            || episode.request_owner_id == 0
            || token.cancelled.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err("episode_identity_unavailable");
        }
        Ok(Self { token, episode })
    }

    fn matches(&self, expected: &Arc<CancelToken>) -> bool {
        Arc::ptr_eq(&self.token, expected)
            && expected.turn_nonce() == Some(self.episode.turn_nonce.as_str())
            && !expected
                .cancelled
                .load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(crate) struct ActualCodexSubmission {
    token: Arc<CancelToken>,
    context: BindingContext,
}

/// The successful driver operation, rather than readiness or a restored token, creates the receipt.
#[cfg(test)]
pub(crate) fn actual_submit(
    evidence: &EpisodeEvidenceSnapshot,
    expected: &Arc<CancelToken>,
    context: &BindingContext,
    submit: impl FnOnce() -> Result<(), &'static str>,
) -> Result<ActualCodexSubmission, &'static str> {
    if !evidence.matches(expected)
        || context.channel_id != Some(evidence.episode.channel_id)
        || context.provider != "codex"
        || context.source_policy.as_deref() != Some("verified")
        || !matches!(context.launch_mode.as_str(), "fresh" | "resume")
    {
        return Err("submit_identity_mismatch");
    }
    submit()?;
    Ok(ActualCodexSubmission {
        token: expected.clone(),
        context: context.clone(),
    })
}

pub(crate) struct SubmittedEpisode {
    context: BindingContext,
    episode: TurnEpisodeRef,
}

pub(crate) fn submitted_episode(
    evidence: &EpisodeEvidenceSnapshot,
    expected: &Arc<CancelToken>,
    context: &BindingContext,
    submission: &ActualCodexSubmission,
) -> Result<SubmittedEpisode, &'static str> {
    if !evidence.matches(expected)
        || !Arc::ptr_eq(&submission.token, expected)
        || submission.context != *context
    {
        return Err("submit_identity_mismatch");
    }
    Ok(SubmittedEpisode {
        context: context.clone(),
        episode: evidence.episode.clone(),
    })
}

fn proof_matches(context: &BindingContext, proof: &ExecutionProofRef) -> bool {
    context.schema == 1
        && context.provider == "codex"
        && context.source_policy.as_deref() == Some("verified")
        && matches!(context.launch_mode.as_str(), "fresh" | "resume")
        && context.execution_nonce == proof.execution_nonce
        && context.tmux_session == proof.tmux_session
        && context.owner_runtime_root == proof.owner_runtime_root
        && proof.proof_seq != 0
        && context
            .expected_native_session_id
            .as_ref()
            .is_none_or(|id| id == &proof.source.session_id)
}

pub(crate) fn bind_episode_span(
    submitted: &SubmittedEpisode,
    proof: &ExecutionProofRef,
    anchor: NativeTurnAnchor,
    generation_mtime_ns: i64,
) -> Result<CodexEpisodeSpan, &'static str> {
    if !proof_matches(&submitted.context, proof)
        || anchor.native_turn_id.trim().is_empty()
        || (!submitted.episode.native_turn_id.is_empty()
            && submitted.episode.native_turn_id != anchor.native_turn_id)
    {
        return Err("span_identity_mismatch");
    }
    let mut episode = submitted.episode.clone();
    episode.native_turn_id = anchor.native_turn_id;
    Ok(CodexEpisodeSpan {
        execution: proof.clone(),
        delivery_channel_id: episode.channel_id,
        offset_authority_channel_id: episode.channel_id,
        episode,
        generation_mtime_ns,
        start: anchor.start,
        end: None,
    })
}

#[cfg(test)]
pub(crate) struct BootOwedRecord {
    execution: ExecutionProofRef,
    episode: TurnEpisodeRef,
    turn_start_offset: u64,
    native_floor: NativeSubmitFloor,
    boot_eof: u64,
    generation_mtime_ns: i64,
}

#[cfg(test)]
impl BootOwedRecord {
    /// The boot caller pins the row and EOF before later polls can append another turn.
    pub(crate) fn from_scanned_row(
        execution: ExecutionProofRef,
        episode: TurnEpisodeRef,
        native_floor: &NativeSubmitFloor,
        boot_eof: u64,
        generation_mtime_ns: i64,
    ) -> Result<Self, &'static str> {
        let turn_start_offset = native_floor.verify_for(&execution)?;
        if episode.turn_nonce.trim().is_empty()
            || episode.channel_id == 0
            || episode.user_message_id == 0
            || episode.request_owner_id == 0
            || turn_start_offset > boot_eof
        {
            return Err("boot_row_identity_unavailable");
        }
        Ok(Self {
            execution,
            episode,
            turn_start_offset,
            native_floor: native_floor.clone(),
            boot_eof,
            generation_mtime_ns,
        })
    }
}

#[cfg(test)]
pub(crate) struct SourceCheckpoint {
    pub through: u64,
    pub prefix_hash: String,
}

/// Produces a span only after prefix validation, the actor's durable sidecar replace and a re-read
/// of that sidecar; a missing sidecar is never created here.
#[cfg(test)]
pub(crate) fn complete_boot_span(
    store: &mut ChannelStore,
    record: Option<&BootOwedRecord>,
    context: &BindingContext,
    checkpoint: &SourceCheckpoint,
) -> Result<Option<CodexEpisodeSpan>, &'static str> {
    complete_boot_span_with_hook(store, record, context, checkpoint, || {})
}

#[cfg(test)]
fn complete_boot_span_with_hook(
    store: &mut ChannelStore,
    record: Option<&BootOwedRecord>,
    context: &BindingContext,
    checkpoint: &SourceCheckpoint,
    before_save: impl FnOnce(),
) -> Result<Option<CodexEpisodeSpan>, &'static str> {
    let Some(record) = record else {
        return Err("boot_record_unavailable");
    };
    if !proof_matches(context, &record.execution)
        || context.channel_id != Some(record.episode.channel_id)
    {
        return Err("boot_row_identity_mismatch");
    }
    record
        .native_floor
        .verify_for(&record.execution)
        .map_err(|_| "source_checkpoint_changed")?;
    let mut capture = SourceCapture::reopen(
        record.execution.source.clone(),
        checkpoint.through,
        &checkpoint.prefix_hash,
    )
    .map_err(|_| "source_checkpoint_changed")?;
    if !capture
        .verify_prefix()
        .map_err(|_| "source_checkpoint_unavailable")?
    {
        return Err("source_checkpoint_changed");
    }
    let Sidecar::Present(stored) = store.codex_provenance() else {
        return Err("span_store_unavailable");
    };
    let spans = stored.spans().to_vec();
    let candidates: Vec<_> = spans
        .iter()
        .filter(|s| {
            s.execution == record.execution
                && s.episode.channel_id == record.episode.channel_id
                && s.episode.user_message_id == record.episode.user_message_id
                && s.episode.request_owner_id == record.episode.request_owner_id
                && s.episode.turn_nonce == record.episode.turn_nonce
        })
        .collect();
    let mut span = match candidates.as_slice() {
        [span] => (**span).clone(),
        [] => {
            let Some(anchor) = scan_anchor(
                &record.execution.source,
                record.turn_start_offset,
                record.boot_eof,
            )?
            else {
                return Ok(None);
            };
            let mut episode = record.episode.clone();
            episode.native_turn_id = anchor.native_turn_id;
            CodexEpisodeSpan {
                execution: record.execution.clone(),
                episode,
                delivery_channel_id: record.episode.channel_id,
                offset_authority_channel_id: record.episode.channel_id,
                generation_mtime_ns: record.generation_mtime_ns,
                start: anchor.start,
                end: None,
            }
        }
        _ => return Err("span_identity_ambiguous"),
    };
    if !record.episode.native_turn_id.is_empty()
        && record.episode.native_turn_id != span.episode.native_turn_id
    {
        return Err("boot_native_turn_mismatch");
    }
    if span.generation_mtime_ns != record.generation_mtime_ns
        || span.start < record.turn_start_offset
    {
        return Err("boot_span_mismatch");
    }
    let anchor = NativeTurnAnchor {
        native_turn_id: span.episode.native_turn_id.clone(),
        start: span.start,
    };
    let through = capture
        .file_len()
        .map_err(|_| "source_checkpoint_unavailable")?;
    // Retain the full scan prefix so the final guard also covers newly read terminal evidence.
    while capture.read_through() < through {
        let before = capture.read_through();
        let remaining = through - before;
        if matches!(
            capture.poll(remaining.min(MAX_READ_BYTES)),
            CaptureOutcome::Anomaly(_)
        ) {
            return Err("source_checkpoint_changed");
        }
        if capture.read_through() == before {
            return Err("source_checkpoint_unavailable");
        }
    }
    if capture.captured_through() != through {
        return Err("incomplete_native_record");
    }
    if let Some(end) = terminal_end(&span.execution.source, &anchor, through)? {
        if span.end.is_some_and(|old| old != end) {
            return Err("terminal_boundary_changed");
        }
        span.end = Some(end);
    }
    before_save();
    if !capture
        .verify_prefix()
        .map_err(|_| "source_checkpoint_unavailable")?
        || matches!(capture.poll(0), CaptureOutcome::Anomaly(_))
    {
        return Err("source_checkpoint_changed");
    }
    let span = (store.persist_codex_span(span)).map_err(|_| "span_store_unavailable")?;
    match store.codex_provenance() {
        Sidecar::Present(read) if read.spans().contains(&span) => Ok(Some(span)),
        _ => Err("span_store_unavailable"),
    }
}

#[cfg(test)]
#[path = "dormant_tests.rs"]
mod tests;

/// Only the real input operation's Submitted outcome creates warm evidence.
pub(crate) fn submit_warm(
    evidence: &EpisodeEvidenceSnapshot,
    expected: &Arc<CancelToken>,
    context: &BindingContext,
    submit: impl FnOnce() -> crate::services::codex_tui::input::CodexFollowupPromptSubmitOutcome,
) -> Result<ActualCodexSubmission, &'static str> {
    if !evidence.matches(expected)
        || context.channel_id != Some(evidence.episode.channel_id)
        || context.provider != "codex"
        || context.source_policy.as_deref() != Some("verified")
        || !matches!(context.launch_mode.as_str(), "fresh" | "resume")
    {
        return Err("submit_identity_mismatch");
    }
    if !matches!(
        submit(),
        crate::services::codex_tui::input::CodexFollowupPromptSubmitOutcome::Submitted
    ) {
        return Err("actual_submit_unconfirmed");
    }
    Ok(ActualCodexSubmission {
        token: Arc::clone(expected),
        context: context.clone(),
    })
}

impl ActualCodexSubmission {
    pub(in super::super) fn into_token(self) -> Arc<CancelToken> {
        self.token
    }

    pub(crate) fn issue_live(
        self,
        ready: &crate::services::tui_o::writer::host::Readiness,
        request: LiveGrantRequest<'_>,
    ) -> Result<super::super::live_grant::LiveGrant, super::super::decisions::LiveReadReason> {
        use super::super::decisions::{EvidenceReason as E, LiveReadReason};
        use crate::services::codex_tui::rollout_tail::provenance::scan_anchor;
        use crate::services::tui_o::store::codex_provenance::projection::floor_names;
        let LiveGrantRequest {
            evidence,
            submitted,
            prepared,
            source,
            through,
        } = request;
        if !evidence.matches(&self.token)
            || self.context != submitted.context
            || self.context != *source.context()
            || evidence.episode != submitted.episode
        {
            return Err(E::IdentityMismatch.into());
        }
        let floor = prepared.floor();
        if floor.execution() != source.execution() {
            return Err(E::IdentityMismatch.into());
        }
        let (native, start, _) = floor.checkpoint();
        let anchor = scan_anchor(native, start, through)
            .map_err(|_| LiveReadReason::from(E::ForeignTurn))?
            .ok_or(LiveReadReason::from(E::MissingEvidence))?;
        let span = bind_episode_span(submitted, floor.execution(), anchor, floor.generation())
            .map_err(|_| LiveReadReason::from(E::IdentityMismatch))?;
        if !floor_names(floor, &span) {
            return Err(E::IdentityMismatch.into());
        }
        let channel = span.episode.channel_id;
        let grant =
            super::super::live_grant::LiveGrant::from_receipt(self, span, floor.clone(), through)?;
        crate::services::tmux_common::with_tmux_source_authority(
            &source.context().tmux_session,
            |authority| {
                source
                    .recheck(authority)
                    .map_err(|_| LiveReadReason::from(E::SourceChanged))?;
                ready
                    .with_verified_prepared(channel, prepared, || grant)
                    .map_err(LiveReadReason::Persistence)
            },
        )
    }
}

pub(crate) struct LiveGrantRequest<'a> {
    pub evidence: &'a EpisodeEvidenceSnapshot,
    pub submitted: &'a SubmittedEpisode,
    pub prepared: &'a crate::services::tui_o::writer::actor::codex_submission::Prepared,
    pub source: &'a crate::services::tui_prompt_dedupe::codex_verified::VerifiedSourceSnapshot,
    pub through: u64,
}
