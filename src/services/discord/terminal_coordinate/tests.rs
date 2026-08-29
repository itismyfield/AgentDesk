use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::*;

fn candidate() -> TerminalCoordinateCandidate<'static> {
    TerminalCoordinateCandidate::new(
        Some(9),
        Some((3, 9)),
        Some("reset-a"),
        Some(41),
        Some("2026-08-29T00:00:00Z"),
        Some(3),
        RouteFamily::Watcher,
    )
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
fn sibling_api_constructs_candidate_and_reads_coordinate_identity() {
    let input = TerminalCoordinateCandidate::new(
        Some(17),
        None,
        Some("reset-sibling"),
        Some(91),
        Some("2026-08-29T00:01:00Z"),
        None,
        RouteFamily::SessionSink,
    );
    let coordinate = validate_terminal_coordinate_candidate(input).expect("sibling-style API");

    assert_eq!(coordinate.canonical_c().get(), 17);
    assert_eq!(coordinate.source_range(), None);
    assert_eq!(coordinate.reset_identity(), "reset-sibling");
    assert_eq!(coordinate.route_family(), RouteFamily::SessionSink);
    assert_eq!(
        coordinate.turn_identity(),
        TurnIdentity::Message {
            user_message_id: 91,
            started_at: "2026-08-29T00:01:00Z",
            start_offset: None,
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

fn collect_rust_files(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read Discord source directory") {
        let path = entry.expect("Discord source entry").path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn raw_string_start(characters: &[char], index: usize) -> Option<(usize, usize)> {
    let mut cursor = index;
    if matches!(characters.get(cursor), Some('b' | 'c')) {
        cursor += 1;
    }
    if characters.get(cursor) != Some(&'r') {
        return None;
    }
    cursor += 1;
    let hash_start = cursor;
    while characters.get(cursor) == Some(&'#') {
        cursor += 1;
    }
    (characters.get(cursor) == Some(&'"')).then_some((cursor - hash_start, cursor + 1))
}

fn rust_identifiers(source: &str) -> Vec<String> {
    #[derive(Clone, Copy)]
    enum State {
        Code,
        LineComment,
        BlockComment(usize),
        String,
        RawString(usize),
    }

    let characters: Vec<char> = source.chars().collect();
    let mut state = State::Code;
    let mut identifiers = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if matches!(state, State::Code) {
            if let Some((hashes, after_opening_quote)) = raw_string_start(&characters, index) {
                state = State::RawString(hashes);
                index = after_opening_quote;
                continue;
            }
        }
        match state {
            State::Code if characters[index] == '/' && characters.get(index + 1) == Some(&'/') => {
                state = State::LineComment;
                index += 2;
            }
            State::Code if characters[index] == '/' && characters.get(index + 1) == Some(&'*') => {
                state = State::BlockComment(1);
                index += 2;
            }
            State::Code if characters[index] == '"' => {
                state = State::String;
                index += 1;
            }
            State::Code if characters[index].is_ascii_alphabetic() || characters[index] == '_' => {
                let start = index;
                index += 1;
                while index < characters.len()
                    && (characters[index].is_ascii_alphanumeric() || characters[index] == '_')
                {
                    index += 1;
                }
                identifiers.push(characters[start..index].iter().collect());
            }
            State::Code => index += 1,
            State::LineComment if characters[index] == '\n' => {
                state = State::Code;
                index += 1;
            }
            State::LineComment => index += 1,
            State::BlockComment(depth)
                if characters[index] == '/' && characters.get(index + 1) == Some(&'*') =>
            {
                state = State::BlockComment(depth + 1);
                index += 2;
            }
            State::BlockComment(depth)
                if characters[index] == '*' && characters.get(index + 1) == Some(&'/') =>
            {
                state = if depth == 1 {
                    State::Code
                } else {
                    State::BlockComment(depth - 1)
                };
                index += 2;
            }
            State::BlockComment(_) => index += 1,
            State::String if characters[index] == '\\' => index += 2,
            State::String if characters[index] == '"' => {
                state = State::Code;
                index += 1;
            }
            State::String => index += 1,
            State::RawString(hashes)
                if characters[index] == '"'
                    && characters.get(index + 1..index + 1 + hashes)
                        == Some(vec!['#'; hashes].as_slice()) =>
            {
                state = State::Code;
                index += hashes + 1;
            }
            State::RawString(_) => index += 1,
        }
    }
    identifiers
}

#[test]
fn identifier_census_ignores_comments_and_string_literals() {
    let source = r####"
        // TerminalCoordinateCandidate
        /* validate_terminal_coordinate_candidate */
        const NORMAL: &str = "TerminalCoordinateCandidate";
        const RAW: &str = r###"validate_terminal_coordinate_candidate \" nested"###;
        TerminalCoordinateCandidate::new
    "####;
    assert_eq!(
        rust_identifiers(source)
            .into_iter()
            .filter(|identifier| {
                matches!(
                    identifier.as_str(),
                    "TerminalCoordinateCandidate" | "validate_terminal_coordinate_candidate"
                )
            })
            .collect::<Vec<_>>(),
        ["TerminalCoordinateCandidate"]
    );
}

#[test]
fn validator_and_candidate_have_no_sibling_production_use() {
    let discord_root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/services/discord");
    let coordinate_root = discord_root.join("terminal_coordinate");
    let mut files = Vec::new();
    collect_rust_files(&discord_root, &mut files);

    let mut violations = Vec::new();
    for path in files {
        if path.starts_with(&coordinate_root) {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read Discord Rust source");
        let identifiers = rust_identifiers(&source);
        for forbidden in [
            "validate_terminal_coordinate_candidate",
            "TerminalCoordinateCandidate",
        ] {
            if identifiers.iter().any(|identifier| identifier == forbidden) {
                violations.push(format!(
                    "{} uses {forbidden}",
                    path.strip_prefix(&discord_root)
                        .expect("Discord-relative source path")
                        .display()
                ));
            }
        }
    }

    violations.sort();
    assert!(
        violations.is_empty(),
        "dormant coordinate substrate gained sibling production use: {violations:?}"
    );
}
