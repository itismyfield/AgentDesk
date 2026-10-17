//! Exact-sequence settlement of observations accompanying the first executed resume.

use super::*;

#[cfg(test)]
thread_local! {
    static AFTER_APPEND: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
    static CORRUPT_SEQ: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn after_companion_for_tests(action: impl FnOnce() + 'static) {
    AFTER_APPEND.with(|slot| *slot.borrow_mut() = Some(Box::new(action)));
}

#[cfg(test)]
pub(crate) fn corrupt_companion_candidate_for_tests() {
    CORRUPT_SEQ.with(|slot| slot.set(true));
}

pub(super) fn settle(
    context: &BindingContext,
    claim: &Claim,
    source: &SourceId,
    validate_current: &dyn Fn() -> io::Result<()>,
    validate_pending: PendingCheck<'_>,
) -> io::Result<()> {
    let channel = ownership_context(context)?;
    loop {
        let mut failure = None;
        let committed = commit_with(channel, |writer| {
            let result = (|| {
                validate_current()?;
                let records = ownership_records(channel)?;
                if records.len() as u64 != writer.last_seq {
                    return Err(invalid("Codex writer/history revision mismatch"));
                }
                let fold = fold_records(context, &records)?;
                plan_companion(
                    Request {
                        context,
                        claim,
                        decision: Decision::Verified(source.clone()),
                        published_at: None,
                        executed: Some(validate_pending),
                    },
                    writer.last_seq,
                    records,
                    fold,
                )
            })();
            match result {
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
        if committed != Committed::Appended {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(action) = AFTER_APPEND.with(|slot| slot.borrow_mut().take()) {
            action();
        }
    }
}

fn plan_companion(
    request: Request<'_>,
    last_seq: u64,
    mut records: Vec<Logged>,
    fold: Fold,
) -> io::Result<Planned> {
    let keep = || Planned::Keep(Committed::Unchanged);
    let Decision::Verified(source) = &request.decision else {
        return Ok(keep());
    };
    if fold.verified.is_some() || !request.claim.eligible(request.context) {
        return Ok(keep());
    }
    let mut anchor = None;
    let mut companions = Vec::new();
    for pending in &fold.pending {
        if initial_resume_resolves(request.context, &fold, pending, request.claim, source) {
            if anchor.replace(pending).is_some() {
                return Ok(keep());
            }
        } else if matches!(&pending.claim.evidence,
            ClaimEvidence::NativeHook { event, source, .. }
                if event == "user_prompt_submit"
                    && matches!(source.as_deref(), None | Some("unknown" | "startup" | "resume")))
        {
            companions.push(pending);
        } else {
            return Ok(keep());
        }
    }
    let (Some(anchor), Some(pending)) = (anchor, companions.into_iter().min_by_key(|p| p.seq))
    else {
        return Ok(keep());
    };
    let validate = request
        .executed
        .ok_or_else(|| invalid("Codex resume descriptor validator missing"))?;
    for observed in &fold.pending {
        validate(&observed.claim)?;
    }
    let prospective = Fold {
        pending: vec![anchor.clone()],
        conflicted: false,
        ..fold.clone()
    };
    if !matches!(
        plan(
            Request {
                context: request.context,
                claim: request.claim,
                decision: request.decision.clone(),
                published_at: None,
                executed: request.executed,
            },
            last_seq,
            &records,
            prospective,
        )?,
        Planned::CodexAppend(BindingEvent { new: BindingTarget::Resolved { pending_seq, .. }, .. }, _)
            if pending_seq == anchor.seq
    ) {
        return Ok(keep());
    }
    let candidate = plan(
        Request {
            context: request.context,
            claim: &pending.claim,
            decision: Decision::Rejected("superseded_by_executed_resume".into()),
            published_at: pending.published_at,
            executed: None,
        },
        last_seq,
        &records,
        fold.clone(),
    )?;
    let Planned::CodexAppend(event, ownership) = candidate else {
        return Ok(keep());
    };
    if ownership.pending_seq != Some(pending.seq) || ownership.claim != pending.claim {
        return Ok(keep());
    }
    #[cfg(test)]
    let ownership = {
        let mut ownership = ownership;
        if CORRUPT_SEQ.with(|slot| slot.replace(false)) {
            ownership.pending_seq = Some(anchor.seq);
        }
        ownership
    };
    records.push(Logged {
        event: event.clone(),
        verified: false,
        published_at: None,
        codex_ownership: Some((*ownership).clone()),
    });
    let after = match fold_records(request.context, &records) {
        Ok(after) => after,
        Err(_) => return Ok(keep()),
    };
    if after.verified.is_some()
        || after.pending.len() + 1 != fold.pending.len()
        || after.pending.iter().any(|p| p.seq == pending.seq)
    {
        return Ok(keep());
    }
    Ok(Planned::CodexAppend(event, ownership))
}
