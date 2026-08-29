//! Dormant terminal-coordinate value substrate for #5191 S2c S1a.
//!
//! This module is intentionally private and has no re-export. Production code
//! compiles these values but cannot name them outside this module. The later
//! wiring change must deliberately widen this boundary (through one reviewed
//! facade) before any terminal route can construct a coordinate.

use std::num::NonZeroU64;

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct CanonicalC(NonZeroU64);

impl CanonicalC {
    #[allow(dead_code)]
    fn get(self) -> u64 {
        self.0.get()
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceRange {
    start: u64,
    end: u64,
}

impl SourceRange {
    #[allow(dead_code)]
    fn start(self) -> u64 {
        self.start
    }

    #[allow(dead_code)]
    fn end(self) -> u64 {
        self.end
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum RouteFamily {
    Sink,
    Watcher,
    Controller,
    Recovery,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TurnIdentity {
    Message {
        user_message_id: NonZeroU64,
        started_at: String,
        start_offset: Option<u64>,
    },
    External {
        started_at: String,
        start_offset: u64,
    },
}

impl TurnIdentity {
    #[allow(dead_code)]
    fn message(
        user_message_id: u64,
        started_at: impl Into<String>,
        start_offset: Option<u64>,
    ) -> Result<Self, TerminalCoordinateError> {
        let user_message_id =
            NonZeroU64::new(user_message_id).ok_or(TerminalCoordinateError::MessageUserIdZero)?;
        let started_at = started_at.into();
        validate_started_at(&started_at)?;
        Ok(Self::Message {
            user_message_id,
            started_at,
            start_offset,
        })
    }

    #[allow(dead_code)]
    fn external(
        started_at: impl Into<String>,
        start_offset: u64,
    ) -> Result<Self, TerminalCoordinateError> {
        let started_at = started_at.into();
        validate_started_at(&started_at)?;
        Ok(Self::External {
            started_at,
            start_offset,
        })
    }

    #[allow(dead_code)]
    fn user_message_id(&self) -> u64 {
        match self {
            Self::Message {
                user_message_id, ..
            } => user_message_id.get(),
            Self::External { .. } => 0,
        }
    }

    #[allow(dead_code)]
    fn started_at(&self) -> &str {
        match self {
            Self::Message { started_at, .. } | Self::External { started_at, .. } => started_at,
        }
    }

    #[allow(dead_code)]
    fn start_offset(&self) -> Option<u64> {
        match self {
            Self::Message { start_offset, .. } => *start_offset,
            Self::External { start_offset, .. } => Some(*start_offset),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalCoordinate {
    canonical_c: CanonicalC,
    source_range: Option<SourceRange>,
    reset_incarnation: u64,
    turn_identity: TurnIdentity,
    route_family: RouteFamily,
}

impl TerminalCoordinate {
    #[allow(dead_code)]
    fn new(
        canonical_c: u64,
        source_range: Option<(u64, u64)>,
        reset_incarnation: u64,
        turn_identity: TurnIdentity,
        route_family: RouteFamily,
    ) -> Result<Self, TerminalCoordinateError> {
        let canonical_c = NonZeroU64::new(canonical_c)
            .map(CanonicalC)
            .ok_or(TerminalCoordinateError::CanonicalCZero)?;
        let source_range = match source_range {
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
        Ok(Self {
            canonical_c,
            source_range,
            reset_incarnation,
            turn_identity,
            route_family,
        })
    }

    #[allow(dead_code)]
    fn canonical_c(&self) -> CanonicalC {
        self.canonical_c
    }

    #[allow(dead_code)]
    fn source_range(&self) -> Option<SourceRange> {
        self.source_range
    }

    #[allow(dead_code)]
    fn reset_incarnation(&self) -> u64 {
        self.reset_incarnation
    }

    #[allow(dead_code)]
    fn turn_identity(&self) -> &TurnIdentity {
        &self.turn_identity
    }

    #[allow(dead_code)]
    fn route_family(&self) -> RouteFamily {
        self.route_family
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalCoordinateError {
    CanonicalCZero,
    SourceRangeNotStrict { start: u64, end: u64 },
    SourceRangeEndMismatch { end: u64, canonical_c: u64 },
    MessageUserIdZero,
    StartedAtEmpty,
}

fn validate_started_at(started_at: &str) -> Result<(), TerminalCoordinateError> {
    if started_at.is_empty() {
        Err(TerminalCoordinateError::StartedAtEmpty)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    fn message() -> TurnIdentity {
        TurnIdentity::message(41, "exact-start", Some(7)).unwrap()
    }

    fn coordinate(
        canonical_c: u64,
        source_range: Option<(u64, u64)>,
    ) -> Result<TerminalCoordinate, TerminalCoordinateError> {
        TerminalCoordinate::new(
            canonical_c,
            source_range,
            9,
            message(),
            RouteFamily::Watcher,
        )
    }

    #[test]
    fn canonical_c_is_positive_and_hashes_only_c() {
        assert_eq!(
            coordinate(0, None),
            Err(TerminalCoordinateError::CanonicalCZero)
        );
        assert_eq!(coordinate(1, None).unwrap().canonical_c().get(), 1);
        assert_eq!(
            coordinate(u64::MAX, None).unwrap().canonical_c().get(),
            u64::MAX
        );

        let mut set = HashSet::new();
        set.insert(coordinate(44, None).unwrap().canonical_c());
        assert!(set.contains(&coordinate(44, None).unwrap().canonical_c()));
    }

    #[test]
    fn optional_source_range_is_strict_and_ends_at_c() {
        assert_eq!(coordinate(8, None).unwrap().source_range(), None);
        let ranged = coordinate(8, Some((3, 8))).unwrap();
        assert_eq!(ranged.source_range().map(SourceRange::start), Some(3));
        assert_eq!(ranged.source_range().map(SourceRange::end), Some(8));
        assert_eq!(
            coordinate(8, Some((8, 8))),
            Err(TerminalCoordinateError::SourceRangeNotStrict { start: 8, end: 8 })
        );
        assert_eq!(
            coordinate(8, Some((9, 8))),
            Err(TerminalCoordinateError::SourceRangeNotStrict { start: 9, end: 8 })
        );
        assert_eq!(
            coordinate(8, Some((2, 7))),
            Err(TerminalCoordinateError::SourceRangeEndMismatch {
                end: 7,
                canonical_c: 8,
            })
        );
    }

    #[test]
    fn absent_provenance_remains_valid_and_does_not_change_c_identity() {
        let without = coordinate(12, None).unwrap();
        let with = coordinate(12, Some((4, 12))).unwrap();
        let mut map = HashMap::new();
        map.insert(without.canonical_c(), "terminal");
        assert_eq!(map.get(&with.canonical_c()), Some(&"terminal"));
    }

    #[test]
    fn message_identity_preserves_exact_fields_and_optional_offset() {
        assert_eq!(
            TurnIdentity::message(0, "started", None),
            Err(TerminalCoordinateError::MessageUserIdZero)
        );
        assert_eq!(
            TurnIdentity::message(3, "", None),
            Err(TerminalCoordinateError::StartedAtEmpty)
        );
        let no_offset = TurnIdentity::message(3, "  exact  ", None).unwrap();
        let zero_offset = TurnIdentity::message(3, "  exact  ", Some(0)).unwrap();
        assert_eq!(no_offset.user_message_id(), 3);
        assert_eq!(no_offset.started_at(), "  exact  ");
        assert_eq!(no_offset.start_offset(), None);
        assert_eq!(zero_offset.start_offset(), Some(0));
        assert_ne!(no_offset, zero_offset);
        assert_ne!(
            TurnIdentity::message(3, "same", Some(5)).unwrap(),
            TurnIdentity::message(3, "same", Some(6)).unwrap()
        );
    }

    #[test]
    fn external_identity_is_id_zero_and_requires_offset_in_identity() {
        assert_eq!(
            TurnIdentity::external("", 0),
            Err(TerminalCoordinateError::StartedAtEmpty)
        );
        let zero = TurnIdentity::external("same", 0).unwrap();
        let later = TurnIdentity::external("same", 1).unwrap();
        assert_eq!(zero.user_message_id(), 0);
        assert_eq!(zero.started_at(), "same");
        assert_eq!(zero.start_offset(), Some(0));
        assert_ne!(zero, later);
    }

    #[test]
    fn route_family_is_exhaustive_and_constructor_materializes_every_input() {
        let routes = [
            RouteFamily::Sink,
            RouteFamily::Watcher,
            RouteFamily::Controller,
            RouteFamily::Recovery,
        ];
        assert_eq!(routes.len(), 4);
        for route in routes {
            let identity = TurnIdentity::external("external", 0).unwrap();
            let terminal =
                TerminalCoordinate::new(22, Some((5, 22)), u64::MAX, identity.clone(), route)
                    .unwrap();
            assert_eq!(terminal.canonical_c().get(), 22);
            assert_eq!(terminal.source_range().map(SourceRange::start), Some(5));
            assert_eq!(terminal.reset_incarnation(), u64::MAX);
            assert_eq!(terminal.turn_identity(), &identity);
            assert_eq!(terminal.route_family(), route);
        }
    }
}
