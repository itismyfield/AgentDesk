use super::{
    CanonicalC, SourceRange, TerminalCoordinate, TerminalCoordinateCandidate, TurnIdentity,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalCoordinateError {
    MissingCanonicalC,
    CanonicalCZero,
    SourceRangeZeroWidth,
    SourceRangeReversed,
    SourceRangeEndMismatch { range_end: u64, canonical_c: u64 },
    MissingResetIdentity,
    EmptyResetIdentity,
    MissingTurnIdentity,
    EmptyTurnStartedAt,
    MissingExternalTurnStartOffset,
}

pub(in crate::services::discord) fn validate_terminal_coordinate_candidate(
    candidate: TerminalCoordinateCandidate<'_>,
) -> Result<TerminalCoordinate<'_>, TerminalCoordinateError> {
    let raw_c = candidate
        .canonical_c
        .ok_or(TerminalCoordinateError::MissingCanonicalC)?;
    let canonical_c = CanonicalC::new(raw_c).ok_or(TerminalCoordinateError::CanonicalCZero)?;

    let source_range = candidate
        .source_range
        .map(|(start, end)| {
            let range = SourceRange::new(start, end).ok_or(if start == end {
                TerminalCoordinateError::SourceRangeZeroWidth
            } else {
                TerminalCoordinateError::SourceRangeReversed
            })?;
            if range.end() != canonical_c.get() {
                return Err(TerminalCoordinateError::SourceRangeEndMismatch {
                    range_end: range.end(),
                    canonical_c: canonical_c.get(),
                });
            }
            Ok(range)
        })
        .transpose()?;

    let reset_identity = candidate
        .reset_identity
        .ok_or(TerminalCoordinateError::MissingResetIdentity)?;
    if reset_identity.is_empty() {
        return Err(TerminalCoordinateError::EmptyResetIdentity);
    }

    let turn_user_message_id = candidate
        .turn_user_message_id
        .ok_or(TerminalCoordinateError::MissingTurnIdentity)?;
    let turn_started_at = candidate
        .turn_started_at
        .ok_or(TerminalCoordinateError::MissingTurnIdentity)?;
    if turn_started_at.is_empty() {
        return Err(TerminalCoordinateError::EmptyTurnStartedAt);
    }
    let turn_identity = if turn_user_message_id == 0 {
        TurnIdentity::External {
            started_at: turn_started_at,
            start_offset: candidate
                .turn_start_offset
                .ok_or(TerminalCoordinateError::MissingExternalTurnStartOffset)?,
        }
    } else {
        TurnIdentity::Message {
            user_message_id: turn_user_message_id,
            started_at: turn_started_at,
            start_offset: candidate.turn_start_offset,
        }
    };

    Ok(TerminalCoordinate {
        canonical_c,
        source_range,
        reset_identity,
        turn_identity,
        route_family: candidate.route_family,
    })
}
