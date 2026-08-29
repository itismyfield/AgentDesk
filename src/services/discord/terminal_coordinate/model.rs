use std::num::NonZeroU64;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) struct CanonicalC(NonZeroU64);

impl CanonicalC {
    #[allow(dead_code)]
    pub(in crate::services::discord) fn new(value: u64) -> Option<Self> {
        NonZeroU64::new(value).map(Self)
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn get(self) -> u64 {
        self.0.get()
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct SourceRange {
    start: u64,
    end: u64,
}

impl SourceRange {
    #[allow(dead_code)]
    pub(in crate::services::discord) fn start(self) -> u64 {
        self.start
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn end(self) -> u64 {
        self.end
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) enum RouteFamily {
    Sink,
    Watcher,
    Controller,
    Recovery,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) enum TurnIdentity {
    Message {
        user_message_id: NonZeroU64,
        started_at: String,
        turn_start_offset: Option<u64>,
    },
    External {
        started_at: String,
        start_offset: u64,
    },
}

impl TurnIdentity {
    #[allow(dead_code)]
    pub(in crate::services::discord) fn message(
        user_message_id: u64,
        started_at: impl Into<String>,
        turn_start_offset: Option<u64>,
    ) -> Result<Self, TerminalCoordinateError> {
        let user_message_id =
            NonZeroU64::new(user_message_id).ok_or(TerminalCoordinateError::MessageUserIdZero)?;
        let started_at = started_at.into();
        if started_at.is_empty() {
            return Err(TerminalCoordinateError::StartedAtEmpty);
        }
        Ok(Self::Message {
            user_message_id,
            started_at,
            turn_start_offset,
        })
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn external(
        started_at: impl Into<String>,
        start_offset: u64,
    ) -> Result<Self, TerminalCoordinateError> {
        let started_at = started_at.into();
        if started_at.is_empty() {
            return Err(TerminalCoordinateError::StartedAtEmpty);
        }
        Ok(Self::External {
            started_at,
            start_offset,
        })
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn user_message_id(&self) -> u64 {
        match self {
            Self::Message {
                user_message_id, ..
            } => user_message_id.get(),
            Self::External { .. } => 0,
        }
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn started_at(&self) -> &str {
        match self {
            Self::Message { started_at, .. } | Self::External { started_at, .. } => started_at,
        }
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn start_offset(&self) -> Option<u64> {
        match self {
            Self::Message {
                turn_start_offset, ..
            } => *turn_start_offset,
            Self::External { start_offset, .. } => Some(*start_offset),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TerminalCoordinateCandidate {
    canonical_c: u64,
    source_range: Option<(u64, u64)>,
    reset_incarnation: u64,
    turn_identity: TurnIdentity,
    route_family: RouteFamily,
}

impl TerminalCoordinateCandidate {
    #[allow(dead_code)]
    pub(in crate::services::discord) fn new(
        canonical_c: u64,
        source_range: Option<(u64, u64)>,
        reset_incarnation: u64,
        turn_identity: TurnIdentity,
        route_family: RouteFamily,
    ) -> Self {
        Self {
            canonical_c,
            source_range,
            reset_incarnation,
            turn_identity,
            route_family,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TerminalCoordinate {
    canonical_c: CanonicalC,
    source_range: Option<SourceRange>,
    reset_incarnation: u64,
    turn_identity: TurnIdentity,
    route_family: RouteFamily,
}

impl TerminalCoordinate {
    #[allow(dead_code)]
    pub(in crate::services::discord) fn canonical_c(&self) -> CanonicalC {
        self.canonical_c
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn source_range(&self) -> Option<SourceRange> {
        self.source_range
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn reset_incarnation(&self) -> u64 {
        self.reset_incarnation
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn turn_identity(&self) -> &TurnIdentity {
        &self.turn_identity
    }

    #[allow(dead_code)]
    pub(in crate::services::discord) fn route_family(&self) -> RouteFamily {
        self.route_family
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalCoordinateError {
    CanonicalCZero,
    SourceRangeNotStrict { start: u64, end: u64 },
    SourceRangeEndMismatch { end: u64, canonical_c: u64 },
    MessageUserIdZero,
    StartedAtEmpty,
}

#[allow(dead_code)]
pub(in crate::services::discord) fn validate_terminal_coordinate(
    candidate: TerminalCoordinateCandidate,
) -> Result<TerminalCoordinate, TerminalCoordinateError> {
    let canonical_c =
        CanonicalC::new(candidate.canonical_c).ok_or(TerminalCoordinateError::CanonicalCZero)?;
    let source_range = match candidate.source_range {
        None => None,
        Some((start, end)) if start >= end => {
            return Err(TerminalCoordinateError::SourceRangeNotStrict { start, end });
        }
        Some((_, end)) if end != canonical_c.get() => {
            return Err(TerminalCoordinateError::SourceRangeEndMismatch {
                end,
                canonical_c: canonical_c.get(),
            });
        }
        Some((start, end)) => Some(SourceRange { start, end }),
    };

    Ok(TerminalCoordinate {
        canonical_c,
        source_range,
        reset_incarnation: candidate.reset_incarnation,
        turn_identity: candidate.turn_identity,
        route_family: candidate.route_family,
    })
}
