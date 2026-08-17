//! Row-independent transcript resolution — 4987 §-1.3 (#5071 T4-B1).
//!
//! The whole point of this file is that it never asks the inflight row where
//! the transcript is. 4987 §1.3(a) named the structural defect: the observer
//! shared the observed party's broken coordinate, so `output_path` being wrong
//! made both the relay and its watchdog wrong in the same direction. The ladder
//! below resolves the file from coordinates the row does not own, and compares
//! file **identity** rather than path strings, because #4986 형상1 had one live
//! file and one ENOENT under names that a string comparison cannot separate.
//!
//! Ladder (4987 §-1.3, rank order preserved):
//!
//! 1. the watcher registry entry's `output_path` — the value that actually held
//!    the native transcript in #4986 형상1. T4-B0 exposes it on the binding;
//!    this slice takes it as a parameter and is therefore independent of B0's
//!    landing;
//! 2. `TuiRuntimeBinding` — 4987 §-1.1 R1 accepted that this is **not** an
//!    independent coordinate (it can fall back to the wrapper), so it is used
//!    as a resolution source only when rank 1 produced nothing, and otherwise
//!    only as a comparison operand;
//! 3. filesystem discovery over caller-supplied roots;
//! 4. everything failed ⇒ [`ReachabilityUnknownReason::TranscriptUnresolved`],
//!    which is `Unknown`, which is **not** `Reachable`. Fail-closed is the
//!    design: 4987 §-1.4 makes discovery failure itself a detection, so a wrong
//!    resolution cannot silently become GREEN.
//!
//! # What this file does NOT do
//!
//! It produces no verdict and no derived signal. It returns the comparison it
//! made as data ([`BindingComparison`]) so T4-B4 can wire
//! `RowPathUnresolvableWhileRegistryLive` and the snapshot-level divergence
//! signal without re-statting; deciding what those mean is B4's, not this
//! slice's. Nothing here reads a clock, so a caller supplies `since_secs` when
//! it turns a reason into a verdict.

use std::path::{Path, PathBuf};

use super::verdict::ReachabilityUnknownReason;

/// A transcript file's identity: the file itself, never the path that led to
/// it (4987 §-1.3 — `transcript_file_id: (dev, ino)`).
///
/// Size is deliberately NOT part of identity. 4987 §-1.4 counterexample 4 is a
/// wrapper and a native transcript of *equal size* and different inode; folding
/// size in would let that pair compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct TranscriptFileId {
    pub(in crate::services::discord) dev: u64,
    pub(in crate::services::discord) ino: u64,
}

/// One stat of a transcript: its identity plus the length observed at that
/// moment. The length is a separate field precisely because it is not identity;
/// B2 uses it as the "file is advancing" half of 4987 §-1.4's positive
/// incarnation-alive evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct TranscriptStat {
    pub(in crate::services::discord) file_id: TranscriptFileId,
    pub(in crate::services::discord) len: u64,
}

/// Which rung of the §-1.3 ladder produced a resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TranscriptSource {
    RegistryOutputPath,
    RuntimeBinding,
    FilesystemDiscovery,
}

/// What the rank-2 operand said about the rank-1 winner.
///
/// This is recorded, not judged. `Unresolvable` is the shape of #4986 형상1 (a
/// dead row path beside a live registry path); T4-B4 turns it into the
/// `RowPathUnresolvableWhileRegistryLive` signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum BindingComparison {
    /// No rank-2 path was offered, so no comparison was possible.
    NotOffered,
    /// The rank-2 path resolved to the same file. 4987 §-1.3 notes rank 2 is
    /// only meaningful when it differs from rank 1; this is the "does not
    /// differ" case.
    SameFile,
    /// A rank-2 path was offered but could not be stat'ed while rank 1 could.
    Unresolvable,
    /// Rank 2 was itself the resolution, so there was nothing to compare it to.
    WasTheResolution,
}

/// A resolved transcript and how it was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct ResolvedTranscript {
    pub(in crate::services::discord) path: PathBuf,
    pub(in crate::services::discord) source: TranscriptSource,
    pub(in crate::services::discord) stat: TranscriptStat,
    pub(in crate::services::discord) binding_comparison: BindingComparison,
}

/// The outcome of the ladder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum TranscriptResolution {
    Resolved(ResolvedTranscript),
    /// Fail-closed. The reason is one of `TranscriptUnresolved` (no rung
    /// produced a file) or `TranscriptCoordinateDivergence` (two independently
    /// resolved coordinates named different files).
    Unresolved(ReachabilityUnknownReason),
}

/// The coordinates a caller can offer the ladder.
///
/// Every field is borrowed and optional: this slice is wired to nothing, so the
/// caller — the observation task T4-B2 adds — decides which coordinates exist.
#[derive(Debug, Clone, Copy)]
pub(in crate::services::discord) struct TranscriptCandidates<'a> {
    /// Rank 1: the live watcher registry entry's `output_path`.
    pub(in crate::services::discord) registry_output_path: Option<&'a Path>,
    /// Rank 2: the `TuiRuntimeBinding` path. Not an independent coordinate.
    pub(in crate::services::discord) runtime_binding_path: Option<&'a Path>,
    /// Rank 3: directories to scan for `*.jsonl`, most recently modified wins.
    /// Scanning is non-recursive and unbounded in entry count; the caller picks
    /// the roots and therefore owns the cost 4987 §-1.3 flags as "비용·위험 큼".
    pub(in crate::services::discord) discovery_roots: &'a [PathBuf],
}

/// Stat one candidate path, returning `None` for anything that is not an
/// existing regular file (missing, a directory, a broken symlink, or a stat
/// that fails outright — e.g. EACCES from an unsearchable parent directory).
///
/// Readability is NOT checked, and nothing downstream may assume it: `metadata`
/// needs no read permission on the file itself, so a mode-000 transcript stats
/// clean and comes back `Some` (measured). Whether the tail can actually be
/// read is the reader's problem, not this ladder's.
pub(in crate::services::discord) fn stat_transcript(path: &Path) -> Option<TranscriptStat> {
    // `metadata` follows symlinks on purpose: a transcript reached through a
    // symlink is the same file, and identity is taken from the target.
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    Some(TranscriptStat {
        file_id: file_id_of(&meta),
        len: meta.len(),
    })
}

/// Extract `(dev, ino)` from an already-taken `Metadata`.
///
/// Unix-only, which is why the whole module is `#[cfg(unix)]` at its
/// declaration site: Windows exposes no stable file index through
/// `std::fs::Metadata`, and a synthesized identity would be a lie in exactly
/// the comparison this file exists to make.
pub(in crate::services::discord) fn file_id_of(meta: &std::fs::Metadata) -> TranscriptFileId {
    use std::os::unix::fs::MetadataExt;

    TranscriptFileId {
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

/// Walk the 4987 §-1.3 ladder.
pub(in crate::services::discord) fn resolve_transcript(
    candidates: TranscriptCandidates<'_>,
) -> TranscriptResolution {
    let registry = candidates
        .registry_output_path
        .and_then(|path| stat_transcript(path).map(|stat| (path, stat)));
    let binding = candidates
        .runtime_binding_path
        .and_then(|path| stat_transcript(path).map(|stat| (path, stat)));

    match (registry, binding) {
        (Some((registry_path, registry_stat)), Some((_, binding_stat))) => {
            if registry_stat.file_id != binding_stat.file_id {
                // Two coordinates that both resolve, to different files. 4987
                // §-1.4 counterexample 4 is exactly this with equal sizes, so
                // the comparison is on identity alone and size cannot mask it.
                // Fail-closed rather than picking a winner: `Unknown` is
                // non-GREEN, and guessing here is how an observer adopts the
                // observed party's wrong coordinate.
                return TranscriptResolution::Unresolved(
                    ReachabilityUnknownReason::TranscriptCoordinateDivergence,
                );
            }
            TranscriptResolution::Resolved(ResolvedTranscript {
                path: registry_path.to_path_buf(),
                source: TranscriptSource::RegistryOutputPath,
                stat: registry_stat,
                binding_comparison: BindingComparison::SameFile,
            })
        }
        (Some((registry_path, registry_stat)), None) => {
            let binding_comparison = if candidates.runtime_binding_path.is_some() {
                // A rank-2 path exists but does not resolve while rank 1 does:
                // the #4986 형상1 shape. Recorded, not judged (T4-B4).
                BindingComparison::Unresolvable
            } else {
                BindingComparison::NotOffered
            };
            TranscriptResolution::Resolved(ResolvedTranscript {
                path: registry_path.to_path_buf(),
                source: TranscriptSource::RegistryOutputPath,
                stat: registry_stat,
                binding_comparison,
            })
        }
        (None, Some((binding_path, binding_stat))) => {
            TranscriptResolution::Resolved(ResolvedTranscript {
                path: binding_path.to_path_buf(),
                source: TranscriptSource::RuntimeBinding,
                stat: binding_stat,
                binding_comparison: BindingComparison::WasTheResolution,
            })
        }
        (None, None) => discover_from_roots(candidates.discovery_roots),
    }
}

/// Rank 3: the newest `*.jsonl` across the given roots.
///
/// Selection is `(mtime, path)` descending, so equal mtimes resolve to the
/// bytewise-greatest path rather than to readdir order — a tick that flapped
/// between two same-mtime files would produce a cursor that never advances.
fn discover_from_roots(roots: &[PathBuf]) -> TranscriptResolution {
    let mut best: Option<(std::time::SystemTime, PathBuf, TranscriptStat)> = None;

    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let stat = TranscriptStat {
                file_id: file_id_of(&meta),
                len: meta.len(),
            };
            let better = match &best {
                None => true,
                Some((best_mtime, best_path, _)) => (modified, &path) > (*best_mtime, best_path),
            };
            if better {
                best = Some((modified, path, stat));
            }
        }
    }

    match best {
        Some((_, path, stat)) => TranscriptResolution::Resolved(ResolvedTranscript {
            path,
            source: TranscriptSource::FilesystemDiscovery,
            stat,
            binding_comparison: BindingComparison::NotOffered,
        }),
        None => TranscriptResolution::Unresolved(ReachabilityUnknownReason::TranscriptUnresolved),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    fn write(path: &Path, body: &str) {
        let mut file = fs::File::create(path).expect("create fixture");
        file.write_all(body.as_bytes()).expect("write fixture");
    }

    fn candidates<'a>(
        registry: Option<&'a Path>,
        binding: Option<&'a Path>,
        roots: &'a [PathBuf],
    ) -> TranscriptCandidates<'a> {
        TranscriptCandidates {
            registry_output_path: registry,
            runtime_binding_path: binding,
            discovery_roots: roots,
        }
    }

    /// 4987 §-1.3 rung 4 and §-1.4: every rung failing is `Unknown`, and
    /// `Unknown` is not `Reachable`. This is the design row's first mandated
    /// pure test.
    #[test]
    fn unresolved_when_every_rung_fails() {
        let dir = TempDir::new().expect("tempdir");
        let missing_registry = dir.path().join("registry-missing.jsonl");
        let missing_binding = dir.path().join("binding-missing.jsonl");
        let empty_root = dir.path().join("no-such-root");
        let roots = vec![empty_root];

        let resolution = resolve_transcript(candidates(
            Some(&missing_registry),
            Some(&missing_binding),
            &roots,
        ));

        assert_eq!(
            resolution,
            TranscriptResolution::Unresolved(ReachabilityUnknownReason::TranscriptUnresolved)
        );
    }

    #[test]
    fn a_directory_is_not_a_transcript() {
        let dir = TempDir::new().expect("tempdir");
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).expect("mkdir");

        assert_eq!(stat_transcript(&sub), None);

        let roots: Vec<PathBuf> = Vec::new();
        assert_eq!(
            resolve_transcript(candidates(Some(&sub), None, &roots)),
            TranscriptResolution::Unresolved(ReachabilityUnknownReason::TranscriptUnresolved)
        );
    }

    /// 4987 §-1.4 counterexample 4: the wrapper and the native transcript have
    /// the SAME SIZE and different inodes. Identity comparison must catch it;
    /// a size or path-string comparison would not. This is the design row's
    /// second mandated pure test.
    #[test]
    fn same_size_different_inode_is_coordinate_divergence() {
        let dir = TempDir::new().expect("tempdir");
        let native = dir.path().join("native.jsonl");
        let wrapper = dir.path().join("wrapper.jsonl");
        // Byte-identical length, distinct content, distinct inode.
        write(&native, "{\"type\":\"assistant\"}\n");
        write(&wrapper, "{\"type\":\"assistant\"}\n");

        let native_stat = stat_transcript(&native).expect("native stat");
        let wrapper_stat = stat_transcript(&wrapper).expect("wrapper stat");
        assert_eq!(
            native_stat.len, wrapper_stat.len,
            "fixture must reproduce the equal-size case"
        );
        assert_ne!(native_stat.file_id, wrapper_stat.file_id);

        let roots: Vec<PathBuf> = Vec::new();
        assert_eq!(
            resolve_transcript(candidates(Some(&native), Some(&wrapper), &roots)),
            TranscriptResolution::Unresolved(
                ReachabilityUnknownReason::TranscriptCoordinateDivergence
            )
        );
    }

    /// The same file reached through two different paths is not divergence:
    /// identity, not the path string, is the comparison.
    #[test]
    fn same_file_through_two_paths_is_not_divergence() {
        let dir = TempDir::new().expect("tempdir");
        let native = dir.path().join("native.jsonl");
        let link = dir.path().join("link.jsonl");
        write(&native, "{\"type\":\"assistant\"}\n");
        std::os::unix::fs::symlink(&native, &link).expect("symlink");

        let roots: Vec<PathBuf> = Vec::new();
        let resolution = resolve_transcript(candidates(Some(&native), Some(&link), &roots));

        let TranscriptResolution::Resolved(resolved) = resolution else {
            panic!("expected a resolution, got {resolution:?}");
        };
        assert_eq!(resolved.source, TranscriptSource::RegistryOutputPath);
        assert_eq!(resolved.binding_comparison, BindingComparison::SameFile);
        assert_eq!(resolved.path, native);
    }

    /// #4986 형상1's shape: the rank-2 path is dead while rank 1 is live. Rank 1
    /// still wins, and the fact is recorded for T4-B4 rather than dropped.
    #[test]
    fn dead_binding_beside_live_registry_resolves_and_records_unresolvable() {
        let dir = TempDir::new().expect("tempdir");
        let registry = dir.path().join("registry.jsonl");
        let binding = dir.path().join("gone.jsonl");
        write(&registry, "{\"type\":\"assistant\"}\n");

        let roots: Vec<PathBuf> = Vec::new();
        let resolution = resolve_transcript(candidates(Some(&registry), Some(&binding), &roots));

        let TranscriptResolution::Resolved(resolved) = resolution else {
            panic!("expected a resolution, got {resolution:?}");
        };
        assert_eq!(resolved.source, TranscriptSource::RegistryOutputPath);
        assert_eq!(resolved.binding_comparison, BindingComparison::Unresolvable);
    }

    /// Rank 1 resolves and no rank-2 path was offered at all. The comparison
    /// must record `NotOffered`, not `SameFile`: `BindingComparison` is the
    /// datum T4-B4 reads, and "there was nothing to compare" is a different
    /// fact from "we compared two coordinates and they agreed". This case
    /// exists because the arm was otherwise unpinned — swapping its
    /// `NotOffered` for `SameFile` left all nine other discovery tests green.
    /// It pins the rank-1 arm only; rank 3 spells `NotOffered` at its own site.
    #[test]
    fn registry_only_resolution_records_that_no_binding_was_offered() {
        let dir = TempDir::new().expect("tempdir");
        let registry = dir.path().join("registry.jsonl");
        write(&registry, "{\"type\":\"assistant\"}\n");

        let roots: Vec<PathBuf> = Vec::new();
        let resolution = resolve_transcript(candidates(Some(&registry), None, &roots));

        let TranscriptResolution::Resolved(resolved) = resolution else {
            panic!("expected a resolution, got {resolution:?}");
        };
        assert_eq!(resolved.source, TranscriptSource::RegistryOutputPath);
        assert_eq!(
            resolved.binding_comparison,
            BindingComparison::NotOffered,
            "no rank-2 path was offered, so nothing was compared"
        );
        assert_eq!(resolved.path, registry);
    }

    #[test]
    fn binding_resolves_only_when_registry_produced_nothing() {
        let dir = TempDir::new().expect("tempdir");
        let missing_registry = dir.path().join("missing.jsonl");
        let binding = dir.path().join("binding.jsonl");
        write(&binding, "{\"type\":\"assistant\"}\n");

        let roots: Vec<PathBuf> = Vec::new();
        let resolution =
            resolve_transcript(candidates(Some(&missing_registry), Some(&binding), &roots));

        let TranscriptResolution::Resolved(resolved) = resolution else {
            panic!("expected a resolution, got {resolution:?}");
        };
        assert_eq!(resolved.source, TranscriptSource::RuntimeBinding);
        assert_eq!(
            resolved.binding_comparison,
            BindingComparison::WasTheResolution
        );
    }

    #[test]
    fn filesystem_discovery_picks_the_newest_jsonl_and_ignores_other_extensions() {
        let dir = TempDir::new().expect("tempdir");
        let old = dir.path().join("old.jsonl");
        let new = dir.path().join("new.jsonl");
        let decoy = dir.path().join("newest.log");
        write(&old, "old\n");
        write(&new, "new\n");
        write(&decoy, "decoy\n");

        let epoch = std::time::SystemTime::UNIX_EPOCH;
        let old_time = epoch + std::time::Duration::from_secs(1_000);
        let new_time = epoch + std::time::Duration::from_secs(2_000);
        let decoy_time = epoch + std::time::Duration::from_secs(3_000);
        fs::File::open(&old)
            .and_then(|f| f.set_modified(old_time))
            .expect("set old mtime");
        fs::File::open(&new)
            .and_then(|f| f.set_modified(new_time))
            .expect("set new mtime");
        fs::File::open(&decoy)
            .and_then(|f| f.set_modified(decoy_time))
            .expect("set decoy mtime");

        let roots = vec![dir.path().to_path_buf()];
        let resolution = resolve_transcript(candidates(None, None, &roots));

        let TranscriptResolution::Resolved(resolved) = resolution else {
            panic!("expected a resolution, got {resolution:?}");
        };
        assert_eq!(resolved.source, TranscriptSource::FilesystemDiscovery);
        assert_eq!(
            resolved.path, new,
            "the newer .jsonl must win over the older"
        );
    }

    /// Equal mtimes must not resolve by readdir order, or the chosen file could
    /// flip between ticks and the byte cursor would never advance.
    #[test]
    fn equal_mtimes_break_the_tie_deterministically() {
        let dir = TempDir::new().expect("tempdir");
        let a = dir.path().join("a.jsonl");
        let b = dir.path().join("b.jsonl");
        write(&a, "a\n");
        write(&b, "b\n");

        let stamp = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(4_000);
        for path in [&a, &b] {
            fs::File::open(path)
                .and_then(|f| f.set_modified(stamp))
                .expect("set mtime");
        }

        let roots = vec![dir.path().to_path_buf()];
        for _ in 0..8 {
            let TranscriptResolution::Resolved(resolved) =
                resolve_transcript(candidates(None, None, &roots))
            else {
                panic!("expected a resolution");
            };
            assert_eq!(resolved.path, b, "the bytewise-greatest path must win");
        }
    }

    #[test]
    fn unreadable_discovery_root_is_skipped_not_fatal() {
        let dir = TempDir::new().expect("tempdir");
        let live = dir.path().join("live.jsonl");
        write(&live, "live\n");

        let roots = vec![dir.path().join("does-not-exist"), dir.path().to_path_buf()];
        let TranscriptResolution::Resolved(resolved) =
            resolve_transcript(candidates(None, None, &roots))
        else {
            panic!("expected a resolution");
        };
        assert_eq!(resolved.path, live);
    }
}
