#![allow(dead_code)] // #5191 S2c S1a: dormant until a later authority slice wires callers.

mod validation;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) struct CanonicalC(u64);

impl CanonicalC {
    pub(in crate::services::discord) const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    pub(in crate::services::discord) const fn get(self) -> u64 {
        self.0
    }
}

/// Observation-only provenance. It has no lease, progress, or frontier authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct SourceRange {
    start: u64,
    end: u64,
}

impl SourceRange {
    pub(in crate::services::discord) const fn new(start: u64, end: u64) -> Option<Self> {
        if start < end {
            Some(Self { start, end })
        } else {
            None
        }
    }

    pub(in crate::services::discord) const fn start(self) -> u64 {
        self.start
    }

    pub(in crate::services::discord) const fn end(self) -> u64 {
        self.end
    }
}

/// Closed identity vocabulary only; this slice adds no production conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) enum RouteFamily {
    Watcher,
    Bridge,
    SessionSink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct TerminalCoordinateCandidate<'a> {
    canonical_c: Option<u64>,
    source_range: Option<(u64, u64)>,
    reset_identity: Option<&'a str>,
    turn_user_message_id: Option<u64>,
    turn_started_at: Option<&'a str>,
    route_family: RouteFamily,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(in crate::services::discord) struct TerminalCoordinate<'a> {
    canonical_c: CanonicalC,
    source_start: u64,
    reset_identity: &'a str,
    turn_user_message_id: u64,
    turn_started_at: &'a str,
    route_family: RouteFamily,
}

impl<'a> TerminalCoordinate<'a> {
    pub(in crate::services::discord) const fn canonical_c(self) -> CanonicalC {
        self.canonical_c
    }

    pub(in crate::services::discord) const fn source_start(self) -> u64 {
        self.source_start
    }
}

pub(in crate::services::discord) use validation::{
    TerminalCoordinateError, validate_terminal_coordinate_candidate,
};
