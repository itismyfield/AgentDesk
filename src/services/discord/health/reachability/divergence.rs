//! Row coordinate ↔ independently resolved coordinate comparison — 4987 §-1.5
//! divergence, S4 (#5071 T4-B4).
//!
//! #4986 형상1 is one channel whose in-flight row and live watcher registry
//! named different transcripts: the row's `output_path` was ENOENT while the
//! registry's file was alive and growing. 4987 redefines divergence for that
//! shape as a comparison of file **identity** — [`TranscriptFileId`], the
//! `(dev, ino)` pair — never of path strings:
//!
//! * §-1.4 counterexample 4 is a wrapper and a native transcript of EQUAL
//!   size and different inode, which a size or string comparison calls equal;
//! * the same file reached through a symlink or a `..` alias differs as a
//!   string and must not read as divergence;
//! * "one side opens, the other does not" holds whether or not the strings
//!   are equal (4987 §-1.3), so the dead row path beside the live registry
//!   file is its own outcome rather than a string case.
//!
//! # This module decides nothing
//!
//! Every outcome is a descriptive attribute (4987 §9.2 S4). Nothing here
//! produces a `ReachabilityVerdict`, feeds `RelayStallState`, or authorizes
//! recovery, redelivery, or any destructive action;
//! [`RowCoordinateDivergence::unknown_reason`] only names the
//! [`ReachabilityUnknownReason`] the T4-B6 composition may spell from a
//! non-GREEN outcome. The in-flight row's path enters this tree here and only
//! here, as a comparison operand (4987 I14): obligation production does not
//! read it, and `obligation`/`ledger` must never depend on this module.
//!
//! # Fail-closed, and the one stat failure that is a signal
//!
//! Two different things live here, and calling both of them "fail-closed"
//! would misdescribe what the code does:
//!
//! * **The independently resolved side fails to stat** (or both sides do).
//!   An equality claim between two identities needs both identities in hand,
//!   so `(Unresolvable | Resolved(_), Unresolvable)` is
//!   [`RowCoordinateDivergence::Unknown`], maps to no reason, and stays
//!   silent — `discovery`'s fail-closed discipline. (A row that offered no
//!   coordinate at all is [`RowCoordinateDivergence::NoRowCoordinate`],
//!   regardless of the independent side.)
//! * **The row side fails to stat while the independent side is alive.** This
//!   is not a retreat, it is the designed detection. `(Unresolvable,
//!   Resolved)` is
//!   [`RowCoordinateDivergence::RowPathUnresolvableWhileRegistryLive`]:
//!   [`RowCoordinateDivergence::is_non_green_signal`] answers `true`, the
//!   `reachability_row_coordinate_divergence` record fires, and
//!   [`RowCoordinateDivergence::unknown_reason`] yields
//!   `TranscriptCoordinateDivergence`. 4987 §6.2's 검출표 #1 (:900) asks for
//!   exactly this shape at 1 tick as the 모순의 직접 관측, and the same section's
//!   mutation test (:926-936) specifies it. Its verdict type is
//!   `Unknown{reason}`, which is **not** `Reachable` (4987:699) — that
//!   `Unknown` is the verdict's name for "no obligation set is computable",
//!   and must not be read as the silent
//!   [`RowCoordinateDivergence::Unknown`] outcome above.
//!
//! Which stat errors reach the second bullet is deliberately not narrowed:
//! the design says "stat 실패" without qualification, and `stat_transcript`
//! answers `None` for ENOENT, for a directory, for a broken symlink, and for
//! EACCES alike. A transcript under a directory whose search bit was dropped
//! raises `PermissionDenied` (errno 13) from `fs::metadata` and therefore
//! fires this signal; the comparison has no permission-vs-absence
//! distinction to offer, and does not pretend to.

use std::path::Path;

use super::discovery::{TranscriptFileId, stat_transcript};
use super::verdict::ReachabilityUnknownReason;

/// One comparison operand, as data: what a single stat observed. Produced by
/// [`CoordinateObservation::observe`] (the I/O adapter) and consumed by
/// [`divergence`] (the pure comparison), so the §9.4 pure/I-O split is a
/// module seam rather than a convention inside one function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CoordinateObservation {
    /// The caller had no path to offer for this operand.
    NotOffered,
    /// A path was offered but did not stat to an existing regular file
    /// (missing, EACCES, a directory, a broken symlink). Readability is not
    /// part of this — see [`stat_transcript`].
    Unresolvable,
    /// The path resolved to a regular file with this identity.
    Resolved(TranscriptFileId),
}

impl CoordinateObservation {
    /// Stat one operand. Blank and whitespace-only paths are `NotOffered` —
    /// the reading `resolve_bound_selector` gives them: a row that never
    /// carried a path is not a row whose path failed to stat.
    pub(in crate::services::discord) fn observe(path: Option<&str>) -> Self {
        let Some(path) = path.map(str::trim).filter(|path| !path.is_empty()) else {
            return Self::NotOffered;
        };
        match stat_transcript(Path::new(path)) {
            Some(stat) => Self::Resolved(stat.file_id),
            None => Self::Unresolvable,
        }
    }
}

/// The outcome of comparing the in-flight row's transcript coordinate against
/// the independently resolved one. Descriptive only — see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum RowCoordinateDivergence {
    /// No row coordinate was offered. Not a defect for this comparison:
    /// obligation production is row-independent (I14), and a live turn with no
    /// row at all is `RowlessActiveTurn`'s business, not divergence's.
    NoRowCoordinate,
    /// A row coordinate was offered but no independently resolved coordinate
    /// was (typically: no live watcher binding). Nothing to compare against.
    NoIndependentCoordinate,
    /// Both coordinates name the same file identity. The path strings may
    /// still differ (symlink, `..` alias) — that is agreement, not divergence.
    SameFile,
    /// Both coordinates resolved, to different files: the #4986 형상1
    /// contradiction observed directly, immune to the equal-size mask of
    /// §-1.4 counterexample 4 because size is not part of identity.
    Diverged,
    /// The row's path does not stat while the independently resolved
    /// transcript is alive — #4986 형상1's exact shape, 4987 §-1.4's
    /// `RowPathUnresolvableWhileRegistryLive` derived signal.
    RowPathUnresolvableWhileRegistryLive,
    /// The independently resolved side (or both sides) failed to stat, so no
    /// comparison could be made. Never asserted as divergence.
    Unknown,
}

impl RowCoordinateDivergence {
    pub(in crate::services::discord) const fn as_str(self) -> &'static str {
        match self {
            Self::NoRowCoordinate => "no_row_coordinate",
            Self::NoIndependentCoordinate => "no_independent_coordinate",
            Self::SameFile => "same_file",
            Self::Diverged => "diverged",
            Self::RowPathUnresolvableWhileRegistryLive => {
                "row_path_unresolvable_while_registry_live"
            }
            Self::Unknown => "unknown",
        }
    }

    /// Whether this outcome is one of the two 4987 §-1.4 non-GREEN shapes.
    /// It marks; it does not act — `true` is not permission for redelivery,
    /// recovery, or any destructive step (4987 §7.1 / I15).
    pub(in crate::services::discord) const fn is_non_green_signal(self) -> bool {
        // Spelled out rather than collapsed so a new variant must choose a
        // side here before it compiles (`authorizes_redelivery`'s device).
        match self {
            Self::Diverged | Self::RowPathUnresolvableWhileRegistryLive => true,
            Self::NoRowCoordinate
            | Self::NoIndependentCoordinate
            | Self::SameFile
            | Self::Unknown => false,
        }
    }

    /// The [`ReachabilityUnknownReason`] a non-GREEN outcome maps to when the
    /// T4-B6 composition spells it. The two shapes arrive here on different
    /// warrants, and only one of them is named by a test in the design:
    /// `RowPathUnresolvableWhileRegistryLive` is the shape 4987 §6.2's
    /// mutation test (:926-936) writes out — an unresolvable row path beside a
    /// live resolved transcript, expecting
    /// `Unknown{TranscriptCoordinateDivergence}`. `Diverged` appears in no
    /// test there; it maps to the same reason on the strength of the enum
    /// definition (4987:705), which defines the reason as
    /// `행 좌표 ≠ 독립 해결 좌표`. Comparison failures map to nothing: an
    /// unobservable operand is not a divergence claim, and
    /// `TranscriptUnresolved` belongs to `discovery`, not to this comparison.
    pub(in crate::services::discord) const fn unknown_reason(
        self,
    ) -> Option<ReachabilityUnknownReason> {
        match self {
            Self::Diverged | Self::RowPathUnresolvableWhileRegistryLive => {
                Some(ReachabilityUnknownReason::TranscriptCoordinateDivergence)
            }
            Self::NoRowCoordinate
            | Self::NoIndependentCoordinate
            | Self::SameFile
            | Self::Unknown => None,
        }
    }
}

/// The pure comparison — 4987 §6.2's
/// `divergence(row_coordinate, independently_resolved_coordinate)`. Reads no
/// clock and opens no file. Every observation pair is named, with no wildcard
/// arm, so a new [`CoordinateObservation`] variant fails to compile here until
/// someone decides its rows.
pub(in crate::services::discord) fn divergence(
    row_coordinate: CoordinateObservation,
    independently_resolved_coordinate: CoordinateObservation,
) -> RowCoordinateDivergence {
    use CoordinateObservation as Obs;

    match (row_coordinate, independently_resolved_coordinate) {
        (Obs::NotOffered, Obs::NotOffered | Obs::Unresolvable | Obs::Resolved(_)) => {
            RowCoordinateDivergence::NoRowCoordinate
        }
        (Obs::Unresolvable | Obs::Resolved(_), Obs::NotOffered) => {
            RowCoordinateDivergence::NoIndependentCoordinate
        }
        (Obs::Unresolvable, Obs::Resolved(_)) => {
            RowCoordinateDivergence::RowPathUnresolvableWhileRegistryLive
        }
        (Obs::Unresolvable | Obs::Resolved(_), Obs::Unresolvable) => {
            RowCoordinateDivergence::Unknown
        }
        (Obs::Resolved(row), Obs::Resolved(resolved)) => {
            // Limit of the identity being compared: `TranscriptFileId` is
            // `(dev, ino)` and nothing else — no generation number, no birth
            // time — so this equality cannot distinguish "the same file" from
            // "an inode number the OS handed out again". A transcript deleted
            // and recreated onto the same `(dev, ino)` reads as `SameFile`.
            if row == resolved {
                RowCoordinateDivergence::SameFile
            } else {
                RowCoordinateDivergence::Diverged
            }
        }
    }
}

/// Stat both operands, compare, and emit the structured record when the
/// outcome is one of the two non-GREEN shapes. One record per call — one per
/// health poll for as long as the split lasts, the same no-dedupe contract as
/// T4-B0's string-comparison record.
///
/// This **coexists with** that record rather than superseding it. T4-B0's
/// `SessionEnrichment::record_transcript_source_divergence`
/// (`session_enrichment.rs:196`) is untouched by this slice and still fires
/// from the `SessionEnrichment::load` earlier in the very same poll
/// (`snapshot.rs:461`), so a split that both records recognise now logs twice
/// per poll: once as `relay_transcript_source_divergence`, once as
/// `reachability_row_coordinate_divergence`. The two do not agree on what a
/// split is — B0 compares path strings, so it still shouts about two aliases
/// of one file that this comparison correctly calls `SameFile`. Retiring it is
/// a follow-up slice's item, deliberately not this one's.
///
/// Agreement and not-comparable outcomes stay silent so an idle channel logs
/// nothing.
///
/// The two stats are taken one after the other (row first, then registry), so
/// the pair being compared is not an atomic snapshot, and a transcript
/// rotation landing between them can be misread in EITHER direction:
///
/// * a one-poll `Diverged` on files that never disagreed; and
/// * a one-poll `SameFile` that masks a real split. Measured: with the row at
///   inode A and the registry at inode B (already diverged), a rotation that
///   `rename`s the row's file onto the registry path and creates a fresh file
///   at the row path between the two stats hands both operands
///   `Resolved(A)` — `rename` carries the inode — and `SameFile` then names a
///   state that held at no instant.
///
/// Neither is corrected inside the poll. The next poll re-observes, which
/// recovers the true pair only if the files have settled by then; a rotation
/// that recurs every poll can be misread every poll.
pub(in crate::services::discord) fn observe_row_coordinate_divergence(
    provider: &str,
    channel_id: u64,
    row_output_path: Option<&str>,
    registry_output_path: Option<&str>,
) -> RowCoordinateDivergence {
    let outcome = divergence(
        CoordinateObservation::observe(row_output_path),
        CoordinateObservation::observe(registry_output_path),
    );
    if outcome.is_non_green_signal() {
        tracing::warn!(
            counter = "reachability_row_coordinate_divergence",
            provider,
            channel_id,
            row_output_path = row_output_path.unwrap_or(""),
            registry_output_path = registry_output_path.unwrap_or(""),
            outcome = outcome.as_str(),
            "in-flight row and independently resolved transcript disagree by file identity; \
             descriptive signal only — no verdict changes here and no redelivery or \
             destructive action is authorized (4987 S4)"
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::super::verdict::ReachabilityVerdict;
    use super::*;

    fn write(path: &Path, body: &str) {
        fs::write(path, body).expect("write fixture");
    }

    fn utf8(path: &Path) -> &str {
        path.to_str().expect("utf8 fixture path")
    }

    fn every_outcome() -> [RowCoordinateDivergence; 6] {
        [
            RowCoordinateDivergence::NoRowCoordinate,
            RowCoordinateDivergence::NoIndependentCoordinate,
            RowCoordinateDivergence::SameFile,
            RowCoordinateDivergence::Diverged,
            RowCoordinateDivergence::RowPathUnresolvableWhileRegistryLive,
            RowCoordinateDivergence::Unknown,
        ]
    }

    /// 4987 §-1.4 counterexample 4: the wrapper and the native transcript have
    /// the SAME size and different inodes, on two real files. The identity
    /// comparison must catch what a size or path-string comparison cannot.
    #[test]
    fn same_size_different_inode_is_divergence() {
        let dir = TempDir::new().expect("tempdir");
        let native = dir.path().join("native.jsonl");
        let wrapper = dir.path().join("wrapper.jsonl");
        // Byte-identical length, distinct content, distinct inode.
        write(&native, "{\"type\":\"assistant\",\"n\":1}\n");
        write(&wrapper, "{\"type\":\"assistant\",\"n\":2}\n");

        let native_stat = stat_transcript(&native).expect("native stat");
        let wrapper_stat = stat_transcript(&wrapper).expect("wrapper stat");
        assert_eq!(
            native_stat.len, wrapper_stat.len,
            "fixture must reproduce the equal-size mask"
        );
        assert_ne!(native_stat.file_id, wrapper_stat.file_id);

        let outcome = observe_row_coordinate_divergence(
            "claude",
            4_986,
            Some(utf8(&wrapper)),
            Some(utf8(&native)),
        );
        assert_eq!(outcome, RowCoordinateDivergence::Diverged);
        assert!(outcome.is_non_green_signal());
        assert_eq!(
            outcome.unknown_reason(),
            Some(ReachabilityUnknownReason::TranscriptCoordinateDivergence)
        );
    }

    /// The same file reached through a symlink and through a `..` alias is
    /// agreement: identity, not the path string, is the comparison.
    #[test]
    fn same_file_through_different_path_strings_is_not_divergence() {
        let dir = TempDir::new().expect("tempdir");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).expect("mkdir");
        let native = dir.path().join("native.jsonl");
        write(&native, "{\"type\":\"assistant\"}\n");
        let link = dir.path().join("link.jsonl");
        std::os::unix::fs::symlink(&native, &link).expect("symlink");
        let dotdot_alias = sub.join("..").join("native.jsonl");

        for row_alias in [&link, &dotdot_alias] {
            assert_ne!(
                *row_alias, native,
                "the fixture must present two different path strings"
            );
            let outcome = observe_row_coordinate_divergence(
                "claude",
                4_986,
                Some(utf8(row_alias)),
                Some(utf8(&native)),
            );
            assert_eq!(outcome, RowCoordinateDivergence::SameFile);
            assert!(!outcome.is_non_green_signal());
            assert_eq!(outcome.unknown_reason(), None);
        }
    }

    /// 4987 §6.2's mutation-test anchor at this module's level: the row names
    /// a path that does not stat while the independently resolved transcript
    /// is alive (#4986 형상1). The outcome must map to the
    /// `Unknown{TranscriptCoordinateDivergence}` the design's test expects,
    /// and that verdict must not permit health. Producing the verdict in
    /// production is T4-B6's; the mapping is pinned here so B6 cannot spell a
    /// different reason.
    #[test]
    fn reachability_detects_row_path_vs_resolved_transcript_divergence() {
        let dir = TempDir::new().expect("tempdir");
        let native = dir.path().join("native.jsonl");
        write(&native, "{\"type\":\"assistant\"}\n");
        let missing_wrapper = dir.path().join("wrapper-missing.jsonl");

        let outcome = observe_row_coordinate_divergence(
            "claude",
            4_986,
            Some(utf8(&missing_wrapper)),
            Some(utf8(&native)),
        );
        assert_eq!(
            outcome,
            RowCoordinateDivergence::RowPathUnresolvableWhileRegistryLive
        );
        assert!(outcome.is_non_green_signal());

        let reason = outcome
            .unknown_reason()
            .expect("a non-GREEN shape must map to a reason");
        assert_eq!(
            reason,
            ReachabilityUnknownReason::TranscriptCoordinateDivergence
        );
        assert!(
            !ReachabilityVerdict::unknown(reason, 30).permits_health(),
            "4987 §4.1: an Unknown built from this reason is not GREEN"
        );
    }

    /// Fail-closed: a stat failure on the independently resolved side (or on
    /// both sides) is `Unknown`, never a divergence claim.
    #[test]
    fn stat_failure_is_unknown_not_divergence() {
        let dir = TempDir::new().expect("tempdir");
        let live_row = dir.path().join("row.jsonl");
        write(&live_row, "{\"type\":\"assistant\"}\n");
        let missing = dir.path().join("gone.jsonl");
        let directory = dir.path().join("not-a-file");
        fs::create_dir(&directory).expect("mkdir");

        for registry in [&missing, &directory] {
            let outcome = observe_row_coordinate_divergence(
                "claude",
                4_986,
                Some(utf8(&live_row)),
                Some(utf8(registry)),
            );
            assert_eq!(outcome, RowCoordinateDivergence::Unknown);
            assert!(!outcome.is_non_green_signal());
            assert_eq!(outcome.unknown_reason(), None);
        }

        let both_dead = observe_row_coordinate_divergence(
            "claude",
            4_986,
            Some(utf8(&missing)),
            Some(utf8(&dir.path().join("also-gone.jsonl"))),
        );
        assert_eq!(both_dead, RowCoordinateDivergence::Unknown);
    }

    /// An operand that was never offered is not a defect observation, and a
    /// blank path is `NotOffered`, not a failed stat.
    #[test]
    fn absent_or_blank_operands_are_not_comparable_and_not_signals() {
        let dir = TempDir::new().expect("tempdir");
        let native = dir.path().join("native.jsonl");
        write(&native, "{\"type\":\"assistant\"}\n");

        assert_eq!(
            CoordinateObservation::observe(None),
            CoordinateObservation::NotOffered
        );
        assert_eq!(
            CoordinateObservation::observe(Some("   ")),
            CoordinateObservation::NotOffered
        );

        for row in [None, Some("   ")] {
            assert_eq!(
                observe_row_coordinate_divergence("claude", 4_986, row, Some(utf8(&native))),
                RowCoordinateDivergence::NoRowCoordinate
            );
        }
        assert_eq!(
            observe_row_coordinate_divergence("claude", 4_986, Some(utf8(&native)), None),
            RowCoordinateDivergence::NoIndependentCoordinate
        );
    }

    /// The full 3×3 observation matrix, pinned pair by pair so no arm of the
    /// pure comparison can be rewired without a named failure here.
    #[test]
    fn the_comparison_matrix_is_fail_closed() {
        use CoordinateObservation as Obs;
        use RowCoordinateDivergence as Out;

        let dir = TempDir::new().expect("tempdir");
        let a_path = dir.path().join("a.jsonl");
        let b_path = dir.path().join("b.jsonl");
        write(&a_path, "a\n");
        write(&b_path, "b\n");
        let a = Obs::Resolved(stat_transcript(&a_path).expect("a stat").file_id);
        let b = Obs::Resolved(stat_transcript(&b_path).expect("b stat").file_id);

        let table = [
            (Obs::NotOffered, Obs::NotOffered, Out::NoRowCoordinate),
            (Obs::NotOffered, Obs::Unresolvable, Out::NoRowCoordinate),
            (Obs::NotOffered, a, Out::NoRowCoordinate),
            (
                Obs::Unresolvable,
                Obs::NotOffered,
                Out::NoIndependentCoordinate,
            ),
            (a, Obs::NotOffered, Out::NoIndependentCoordinate),
            (
                Obs::Unresolvable,
                a,
                Out::RowPathUnresolvableWhileRegistryLive,
            ),
            (Obs::Unresolvable, Obs::Unresolvable, Out::Unknown),
            (a, Obs::Unresolvable, Out::Unknown),
            (a, a, Out::SameFile),
            (a, b, Out::Diverged),
        ];
        for (row, resolved, expected) in table {
            assert_eq!(
                divergence(row, resolved),
                expected,
                "wrong outcome for ({row:?}, {resolved:?})"
            );
        }
    }

    /// Polarity table in the `verdict.rs` genre: exactly the two divergence
    /// shapes are non-GREEN, and exactly those two map to
    /// `TranscriptCoordinateDivergence`.
    #[test]
    fn only_the_two_divergence_shapes_are_non_green_and_map_to_a_reason() {
        for outcome in every_outcome() {
            let expected = matches!(
                outcome,
                RowCoordinateDivergence::Diverged
                    | RowCoordinateDivergence::RowPathUnresolvableWhileRegistryLive
            );
            assert_eq!(
                outcome.is_non_green_signal(),
                expected,
                "wrong non-GREEN polarity for {outcome:?}"
            );
            assert_eq!(
                outcome.unknown_reason().is_some(),
                expected,
                "wrong reason mapping for {outcome:?}"
            );
        }
    }
}
