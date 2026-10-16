//! Codex ownership appends; the channel log mutex orders hooks against launch proofs.

use super::*;

#[cfg(test)]
pub(crate) fn commit_claim(
    context: &BindingContext,
    claim: &Claim,
    decision: Decision,
    validate_current: impl FnOnce() -> io::Result<()>,
) -> io::Result<Fold> {
    commit_claim_published(context, claim, decision, None, validate_current)
}

/// Caller holds source authority; revalidate canonical/current context and descriptor inside `validate_current`.
/// The callback must not reacquire the channel log mutex or publish runtime state.
pub(crate) fn commit_claim_published(
    context: &BindingContext,
    claim: &Claim,
    decision: Decision,
    published_at: Option<DateTime<Utc>>,
    validate_current: impl FnOnce() -> io::Result<()>,
) -> io::Result<Fold> {
    commit(
        context,
        claim,
        decision,
        published_at,
        validate_current,
        None,
    )
}

/// Appends an executed resume's initial proof, resolving the sole native resume Pending only
/// after `validate_pending` proves that Pending names the same descriptor; never appends Pending.
pub(crate) fn commit_executed_resume_published(
    context: &BindingContext,
    claim: &Claim,
    source: SourceId,
    validate_current: impl FnOnce() -> io::Result<()>,
    validate_pending: impl Fn(&Claim) -> io::Result<()>,
) -> io::Result<Fold> {
    commit(
        context,
        claim,
        Decision::Verified(source),
        None,
        validate_current,
        Some(&validate_pending),
    )
}

type PendingCheck<'a> = &'a dyn Fn(&Claim) -> io::Result<()>;

fn commit(
    context: &BindingContext,
    claim: &Claim,
    decision: Decision,
    published_at: Option<DateTime<Utc>>,
    validate_current: impl FnOnce() -> io::Result<()>,
    executed: Option<PendingCheck<'_>>,
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
            // Reject unordered switches before even an unavailable descriptor can create Pending.
            let changed = fold.verified.as_ref().is_some_and(|proof| match &decision {
                Decision::Verified(source) => source != &proof.source,
                _ => {
                    claim.session_id != proof.source.session_id
                        || claim
                            .path
                            .as_ref()
                            .is_some_and(|path| path != &proof.source.path)
                }
            });
            if changed
                && fold.verified.as_ref().is_some_and(|proof| {
                    !published_at.is_some_and(|time| time > proof_time(proof, &records))
                })
            {
                tracing::warn!(
                    tmux = context.tmux_session,
                    ?published_at,
                    "Codex source transition held: missing or non-increasing publish time"
                );
                return Ok(Planned::Keep(Committed::Unchanged));
            }
            // Selected under the log mutex, so a hook that won the race is resolved, not split.
            let initial = match (executed, &decision) {
                (Some(validate_pending), Decision::Verified(source)) => fold
                    .pending
                    .iter()
                    .find(|p| initial_resume_resolves(context, &fold, p, claim, source))
                    .map(|p| validate_pending(&p.claim).map(|()| p))
                    .transpose()?,
                _ => None,
            };
            let pending =
                initial.or_else(|| fold.pending.iter().find(|p| p.claim.same_candidate(claim)));
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
                    let transition = changed
                        && fold
                            .verified
                            .as_ref()
                            .is_some_and(|proof| claim.transition_from(&proof.source));
                    (claim.eligible(context) || reused_proof_seq.is_some() || transition)
                        && (fold.verified.is_none() || same_proof || transition)
                        && fold.pending.iter().all(|p| {
                            p.claim.same_candidate(claim) || initial.is_some_and(|i| i.seq == p.seq)
                        })
                        && neutral_legacy(
                            context,
                            &records,
                            source,
                            fold.verified.is_none(),
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
            if executed.is_some() && !verified {
                return Ok(Planned::Keep(Committed::Unchanged));
            }
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
                    published_at,
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
