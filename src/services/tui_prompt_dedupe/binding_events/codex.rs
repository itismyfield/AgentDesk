//! Codex observations carry the verifier's identity, never a fresh pathname stat.

use super::*;
use crate::services::cluster::stream_relay::SourceFileIdentity;
use crate::services::codex_tui::session::codex_tui_rollout_paths_same;
use crate::services::codex_tui::session::source_observation::VerifiedCodexHookSource;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;

fn history(channel: u64, tmux: &str, nonce: &str) -> io::Result<Vec<BindingEvent>> {
    let mut events = binding_events_since(channel, 0)?;
    events.retain(|e| {
        e.provider == "codex"
            && e.tmux_session == tmux
            && e.execution_nonce.as_deref() == Some(nonce)
    });
    Ok(events)
}

fn current(events: &[BindingEvent]) -> Option<(&BindingEvent, &SourceId)> {
    events.iter().rev().find_map(|e| match &e.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some((e, source)),
        _ => None,
    })
}

/// Whether an event up to `seq` named the claim as its old, new, or pending source.
fn named_before(
    events: &[BindingEvent],
    seq: u64,
    claim: impl Fn(&str, Option<&Path>) -> bool,
) -> bool {
    events.iter().any(|e| {
        e.seq <= seq
            && (e
                .old
                .as_ref()
                .is_some_and(|old| claim(&old.session_id, Some(&old.path)))
                || match &e.new {
                    BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                        claim(&source.session_id, Some(&source.path))
                    }
                    BindingTarget::Pending {
                        payload_session_id,
                        payload_transcript_path,
                    } => claim(
                        payload_session_id,
                        payload_transcript_path.as_deref().map(Path::new),
                    ),
                    BindingTarget::Rejected { .. } => false,
                })
    })
}

pub(crate) fn superseded(context: &BindingContext, session: &str) -> io::Result<bool> {
    let events = history(
        context.channel_id.unwrap_or_default(),
        &context.tmux_session,
        &context.execution_nonce,
    )?;
    let Some((latest, current)) = current(&events) else {
        return Ok(false);
    };
    if current.session_id == session {
        return Ok(false);
    }
    Ok(named_before(&events, latest.seq, |id, _| id == session))
}

/// The hook-recorded current source when the claimed source is one it already retired.
pub(crate) fn source_ahead(
    channel: u64,
    tmux: &str,
    nonce: &str,
    session: Option<&str>,
    path: &Path,
) -> io::Result<Option<SourceId>> {
    let events = history(channel, tmux, nonce)?;
    let Some((latest, current)) = current(&events) else {
        return Ok(None);
    };
    let claim = |id: &str, other: Option<&Path>| {
        session.is_some_and(|session| session == id)
            || other.is_some_and(|other| codex_tui_rollout_paths_same(other, path))
    };
    let retired = latest.evidence.hook_event.is_some()
        && !claim(&current.session_id, Some(&current.path))
        && named_before(&events, latest.seq, claim);
    Ok(retired.then(|| current.clone()))
}

/// Whether the recorded source's file is still the one the hook verified.
pub(crate) fn source_file_matches(source: &SourceId) -> bool {
    fs::metadata(&source.path).is_ok_and(|meta| file_identity(&meta) == (source.dev, source.ino))
}

pub(crate) fn record(
    context: &BindingContext,
    session: &str,
    hook: &HookSignal,
    verified: Option<&VerifiedCodexHookSource>,
) -> io::Result<bool> {
    let channel = context
        .channel_id
        .filter(|id| *id != 0)
        .ok_or_else(|| io::Error::other("Codex binding has no channel log"))?;
    if log_path(channel)?.is_none() {
        return Err(io::Error::other("Codex binding log unavailable"));
    }
    let source = verified
        .map(|v| {
            let (dev, ino) = match v.identity {
                #[cfg(unix)]
                SourceFileIdentity::Unix { dev, ino } => (dev, ino),
                SourceFileIdentity::Unavailable => {
                    return Err(io::Error::other("unverified identity"));
                }
            };
            Ok(SourceId {
                session_id: v.session_id.clone(),
                path: v.rollout_path.clone(),
                dev,
                ino,
            })
        })
        .transpose()?;
    let committed = commit_with(channel, |writer| {
        match writer.plan_codex(context, session, hook, source) {
            Some(event) => Planned::Append(event, false, None),
            None => Planned::Keep(Committed::Unchanged),
        }
    })?;
    Ok(committed == Committed::Appended)
}

impl Writer {
    fn plan_codex(
        &self,
        context: &BindingContext,
        session: &str,
        hook: &HookSignal,
        source: Option<SourceId>,
    ) -> Option<BindingEvent> {
        let pane = self.panes.get(&context.tmux_session);
        let old = pane.and_then(|pane| pane.current.clone());
        let pending = pane.and_then(|pane| pane.pending.as_ref()).filter(|pending| {
            pending.execution_nonce.as_deref() == Some(&context.execution_nonce)
                && matches!(&pending.new, BindingTarget::Pending { payload_session_id, .. } if payload_session_id == session)
        });
        if source
            .as_ref()
            .is_some_and(|source| Some(source) == old.as_ref())
            || (source.is_none() && pending.is_some())
        {
            return None;
        }
        let new = match (source, pending) {
            (Some(source), Some(pending)) => BindingTarget::Resolved {
                pending_seq: pending.seq,
                source,
            },
            (Some(source), None) => BindingTarget::Source(source),
            (None, _) => BindingTarget::Pending {
                payload_session_id: session.to_owned(),
                payload_transcript_path: hook.transcript_path.clone(),
            },
        };
        Some(BindingEvent {
            seq: self.last_seq + 1,
            channel_id: context.channel_id.unwrap_or_default(),
            provider: "codex".to_owned(),
            tmux_session: context.tmux_session.clone(),
            execution_nonce: Some(context.execution_nonce.clone()),
            old,
            new,
            cause: pending.map_or_else(|| hook.cause(), |p| p.cause),
            parent_hint: pending.and_then(|p| p.parent_hint.clone()),
            evidence: BindingEvidence {
                hook_event: Some(hook.event.clone()),
                received_at: hook.received_at,
            },
            committed_at: Utc::now(),
        })
    }
}

/// Additive launch evidence; Claude's `verified` flag is not Codex ownership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ownership {
    pub schema: u32,
    pub seq: u64,
    pub context: BindingContext,
    pub claim: Claim,
    pub pending_seq: Option<u64>,
    // A revalidated source may reuse eligibility without changing the exact Pending claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_proof_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub canonical_witnesses: Vec<CanonicalWitness>,
}

/// Canonical equivalence checked at append time, bound to both recorded descriptors.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalWitness {
    pub observation_seq: u64,
    pub observed: SourceId,
    pub source: SourceId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ClaimEvidence {
    NativeHook {
        event: String,
        source: Option<String>,
        first_prompt_digest: Option<String>,
    },
    ExplicitResume {
        executed_session_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Claim {
    pub session_id: String,
    pub path: Option<PathBuf>,
    pub evidence: ClaimEvidence,
}

impl Claim {
    fn fresh(&self) -> bool {
        matches!(&self.evidence, ClaimEvidence::NativeHook { event, source, .. }
            if event == "user_prompt_submit" || (event == "session_start" && source.as_deref() == Some("startup")))
    }

    pub(crate) fn same_candidate(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.path == other.path
            && ((self.fresh() && other.fresh()) || self.evidence == other.evidence)
    }

    fn same_source_candidate(&self, other: &Self, source: &SourceId) -> bool {
        self.session_id == other.session_id
            && self.session_id == source.session_id
            && [self.path.as_ref(), other.path.as_ref()]
                .into_iter()
                .all(|path| path.is_none_or(|path| path == &source.path))
            && ((self.fresh() && other.fresh()) || self.evidence == other.evidence)
    }

    fn eligible(&self, context: &BindingContext) -> bool {
        match &self.evidence {
            ClaimEvidence::ExplicitResume {
                executed_session_id,
            } => {
                context.launch_mode == "resume"
                    && executed_session_id == &self.session_id
                    && context.expected_native_session_id.as_deref() == Some(&self.session_id)
            }
            ClaimEvidence::NativeHook {
                event,
                first_prompt_digest,
                ..
            } => {
                context.launch_mode == "fresh"
                    && context.expected_native_session_id.is_none()
                    && self.fresh()
                    && (event != "user_prompt_submit"
                        || first_prompt_digest.as_ref().is_some_and(|digest| {
                            valid_digest(digest)
                                && Some(digest) == context.first_prompt_digest.as_ref()
                        }))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaimRecord {
    pub seq: u64,
    pub claim: Claim,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedProof {
    pub seq: u64,
    pub source: SourceId,
    pub ownership: Ownership,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Fold {
    pub legacy_unverified: Vec<BindingEvent>,
    pub pending: Vec<ClaimRecord>,
    // Consumers must refuse `conflicted` before using the retained proof.
    pub verified: Option<VerifiedProof>,
    pub conflicted: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum Decision {
    Pending,
    Verified(SourceId),
    Rejected(String),
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn absolute_clean(path: &Path) -> bool {
    path.is_absolute()
        && !path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
}

fn claim_path(context: &BindingContext, path: &Path) -> bool {
    absolute_clean(path)
        && context
            .provider_root
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
}

fn ownership_context(context: &BindingContext) -> io::Result<u64> {
    if context.schema != 1
        || context.provider != "codex"
        || context.source_policy.as_deref() != Some("verified")
        || context.execution_nonce.len() != 32
        || !context
            .execution_nonce
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
        || context.tmux_session.is_empty()
        || !absolute_clean(Path::new(&context.owner_runtime_root))
        || !context
            .provider_root
            .as_ref()
            .is_some_and(|path| absolute_clean(path))
        || !matches!(context.launch_mode.as_str(), "fresh" | "resume")
    {
        return Err(invalid("invalid Codex ownership context"));
    }
    context
        .channel_id
        .filter(|id| *id != 0)
        .ok_or_else(|| invalid("Codex ownership has no channel"))
}

/// Called with the log mutex held; validate every line before selecting an incarnation.
fn ownership_records(channel: u64) -> io::Result<Vec<Logged>> {
    let path = log_path(channel)?.ok_or_else(|| invalid("Codex binding log unavailable"))?;
    let read = read_log::<Logged>(&path)?;
    if read.lines != read.records.len() as u64 {
        return Err(invalid("unreadable Codex ownership history"));
    }
    for (index, logged) in read.records.iter().enumerate() {
        let event = &logged.event;
        if event.seq != index as u64 + 1 || event.channel_id != channel {
            return Err(invalid("Codex ownership history sequence/channel mismatch"));
        }
        if let Some(proof) = &logged.codex_ownership {
            let valid = proof.schema == 1
                && proof.seq == event.seq
                && ownership_context(&proof.context)? == channel
                && event.provider == "codex"
                && event.tmux_session == proof.context.tmux_session
                && event.execution_nonce.as_deref() == Some(&proof.context.execution_nonce)
                && uuid::Uuid::parse_str(&proof.claim.session_id).is_ok()
                && proof
                    .claim
                    .path
                    .as_ref()
                    .is_none_or(|path| claim_path(&proof.context, path));
            if !valid {
                return Err(invalid("invalid Codex ownership envelope"));
            }
            let (hook_event, cause) = claim_event(&proof.claim);
            if event.evidence.hook_event != hook_event || event.cause != cause {
                return Err(invalid("Codex ownership lifecycle mismatch"));
            }
            let (session, path) = match &event.new {
                BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                    if (!proof.claim.eligible(&proof.context) && proof.reused_proof_seq.is_none())
                        || !claim_path(&proof.context, &source.path)
                    {
                        return Err(invalid("ineligible Codex ownership proof"));
                    }
                    (&source.session_id, Some(source.path.as_path()))
                }
                BindingTarget::Pending {
                    payload_session_id,
                    payload_transcript_path,
                }
                | BindingTarget::Rejected {
                    payload_session_id,
                    payload_transcript_path,
                    ..
                } => (
                    payload_session_id,
                    payload_transcript_path.as_deref().map(Path::new),
                ),
            };
            if session != &proof.claim.session_id
                || proof
                    .claim
                    .path
                    .as_deref()
                    .is_some_and(|claim| Some(claim) != path)
                || matches!(&event.new, BindingTarget::Resolved { pending_seq, .. } if Some(*pending_seq) != proof.pending_seq)
                || matches!(&event.new, BindingTarget::Source(_) | BindingTarget::Pending { .. } if proof.pending_seq.is_some())
                || matches!(&event.new, BindingTarget::Pending { payload_transcript_path, .. } | BindingTarget::Rejected { payload_transcript_path, .. }
                    if payload_transcript_path.as_deref().map(Path::new) != proof.claim.path.as_deref())
            {
                return Err(invalid("Codex claim/source mismatch"));
            }
            validate_reused_proof(proof, event, &read.records)?;
            validate_witnesses(proof, event, &read.records)?;
        }
    }
    let mut checked = std::collections::HashSet::new();
    for proof in read
        .records
        .iter()
        .filter_map(|record| record.codex_ownership.as_ref())
    {
        if checked.insert((&proof.context.tmux_session, &proof.context.execution_nonce)) {
            fold_records(&proof.context, &read.records)?;
        }
    }
    Ok(read.records)
}

fn fold_records(context: &BindingContext, records: &[Logged]) -> io::Result<Fold> {
    let mut fold = Fold::default();
    for logged in records.iter().filter(|logged| {
        let event = &logged.event;
        event.provider == "codex"
            && event.tmux_session == context.tmux_session
            && event.execution_nonce.as_deref() == Some(&context.execution_nonce)
    }) {
        let event = &logged.event;
        let Some(ownership) = &logged.codex_ownership else {
            fold.legacy_unverified.push(event.clone());
            continue;
        };
        if &ownership.context != context {
            return Err(invalid("Codex immutable context changed"));
        }
        if let Some(seq) = ownership.pending_seq {
            let index = fold
                .pending
                .iter()
                .position(|pending| {
                    pending.seq == seq && pending.claim.same_candidate(&ownership.claim)
                })
                .ok_or_else(|| invalid("Codex resolution/rejection has no matching Pending"))?;
            fold.pending.remove(index);
        }
        match &event.new {
            BindingTarget::Pending { .. } => {
                if fold
                    .pending
                    .iter()
                    .any(|p| p.claim.same_candidate(&ownership.claim))
                {
                    return Err(invalid("duplicate Codex Pending claim"));
                }
                fold.pending.push(ClaimRecord {
                    seq: event.seq,
                    claim: ownership.claim.clone(),
                });
            }
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                if fold.verified.as_ref().is_some_and(|p| {
                    p.source != *source
                        || ownership.reused_proof_seq.is_some_and(|seq| seq != p.seq)
                        || (!p.ownership.claim.same_candidate(&ownership.claim)
                            && ownership.reused_proof_seq != Some(p.seq))
                }) || !fold.pending.is_empty()
                    || !neutral_legacy(context, records, source, event.seq, |seq, observed| {
                        recorded_native(seq, observed, source, &ownership.canonical_witnesses)
                    })
                {
                    return Err(invalid("conflicting Codex ownership proofs"));
                }
                fold.verified = Some(VerifiedProof {
                    seq: event.seq,
                    source: source.clone(),
                    ownership: ownership.clone(),
                });
            }
            BindingTarget::Rejected { .. } => {}
        }
    }
    fold.conflicted =
        fold.pending.len() > 1 || (fold.verified.is_some() && !fold.pending.is_empty());
    Ok(fold)
}

pub(crate) fn read_ownership(context: &BindingContext) -> io::Result<Fold> {
    let channel = ownership_context(context)?;
    let _logs = lock_logs();
    fold_records(context, &ownership_records(channel)?)
}

pub(crate) fn proof_at_seq(
    context: &BindingContext,
    seq: u64,
) -> io::Result<Option<VerifiedProof>> {
    let channel = ownership_context(context)?;
    let _logs = lock_logs();
    Ok(ownership_records(channel)?.into_iter().find_map(|logged| {
        let ownership = logged.codex_ownership?;
        if logged.event.seq != seq || ownership.context != *context {
            return None;
        }
        match logged.event.new {
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => {
                Some(VerifiedProof {
                    seq,
                    source,
                    ownership,
                })
            }
            _ => None,
        }
    }))
}

pub(crate) fn context_for_nonce(
    channel: u64,
    tmux: &str,
    nonce: Option<&str>,
) -> io::Result<Option<BindingContext>> {
    let _logs = lock_logs();
    if log_path(channel)?.is_none() {
        return Ok(None);
    }
    Ok(ownership_records(channel)?
        .into_iter()
        .filter_map(|record| record.codex_ownership)
        .rev()
        .find(|record| {
            record.context.tmux_session == tmux
                && nonce.is_none_or(|nonce| record.context.execution_nonce == nonce)
        })
        .map(|record| record.context))
}

fn same_identity(left: &SourceId, right: &SourceId) -> bool {
    left.session_id == right.session_id && (left.dev, left.ino) == (right.dev, right.ino)
}

fn same_native(left: &SourceId, right: &SourceId) -> bool {
    same_identity(left, right)
        && fs::canonicalize(&left.path)
            .ok()
            .zip(fs::canonicalize(&right.path).ok())
            .is_some_and(|(left, right)| left == right)
}

fn recorded_native(
    seq: u64,
    observed: &SourceId,
    source: &SourceId,
    witnesses: &[CanonicalWitness],
) -> bool {
    same_identity(observed, source)
        && (observed.path == source.path
            || witnesses.iter().any(|witness| {
                witness.observation_seq == seq
                    && witness.observed == *observed
                    && witness.source == *source
            }))
}

fn validate_reused_proof(
    ownership: &Ownership,
    event: &BindingEvent,
    records: &[Logged],
) -> io::Result<()> {
    let Some(seq) = ownership.reused_proof_seq else {
        return Ok(());
    };
    let BindingTarget::Resolved { source, .. } = &event.new else {
        return Err(invalid("reused Codex proof without resolution"));
    };
    let prior = seq
        .checked_sub(1)
        .and_then(|index| usize::try_from(index).ok())
        .and_then(|index| records.get(index))
        .filter(|record| record.event.seq == seq && seq < event.seq)
        .ok_or_else(|| invalid("invalid reused Codex proof sequence"))?;
    let prior_ownership = prior
        .codex_ownership
        .as_ref()
        .filter(|prior| prior.context == ownership.context)
        .ok_or_else(|| invalid("reused Codex proof incarnation mismatch"))?;
    let prior_source = match &prior.event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
        _ => return Err(invalid("reused Codex record is not a proof")),
    };
    if ownership.pending_seq.is_none()
        || prior_source != source
        || event.old.as_ref() != Some(source)
        || !prior_ownership
            .claim
            .same_source_candidate(&ownership.claim, source)
    {
        return Err(invalid("reused Codex proof candidate mismatch"));
    }
    Ok(())
}

fn validate_witnesses(
    proof: &Ownership,
    event: &BindingEvent,
    records: &[Logged],
) -> io::Result<()> {
    let mut seen = Vec::new();
    for witness in &proof.canonical_witnesses {
        let source = match &event.new {
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => source,
            _ => return Err(invalid("canonical witness without Codex proof")),
        };
        let observation = witness
            .observation_seq
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| records.get(index))
            .filter(|logged| {
                let observation = &logged.event;
                observation.seq < event.seq
                    && observation.provider == event.provider
                    && observation.tmux_session == event.tmux_session
                    && observation.execution_nonce == event.execution_nonce
                    && logged.codex_ownership.is_none()
            })
            .ok_or_else(|| invalid("invalid canonical witness observation"))?;
        if witness.source != *source
            || !same_identity(&witness.observed, source)
            || witness.observed.path == source.path
            || !(observation.event.old.as_ref() == Some(&witness.observed)
                || matches!(&observation.event.new,
                    BindingTarget::Source(observed) if observed == &witness.observed))
            || seen.contains(witness)
        {
            return Err(invalid("canonical witness identity mismatch"));
        }
        seen.push(witness.clone());
    }
    Ok(())
}

fn neutral_legacy(
    context: &BindingContext,
    records: &[Logged],
    source: &SourceId,
    through_seq: u64,
    mut same_native: impl FnMut(u64, &SourceId) -> bool,
) -> bool {
    let mut current_source_seen = false;
    for logged in records
        .iter()
        .take_while(|logged| logged.event.seq <= through_seq)
        .filter(|l| l.event.provider == "codex" && l.event.tmux_session == context.tmux_session)
    {
        let event = &logged.event;
        if event.execution_nonce.as_deref() != Some(&context.execution_nonce) {
            if context.launch_mode == "fresh"
                && named_before(std::slice::from_ref(event), event.seq, |id, _| {
                    id == source.session_id
                })
            {
                return false;
            }
            continue;
        }
        if logged.codex_ownership.is_none() {
            let neutral = event.evidence.hook_event.is_none()
                && matches!(event.cause, BindingCause::Startup | BindingCause::Unknown)
                && matches!(&event.new, BindingTarget::Source(s) if same_native(event.seq, s))
                && (!current_source_seen
                    || event
                        .old
                        .as_ref()
                        .is_none_or(|old| same_native(event.seq, old)));
            if !neutral {
                return false;
            }
        }
        current_source_seen |= matches!(
            event.new,
            BindingTarget::Source(_) | BindingTarget::Resolved { .. }
        );
    }
    true
}

/// Caller holds source authority; revalidate canonical/current context and descriptor inside `validate_current`.
/// The callback must not reacquire the channel log mutex or publish runtime state.
pub(crate) fn commit_claim(
    context: &BindingContext,
    claim: &Claim,
    decision: Decision,
    validate_current: impl FnOnce() -> io::Result<()>,
) -> io::Result<Fold> {
    let channel = ownership_context(context)?;
    let mut failure = None;
    commit_with(channel, |writer| {
        let plan = (|| -> io::Result<Planned> {
            validate_current()?;
            let records = ownership_records(channel)?;
            if records.len() as u64 != writer.last_seq {
                return Err(invalid("Codex writer/history revision mismatch"));
            }
            let fold = fold_records(context, &records)?;
            if uuid::Uuid::parse_str(&claim.session_id).is_err()
                || claim
                    .path
                    .as_ref()
                    .is_some_and(|path| !claim_path(context, path))
            {
                return Err(invalid("invalid Codex native claim"));
            }
            let pending = fold.pending.iter().find(|p| p.claim.same_candidate(claim));
            let mut canonical_witnesses = Vec::new();
            let mut reused_proof_seq = None;
            let verified = match &decision {
                Decision::Verified(source) => {
                    if !claim_path(context, &source.path)
                        || source.session_id != claim.session_id
                        || claim.path.as_ref().is_some_and(|path| path != &source.path)
                        || !source_file_matches(source)
                    {
                        return Err(invalid("stale Codex claim descriptor"));
                    }
                    reused_proof_seq = fold
                        .verified
                        .as_ref()
                        .filter(|proof| {
                            pending.is_some()
                                && proof.source == *source
                                && proof.ownership.claim.same_source_candidate(claim, source)
                        })
                        .map(|proof| proof.seq);
                    let same_proof = fold.verified.as_ref().is_some_and(|p| {
                        p.source == *source
                            && (p.ownership.claim.same_candidate(claim)
                                || reused_proof_seq.is_some())
                    });
                    if same_proof && pending.is_none() {
                        return Ok(Planned::Keep(Committed::Unchanged));
                    }
                    (claim.eligible(context) || reused_proof_seq.is_some())
                        && (fold.verified.is_none() || same_proof)
                        && fold.pending.iter().all(|p| p.claim.same_candidate(claim))
                        && neutral_legacy(
                            context,
                            &records,
                            source,
                            writer.last_seq,
                            |seq, observed| {
                                let prior = fold
                                    .verified
                                    .as_ref()
                                    .map(|proof| &proof.ownership.canonical_witnesses[..])
                                    .unwrap_or_default();
                                if !recorded_native(seq, observed, source, prior)
                                    && !same_native(observed, source)
                                {
                                    return false;
                                }
                                if observed.path != source.path {
                                    let witness = CanonicalWitness {
                                        observation_seq: seq,
                                        observed: observed.clone(),
                                        source: source.clone(),
                                    };
                                    if !canonical_witnesses.contains(&witness) {
                                        canonical_witnesses.push(witness);
                                    }
                                }
                                true
                            },
                        )
                }
                _ => false,
            };
            let (new, pending_seq) = match decision {
                Decision::Verified(source) if verified => (
                    match pending {
                        Some(p) => BindingTarget::Resolved {
                            pending_seq: p.seq,
                            source,
                        },
                        None => BindingTarget::Source(source),
                    },
                    pending.map(|p| p.seq),
                ),
                Decision::Rejected(reason) => (
                    BindingTarget::Rejected {
                        payload_session_id: claim.session_id.clone(),
                        payload_transcript_path: claim
                            .path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                        reason,
                    },
                    pending.map(|p| p.seq),
                ),
                _ if pending.is_some() => return Ok(Planned::Keep(Committed::Unchanged)),
                _ => (
                    BindingTarget::Pending {
                        payload_session_id: claim.session_id.clone(),
                        payload_transcript_path: claim
                            .path
                            .as_ref()
                            .map(|p| p.to_string_lossy().into_owned()),
                    },
                    None,
                ),
            };
            let (hook_event, cause) = claim_event(claim);
            let seq = writer.last_seq + 1;
            let event = BindingEvent {
                seq,
                channel_id: channel,
                provider: "codex".to_owned(),
                tmux_session: context.tmux_session.clone(),
                execution_nonce: Some(context.execution_nonce.clone()),
                old: fold.verified.map(|p| p.source),
                new,
                cause,
                parent_hint: None,
                evidence: BindingEvidence {
                    hook_event,
                    received_at: Utc::now(),
                },
                committed_at: Utc::now(),
            };
            Ok(Planned::CodexAppend(
                event,
                Box::new(Ownership {
                    schema: 1,
                    seq,
                    context: context.clone(),
                    claim: claim.clone(),
                    pending_seq,
                    reused_proof_seq: verified.then_some(reused_proof_seq).flatten(),
                    canonical_witnesses: if verified {
                        canonical_witnesses
                    } else {
                        Vec::new()
                    },
                }),
            ))
        })();
        match plan {
            Ok(plan) => plan,
            Err(error) => {
                failure = Some(error);
                Planned::Keep(Committed::Unchanged)
            }
        }
    })?;
    if let Some(error) = failure {
        return Err(error);
    }
    read_ownership(context)
}

fn claim_event(claim: &Claim) -> (Option<String>, BindingCause) {
    match &claim.evidence {
        ClaimEvidence::NativeHook { event, source, .. } => {
            let hook = HookSignal {
                event: event.clone(),
                source: source.clone(),
                transcript_path: None,
                received_at: Utc::now(),
                published_at: None,
            };
            (Some(event.clone()), hook.cause())
        }
        ClaimEvidence::ExplicitResume { .. } => (None, BindingCause::Resume),
    }
}
