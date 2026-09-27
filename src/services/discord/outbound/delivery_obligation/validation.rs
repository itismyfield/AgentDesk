use super::schema::{ExactRange, ObligationLedger, WholeCommit};
use super::state::{BlockedReason, SourceIdentityState, SourceObs};

pub(in crate::services::discord) fn source_identity(
    frontier: &WholeCommit,
    source: SourceObs,
) -> Result<SourceIdentityState, BlockedReason> {
    let bound = match (frontier.source_dev, frontier.source_ino) {
        (None, None) => None,
        (Some(dev), Some(ino)) => Some((dev, ino)),
        _ => return Err(BlockedReason::IdentityIncomplete),
    };
    Ok(match (bound, source) {
        (_, SourceObs::Unavailable) => SourceIdentityState::SourceUnavailable,
        (None, _) => SourceIdentityState::LegacyUnbound,
        (Some(pair), SourceObs::Available { token, .. }) => {
            if pair == (token.source_dev, token.source_ino)
                && frontier.commit.generation_mtime_ns == token.generation_mtime_ns
            {
                SourceIdentityState::BoundAndCurrent(token)
            } else {
                SourceIdentityState::BoundButChanged(token)
            }
        }
    })
}

pub(super) fn publication(
    ledger: &ObligationLedger,
    frontier: Option<&WholeCommit>,
    source: SourceObs,
) -> Result<(), BlockedReason> {
    let SourceObs::Available {
        token,
        size,
        publication,
    } = source
    else {
        return Err(BlockedReason::SourceUnavailable);
    };
    let p = ledger.publication;
    let fits = |range: ExactRange| range.0 < range.1 && range.1 <= p.extent_end;
    let commit_fits = |commit: &WholeCommit| {
        fits(commit.commit.range)
            && commit.commit.generation_mtime_ns == p.epoch.generation_mtime_ns
            && matches!(
                source_identity(commit, source),
                Ok(SourceIdentityState::LegacyUnbound | SourceIdentityState::BoundAndCurrent(_))
            )
    };
    if p.rev == 0
        || p.epoch != token
        || p.extent_end > size
        || publication != Some(p)
        || frontier.is_some_and(|commit| !commit_fits(commit))
        || ledger.held.iter().any(|commit| !commit_fits(commit))
        || ledger.intents.iter().any(|&range| !fits(range))
        || ledger.open.iter().any(|open| {
            !fits(open.range)
                || !fits(open.origin)
                || open.origin.0 > open.range.0
                || open.origin.1 < open.range.1
                || open.attempt.as_ref().is_some_and(|attempt| {
                    !fits(attempt.range)
                        || attempt.key.is_empty()
                        || match (&attempt.chunk_nonces, attempt.chunk_total) {
                            (Some(nonces), Some(total)) => {
                                total == 0
                                    || nonces.len() != total as usize
                                    || nonces.iter().any(String::is_empty)
                                    || attempt
                                        .receipts
                                        .iter()
                                        .any(|r| r.chunk >= total || r.message_id == 0)
                            }
                            (None, None) => false,
                            _ => true,
                        }
                })
                || open.redrive_capped.as_ref().is_some_and(|cap| {
                    cap.range != open.range
                        || cap.next_rearm_at_ms < cap.capped_at_ms
                        || cap.last_rejection.is_empty()
                })
        })
    {
        return Err(BlockedReason::PublicationMismatch);
    }
    Ok(())
}
