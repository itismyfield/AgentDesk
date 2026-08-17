//! Bounded incremental tail reader — 4987 S1 (#5071 T4-B1).
//!
//! The obligation prober T4-B2c wires needs to read what a transcript grew by
//! since the last tick, cheaply, without ever concluding "nothing to see" from
//! a file it is no longer looking at. This file is that read and nothing else:
//! it returns raw bytes and the byte range they came from.
//!
//! # Line framing is NOT here
//!
//! A capped read almost always ends mid-line, and a JSONL record can be split
//! across ticks. Partial lines, CRLF, multi-byte boundaries and rotation
//! coordinates are the canonical-schema work 4987 §-1.5 assigns to T4-B2
//! (together with the Rust↔Python fixture equivalence that proves both
//! implementations frame identically). Doing half of it here would create the
//! second oracle §2.4 warns about, so this file hands over bytes and a range
//! and makes no claim about record boundaries.
//!
//! # Identity revalidation
//!
//! The cursor carries the `(dev, ino)` it was established against, and every
//! read re-checks it against the file it actually opened. Reading by path and
//! trusting the offset is how a rotated transcript silently turns into "the
//! relay is quiet": the same path, a different file, a byte offset that means
//! nothing. A cursor that no longer matches is reported, never resumed.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use super::discovery::{TranscriptFileId, file_id_of};
use super::verdict::ReachabilityUnknownReason;

/// The per-tick read cap (4987 §4.1 `ReadTruncated`).
///
/// Exceeding it does not fail the tick and does not stall the cursor: the read
/// returns the capped prefix and marks itself truncated, so the tick advances
/// and the caller reports `Unknown{ReadTruncated}` for it — a bounded read that
/// did not see everything is explicitly not evidence of health.
pub(in crate::services::discord) const TAIL_READ_CAP_BYTES: u64 = 1024 * 1024;

/// Where the last read stopped, and in which file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct TailCursor {
    pub(in crate::services::discord) file_id: TranscriptFileId,
    pub(in crate::services::discord) next_offset: u64,
}

impl TailCursor {
    pub(in crate::services::discord) fn new(file_id: TranscriptFileId, next_offset: u64) -> Self {
        Self {
            file_id,
            next_offset,
        }
    }

    /// The cursor to use next tick after a read that ended at `end`.
    ///
    /// The production observation task advances to `ObligationScan::next_offset`, which is
    /// deliberately BEHIND the read end whenever the chunk ended mid-line, so
    /// the partial line is re-read whole. This helper is the unconditional
    /// advance, correct only where framing cannot defer, and it stays because
    /// this file's own tests read through it.
    #[allow(dead_code)]
    pub(in crate::services::discord) fn advanced_to(self, end: u64) -> Self {
        Self {
            file_id: self.file_id,
            next_offset: end,
        }
    }
}

/// What one incremental read observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum TailOutcome {
    /// `bytes` are the file's contents over `[start, end)`. `cap_truncated`
    /// means more was available than [`TAIL_READ_CAP_BYTES`] allowed.
    Read {
        bytes: Vec<u8>,
        start: u64,
        end: u64,
        observed_len: u64,
        cap_truncated: bool,
    },
    /// The path now names a different file than the cursor was established
    /// against. The offset is meaningless across that boundary, so no bytes are
    /// returned and the caller re-resolves.
    IdentityDiverged {
        expected: TranscriptFileId,
        observed: TranscriptFileId,
    },
    /// Same file, but shorter than the cursor: truncated or rewritten in place.
    /// Also a broken coordinate — the bytes the cursor already consumed are not
    /// the bytes now at those offsets.
    Rewound {
        cursor_offset: u64,
        observed_len: u64,
    },
    /// The file could not be opened or stat'ed at all.
    Unreadable,
}

impl TailOutcome {
    /// The `Unknown` reason this outcome implies, if any.
    ///
    /// A complete read implies none — which is NOT the same as implying
    /// `Reachable`. 4987 §-1.4 requires positive incarnation-alive evidence
    /// before anything may spell `Reachable`, and that decision belongs to the
    /// obligation subtraction, not to a byte read.
    pub(in crate::services::discord) fn unknown_reason(&self) -> Option<ReachabilityUnknownReason> {
        match self {
            Self::Read {
                cap_truncated: true,
                ..
            } => Some(ReachabilityUnknownReason::ReadTruncated),
            Self::Read {
                cap_truncated: false,
                ..
            } => None,
            Self::IdentityDiverged { .. } | Self::Rewound { .. } => {
                Some(ReachabilityUnknownReason::TranscriptCoordinateDivergence)
            }
            Self::Unreadable => Some(ReachabilityUnknownReason::TranscriptUnresolved),
        }
    }
}

/// Read `path` forward from `cursor`, bounded by [`TAIL_READ_CAP_BYTES`].
///
/// Identity and length both come from the metadata of the OPEN handle, not from
/// a second `stat` of the path, so a rename between the two calls cannot make
/// the reader validate one file and read another.
pub(in crate::services::discord) fn read_incremental(
    path: &Path,
    cursor: TailCursor,
) -> TailOutcome {
    let Ok(mut file) = std::fs::File::open(path) else {
        return TailOutcome::Unreadable;
    };
    let Ok(meta) = file.metadata() else {
        return TailOutcome::Unreadable;
    };
    if !meta.is_file() {
        return TailOutcome::Unreadable;
    }

    let observed = file_id_of(&meta);
    if observed != cursor.file_id {
        return TailOutcome::IdentityDiverged {
            expected: cursor.file_id,
            observed,
        };
    }

    let len = meta.len();
    if len < cursor.next_offset {
        return TailOutcome::Rewound {
            cursor_offset: cursor.next_offset,
            observed_len: len,
        };
    }

    let available = len - cursor.next_offset;
    let cap_truncated = available > TAIL_READ_CAP_BYTES;
    let to_read = available.min(TAIL_READ_CAP_BYTES);

    if file.seek(SeekFrom::Start(cursor.next_offset)).is_err() {
        return TailOutcome::Unreadable;
    }

    // `to_read` is bounded by the cap, so the allocation is bounded too.
    let mut bytes = vec![0u8; to_read as usize];
    if file.read_exact(&mut bytes).is_err() {
        // The file shrank between the metadata read and the read itself. Treat
        // it as the rewind it is rather than returning a short buffer whose
        // range would be a lie.
        return TailOutcome::Rewound {
            cursor_offset: cursor.next_offset,
            observed_len: len,
        };
    }

    TailOutcome::Read {
        bytes,
        start: cursor.next_offset,
        end: cursor.next_offset + to_read,
        observed_len: len,
        cap_truncated,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use tempfile::TempDir;

    use super::super::discovery::stat_transcript;
    use super::*;

    fn write(path: &Path, body: &[u8]) {
        let mut file = fs::File::create(path).expect("create fixture");
        file.write_all(body).expect("write fixture");
    }

    fn append(path: &Path, body: &[u8]) {
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("open fixture for append");
        file.write_all(body).expect("append fixture");
    }

    fn cursor_at(path: &Path, offset: u64) -> TailCursor {
        let stat = stat_transcript(path).expect("stat fixture");
        TailCursor::new(stat.file_id, offset)
    }

    #[test]
    fn incremental_read_returns_only_the_new_bytes_and_advances() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        write(&path, b"first\n");

        let cursor = cursor_at(&path, 0);
        let TailOutcome::Read {
            bytes,
            start,
            end,
            observed_len,
            cap_truncated,
        } = read_incremental(&path, cursor)
        else {
            panic!("expected a read");
        };
        assert_eq!(bytes, b"first\n");
        assert_eq!((start, end), (0, 6));
        assert_eq!(observed_len, 6);
        assert!(!cap_truncated);

        append(&path, b"second\n");
        let TailOutcome::Read {
            bytes,
            start,
            end,
            observed_len,
            cap_truncated,
        } = read_incremental(&path, cursor.advanced_to(end))
        else {
            panic!("expected a second read");
        };
        assert_eq!(bytes, b"second\n");
        assert_eq!((start, end), (6, 13));
        assert_eq!(observed_len, 13);
        assert!(!cap_truncated);
    }

    #[test]
    fn a_quiet_file_reads_zero_bytes_without_error() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        write(&path, b"only\n");

        let outcome = read_incremental(&path, cursor_at(&path, 5));
        assert_eq!(
            outcome,
            TailOutcome::Read {
                bytes: Vec::new(),
                start: 5,
                end: 5,
                observed_len: 5,
                cap_truncated: false,
            }
        );
        assert_eq!(outcome.unknown_reason(), None);
    }

    /// The design row's third mandated pure test: the three adjacent sizes at
    /// the cap distinguish a complete read from a truncated one, and every
    /// outcome advances by exactly what it read.
    #[test]
    fn one_tick_pins_the_read_cap_boundary() {
        let dir = TempDir::new().expect("tempdir");
        let cases = [
            (TAIL_READ_CAP_BYTES - 1, false),
            (TAIL_READ_CAP_BYTES, false),
            (TAIL_READ_CAP_BYTES + 1, true),
        ];

        for (available, expected_truncated) in cases {
            let path = dir.path().join(format!("transcript-{available}.jsonl"));
            write(&path, &vec![b'x'; available as usize]);

            let TailOutcome::Read {
                bytes,
                start,
                end,
                observed_len,
                cap_truncated,
            } = read_incremental(&path, cursor_at(&path, 0))
            else {
                panic!("expected a read for {available} available bytes");
            };
            let expected_read = available.min(TAIL_READ_CAP_BYTES);
            assert_eq!(bytes.len() as u64, expected_read);
            assert_eq!((start, end), (0, expected_read));
            assert_eq!(observed_len, available);
            assert_eq!(
                cap_truncated, expected_truncated,
                "unexpected truncation at {available} available bytes"
            );
        }
    }

    /// The cap bounds the UNREAD REMAINDER, not the file.
    ///
    /// Every case above starts at offset 0, where `len - next_offset` and `len`
    /// are the same number, so the whole boundary is pinned in the one position
    /// that cannot tell them apart. Measured: with the cases above as the only
    /// coverage, reading the cap against `len` instead of `available` leaves the
    /// suite green. A second tick — the shape every real tick after the first
    /// has — separates them, and re-pins `>` against `>=` at that offset.
    #[test]
    fn the_read_cap_measures_the_unread_remainder_not_the_file_length() {
        let dir = TempDir::new().expect("tempdir");
        // Consumed by an earlier tick, so `len` overshoots `available` by this
        // much in every case below.
        const CONSUMED: u64 = 10;
        let cases = [
            (TAIL_READ_CAP_BYTES - 1, false),
            (TAIL_READ_CAP_BYTES, false),
            (TAIL_READ_CAP_BYTES + 1, true),
        ];

        for (remaining, expected_truncated) in cases {
            let path = dir.path().join(format!("transcript-{remaining}.jsonl"));
            write(&path, &vec![b'x'; (CONSUMED + remaining) as usize]);

            let TailOutcome::Read {
                bytes,
                start,
                end,
                cap_truncated,
                observed_len,
            } = read_incremental(&path, cursor_at(&path, CONSUMED))
            else {
                panic!("expected a read with {remaining} bytes remaining");
            };
            let expected_read = remaining.min(TAIL_READ_CAP_BYTES);
            assert_eq!(bytes.len() as u64, expected_read);
            assert_eq!((start, end), (CONSUMED, CONSUMED + expected_read));
            // The two quantities this test separates: the full file length vs
            // the capped read window.
            assert_eq!(observed_len, CONSUMED + remaining);
            assert_eq!(
                cap_truncated, expected_truncated,
                "unexpected truncation with {remaining} bytes remaining"
            );
        }
    }

    #[test]
    fn cap_truncation_is_read_truncated_unknown() {
        let outcome = TailOutcome::Read {
            bytes: vec![b'x'; 8],
            start: 0,
            end: 8,
            observed_len: 9,
            cap_truncated: true,
        };
        assert_eq!(
            outcome.unknown_reason(),
            Some(ReachabilityUnknownReason::ReadTruncated)
        );
    }

    /// Same path, different inode: the offset is meaningless and the reader
    /// must say so instead of resuming into an unrelated file.
    #[test]
    fn rotation_to_a_different_inode_diverges() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        write(&path, b"first\n");
        let cursor = cursor_at(&path, 6);

        // Replace the path with a genuinely different file, same name.
        let replacement = dir.path().join("replacement.jsonl");
        write(&replacement, b"rotated\n");
        fs::rename(&replacement, &path).expect("rotate");

        let observed = stat_transcript(&path).expect("stat rotated").file_id;
        assert_ne!(observed, cursor.file_id, "fixture must change the inode");

        let outcome = read_incremental(&path, cursor);
        assert_eq!(
            outcome,
            TailOutcome::IdentityDiverged {
                expected: cursor.file_id,
                observed,
            }
        );
        assert_eq!(
            outcome.unknown_reason(),
            Some(ReachabilityUnknownReason::TranscriptCoordinateDivergence)
        );
    }

    #[test]
    fn in_place_truncation_rewinds_instead_of_reading() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        write(&path, b"aaaaaaaaaa\n");
        let cursor = cursor_at(&path, 11);

        let file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open for truncate");
        file.set_len(3).expect("truncate in place");

        let outcome = read_incremental(&path, cursor);
        assert_eq!(
            outcome,
            TailOutcome::Rewound {
                cursor_offset: 11,
                observed_len: 3,
            }
        );
        assert_eq!(
            outcome.unknown_reason(),
            Some(ReachabilityUnknownReason::TranscriptCoordinateDivergence)
        );
    }

    #[test]
    fn a_missing_file_is_unreadable_not_quiet() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        write(&path, b"present\n");
        let cursor = cursor_at(&path, 0);
        fs::remove_file(&path).expect("remove fixture");

        let outcome = read_incremental(&path, cursor);
        assert_eq!(outcome, TailOutcome::Unreadable);
        assert_eq!(
            outcome.unknown_reason(),
            Some(ReachabilityUnknownReason::TranscriptUnresolved)
        );
    }

    #[test]
    fn a_directory_is_unreadable() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("transcript.jsonl");
        write(&path, b"present\n");
        let cursor = cursor_at(&path, 0);

        let as_dir = dir.path().join("dir.jsonl");
        fs::create_dir(&as_dir).expect("mkdir");
        assert_eq!(read_incremental(&as_dir, cursor), TailOutcome::Unreadable);
    }
}
