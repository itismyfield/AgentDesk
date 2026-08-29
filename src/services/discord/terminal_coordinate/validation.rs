use super::{CanonicalC, SourceRange, TerminalCoordinate, TerminalCoordinateCandidate};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalCoordinateError {
    MissingCanonicalC,
    CanonicalCZero,
    MissingSourceRange,
    SourceRangeZeroWidth,
    SourceRangeReversed,
    SourceRangeEndMismatch { range_end: u64, canonical_c: u64 },
    MissingResetIdentity,
    EmptyResetIdentity,
    MissingTurnIdentity,
    TurnUserMessageIdZero,
    EmptyTurnStartedAt,
}

pub(in crate::services::discord) fn validate_terminal_coordinate_candidate(
    candidate: TerminalCoordinateCandidate<'_>,
) -> Result<TerminalCoordinate<'_>, TerminalCoordinateError> {
    let raw_c = candidate
        .canonical_c
        .ok_or(TerminalCoordinateError::MissingCanonicalC)?;
    let canonical_c = CanonicalC::new(raw_c).ok_or(TerminalCoordinateError::CanonicalCZero)?;

    let (start, end) = candidate
        .source_range
        .ok_or(TerminalCoordinateError::MissingSourceRange)?;
    let source_range = SourceRange::new(start, end).ok_or(if start == end {
        TerminalCoordinateError::SourceRangeZeroWidth
    } else {
        TerminalCoordinateError::SourceRangeReversed
    })?;
    if source_range.end() != canonical_c.get() {
        return Err(TerminalCoordinateError::SourceRangeEndMismatch {
            range_end: source_range.end(),
            canonical_c: canonical_c.get(),
        });
    }

    let reset_identity = candidate
        .reset_identity
        .ok_or(TerminalCoordinateError::MissingResetIdentity)?;
    if reset_identity.is_empty() {
        return Err(TerminalCoordinateError::EmptyResetIdentity);
    }

    let turn_user_message_id = candidate
        .turn_user_message_id
        .ok_or(TerminalCoordinateError::MissingTurnIdentity)?;
    if turn_user_message_id == 0 {
        return Err(TerminalCoordinateError::TurnUserMessageIdZero);
    }
    let turn_started_at = candidate
        .turn_started_at
        .ok_or(TerminalCoordinateError::MissingTurnIdentity)?;
    if turn_started_at.is_empty() {
        return Err(TerminalCoordinateError::EmptyTurnStartedAt);
    }

    Ok(TerminalCoordinate {
        canonical_c,
        source_start: source_range.start(),
        reset_identity,
        turn_user_message_id,
        turn_started_at,
        route_family: candidate.route_family,
    })
}
