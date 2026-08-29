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
        turn_start_offset: Some(3),
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
fn optional_source_range_requires_strict_order_when_present() {
    assert_eq!(SourceRange::new(0, 0), None);
    assert_eq!(SourceRange::new(1, 0), None);
    assert_eq!(SourceRange::new(u64::MAX, u64::MAX), None);
    let full = SourceRange::new(0, u64::MAX).expect("largest positive range");
    assert_eq!((full.start(), full.end()), (0, u64::MAX));

    let mut without_range = candidate();
    without_range.source_range = None;
    let accepted = validate_terminal_coordinate_candidate(without_range)
        .expect("SessionSink may carry authoritative C without source provenance");
    assert_eq!(accepted.source_range(), None);
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
fn present_range_end_must_equal_separately_supplied_c() {
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
fn canonical_c_is_independent_of_optional_source_provenance() {
    let first = validate_terminal_coordinate_candidate(candidate()).expect("valid coordinate");
    let mut shifted = candidate();
    shifted.source_range = Some((1, 9));
    shifted.reset_identity = Some("different-provenance");
    shifted.route_family = RouteFamily::Bridge;
    let second = validate_terminal_coordinate_candidate(shifted).expect("valid coordinate");
    let mut absent = candidate();
    absent.source_range = None;
    let third = validate_terminal_coordinate_candidate(absent).expect("C-only coordinate");

    assert_eq!(first.canonical_c(), second.canonical_c());
    assert_eq!(first.canonical_c(), third.canonical_c());
    assert_eq!(hash(first.canonical_c()), hash(second.canonical_c()));
    assert_eq!(hash(first.canonical_c()), hash(third.canonical_c()));
    assert_ne!(first.source_range(), second.source_range());
    assert_ne!(first.source_range(), third.source_range());
}

#[test]
fn external_turn_requires_offset_and_uses_it_as_identity() {
    let same_time = "2026-08-29T00:00:00Z";
    let mut missing = candidate();
    missing.turn_user_message_id = Some(0);
    missing.turn_started_at = Some(same_time);
    missing.turn_start_offset = None;
    assert_eq!(
        validate_terminal_coordinate_candidate(missing),
        Err(TerminalCoordinateError::MissingExternalTurnStartOffset)
    );

    let mut first = missing;
    first.turn_start_offset = Some(0);
    let first = validate_terminal_coordinate_candidate(first).expect("external turn with offset");
    let mut second = missing;
    second.turn_start_offset = Some(4096);
    let second = validate_terminal_coordinate_candidate(second).expect("next external turn");

    assert_eq!(
        first.turn_identity(),
        TurnIdentity::External {
            started_at: same_time,
            start_offset: 0,
        }
    );
    assert_ne!(first.turn_identity(), second.turn_identity());
    assert_ne!(hash(first.turn_identity()), hash(second.turn_identity()));
}

#[test]
fn message_turn_retains_started_at_and_optional_offset() {
    let mut without_offset = candidate();
    without_offset.turn_start_offset = None;
    let without_offset = validate_terminal_coordinate_candidate(without_offset)
        .expect("nonzero message identity does not fabricate an offset");
    assert_eq!(
        without_offset.turn_identity(),
        TurnIdentity::Message {
            user_message_id: 41,
            started_at: "2026-08-29T00:00:00Z",
            start_offset: None,
        }
    );

    let with_offset = validate_terminal_coordinate_candidate(candidate())
        .expect("nonzero message identity retains a supplied offset");
    assert_eq!(
        with_offset.turn_identity(),
        TurnIdentity::Message {
            user_message_id: 41,
            started_at: "2026-08-29T00:00:00Z",
            start_offset: Some(3),
        }
    );
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
