mod validation;

#[cfg(test)]
mod tests;

#[allow(dead_code)] // #5191 S2c S1a: dormant coordinate value; production callers land later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) struct CanonicalC(u64);

impl CanonicalC {
    #[allow(dead_code)] // #5191 S2c S1a: dormant constructor; production callers land later.
    pub(in crate::services::discord) const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn get(self) -> u64 {
        self.0
    }
}

/// Optional observation-only provenance. It has no lease, progress, or frontier authority.
#[allow(dead_code)] // #5191 S2c S1a: dormant provenance descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) struct SourceRange {
    start: u64,
    end: u64,
}

impl SourceRange {
    #[allow(dead_code)] // #5191 S2c S1a: dormant constructor; production callers land later.
    pub(in crate::services::discord) const fn new(start: u64, end: u64) -> Option<Self> {
        if start < end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn start(self) -> u64 {
        self.start
    }

    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn end(self) -> u64 {
        self.end
    }
}

/// Closed identity vocabulary only; this slice adds no production conversion.
#[allow(dead_code)] // #5191 S2c S1a: dormant route vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) enum RouteFamily {
    Watcher,
    Bridge,
    SessionSink,
}

#[allow(dead_code)] // #5191 S2c S1a: dormant input boundary for later sibling callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TerminalCoordinateCandidate<'a> {
    canonical_c: Option<u64>,
    source_range: Option<(u64, u64)>,
    reset_identity: Option<&'a str>,
    turn_user_message_id: Option<u64>,
    turn_started_at: Option<&'a str>,
    turn_start_offset: Option<u64>,
    route_family: RouteFamily,
}

impl<'a> TerminalCoordinateCandidate<'a> {
    #[allow(dead_code)] // #5191 S2c S1a: explicit dormant API, no hidden defaults or fabrication.
    #[allow(clippy::too_many_arguments)]
    pub(in crate::services::discord) const fn new(
        canonical_c: Option<u64>,
        source_range: Option<(u64, u64)>,
        reset_identity: Option<&'a str>,
        turn_user_message_id: Option<u64>,
        turn_started_at: Option<&'a str>,
        turn_start_offset: Option<u64>,
        route_family: RouteFamily,
    ) -> Self {
        Self {
            canonical_c,
            source_range,
            reset_identity,
            turn_user_message_id,
            turn_started_at,
            turn_start_offset,
            route_family,
        }
    }
}

#[allow(dead_code)] // #5191 S2c S1a: dormant typed turn identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) enum TurnIdentity<'a> {
    Message {
        user_message_id: u64,
        started_at: &'a str,
        start_offset: Option<u64>,
    },
    External {
        started_at: &'a str,
        start_offset: u64,
    },
}

#[allow(dead_code)] // #5191 S2c S1a: dormant validated coordinate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) struct TerminalCoordinate<'a> {
    canonical_c: CanonicalC,
    source_range: Option<SourceRange>,
    reset_identity: &'a str,
    turn_identity: TurnIdentity<'a>,
    route_family: RouteFamily,
}

impl<'a> TerminalCoordinate<'a> {
    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn canonical_c(self) -> CanonicalC {
        self.canonical_c
    }

    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn source_range(self) -> Option<SourceRange> {
        self.source_range
    }

    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn reset_identity(self) -> &'a str {
        self.reset_identity
    }

    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn turn_identity(self) -> TurnIdentity<'a> {
        self.turn_identity
    }

    #[allow(dead_code)] // #5191 S2c S1a: dormant accessor; production callers land later.
    pub(in crate::services::discord) const fn route_family(self) -> RouteFamily {
        self.route_family
    }
}

pub(in crate::services::discord) use validation::{
    TerminalCoordinateError, validate_terminal_coordinate_candidate,
};
