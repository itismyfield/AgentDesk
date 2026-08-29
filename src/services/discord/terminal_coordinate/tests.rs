use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::*;

fn candidate() -> TerminalCoordinateCandidate<'static> {
    TerminalCoordinateCandidate {
        canonical_c: Some(9),
        source_range: Some((3, 9)),
        reset_identity: Some("reset-a"),
        turn_user_message_id: Some(41),
        turn_started_at: Some("2026-08-29T00:00:00Z"),
        route_family: RouteFamily::Watcher,
    }
}

fn hash(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

#[test]
fn canonical_c_accepts_positive_boundaries_and_rejects_c0() {
    assert_eq!(CanonicalC::new(0), None);
    assert_eq!(CanonicalC::new(1).map(CanonicalC::get), Some(1));
    assert_eq!(
        CanonicalC::new(u64::MAX).map(CanonicalC::get),
        Some(u64::MAX)
    );
}

#[test]
fn source_range_requires_strict_order_at_boundaries() {
    assert_eq!(SourceRange::new(0, 0), None);
    assert_eq!(SourceRange::new(1, 0), None);
    assert_eq!(SourceRange::new(u64::MAX, u64::MAX), None);
    let full = SourceRange::new(0, u64::MAX).expect("largest positive range");
    assert_eq!((full.start(), full.end()), (0, u64::MAX));
}

#[test]
fn validator_reports_missing_and_malformed_fields() {
    let mut cases = Vec::new();
    macro_rules! case {
        ($field:ident = $value:expr, $error:expr) => {{
            let mut input = candidate();
            input.$field = $value;
            cases.push((input, $error));
        }};
    }
    case!(
        canonical_c = None,
        TerminalCoordinateError::MissingCanonicalC
    );
    case!(
        canonical_c = Some(0),
        TerminalCoordinateError::CanonicalCZero
    );
    case!(
        source_range = None,
        TerminalCoordinateError::MissingSourceRange
    );
    case!(
        source_range = Some((9, 9)),
        TerminalCoordinateError::SourceRangeZeroWidth
    );
    case!(
        source_range = Some((10, 9)),
        TerminalCoordinateError::SourceRangeReversed
    );
    case!(
        reset_identity = None,
        TerminalCoordinateError::MissingResetIdentity
    );
    case!(
        reset_identity = Some(""),
        TerminalCoordinateError::EmptyResetIdentity
    );
    case!(
        turn_user_message_id = None,
        TerminalCoordinateError::MissingTurnIdentity
    );
    case!(
        turn_user_message_id = Some(0),
        TerminalCoordinateError::TurnUserMessageIdZero
    );
    case!(
        turn_started_at = None,
        TerminalCoordinateError::MissingTurnIdentity
    );
    case!(
        turn_started_at = Some(""),
        TerminalCoordinateError::EmptyTurnStartedAt
    );

    for (input, expected) in cases {
        assert_eq!(validate_terminal_coordinate_candidate(input), Err(expected));
    }

    let accepted = validate_terminal_coordinate_candidate(candidate()).expect("complete candidate");
    assert_eq!(accepted.canonical_c().get(), 9);
}

#[test]
fn range_end_must_equal_separately_supplied_c() {
    let mut input = candidate();
    input.source_range = Some((3, 8));
    assert_eq!(
        validate_terminal_coordinate_candidate(input),
        Err(TerminalCoordinateError::SourceRangeEndMismatch {
            range_end: 8,
            canonical_c: 9,
        })
    );
}

#[test]
fn c_equality_and_hash_ignore_source_start_and_provenance_payload() {
    let first = validate_terminal_coordinate_candidate(candidate()).expect("valid coordinate");
    let mut shifted = candidate();
    shifted.source_range = Some((1, 9));
    shifted.reset_identity = Some("different-provenance");
    shifted.route_family = RouteFamily::Bridge;
    let second = validate_terminal_coordinate_candidate(shifted).expect("valid coordinate");

    assert_eq!(first.canonical_c(), second.canonical_c());
    assert_eq!(hash(first.canonical_c()), hash(second.canonical_c()));
    assert_ne!(first.source_start(), second.source_start());
}

#[test]
fn route_family_match_is_exhaustive() {
    fn ordinal(route: RouteFamily) -> u8 {
        match route {
            RouteFamily::Watcher => 1,
            RouteFamily::Bridge => 2,
            RouteFamily::SessionSink => 3,
        }
    }
    assert_eq!(
        [
            RouteFamily::Watcher,
            RouteFamily::Bridge,
            RouteFamily::SessionSink
        ]
        .map(ordinal),
        [1, 2, 3]
    );
}

#[test]
fn validator_has_no_production_callers() {
    let module = include_str!("mod.rs");
    let validation = include_str!("validation.rs");
    let discord_parent = include_str!("../mod.rs");
    let symbol = ["validate_terminal_coordinate", "_candidate"].concat();

    assert_eq!(module.matches(&symbol).count(), 1, "re-export only");
    assert_eq!(validation.matches(&symbol).count(), 1, "definition only");
    assert!(!discord_parent.contains(&symbol));
}
