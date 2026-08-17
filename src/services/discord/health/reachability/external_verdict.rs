//! Watchdog verdict intake — 4987 S6 (#5071 T4-B5).
//!
//! # The one-way sidecar
//!
//! `scripts/relay_watchdog.py` writes
//! `runtime/discord_external_relay_verdicts/<provider>/<channel_id>.json` and
//! this module reads it. Nothing here writes that file and nothing here calls
//! the watchdog: 4987 §5.3 keeps the direction one-way because a watchdog that
//! POSTed to dcserver would lose its verdict exactly when dcserver is the thing
//! that died.
//!
//! The §5.2 payload fields (`verdict`, `observed_at_epoch_ms`, `source`,
//! `watchdog_state_version`, `reason`, `lost_blocks`, `last_delivered_ts`) are
//! unchanged. §-1.5 ADDED `generation_mtime_ns`, `spawn_nonce`,
//! `transcript_file_id`, `watchdog_epoch` and `read_complete` on top of them, so
//! the version did not move and the writer still fills each older key —
//! `ExternalVerdictRecordTests::test_the_older_payload_keys_are_kept_unchanged`
//! in `tests/test_relay_watchdog.py` pins that.
//!
//! # Identity-gated monotonicity, not freshness
//!
//! §-1.5 revised §5.2's authority rule after the counter-review: the watchdog's
//! `unreachable` may not be promoted on the strength of a recent timestamp,
//! because its bounded `agentdesk discord read --limit N` can miss older
//! messages, an edit or a delete moves the text match, and a stale transcript
//! selection frames the wrong incarnation. The gate is therefore:
//!
//! 1. `read_complete == false`, or an absent identity leg → [`ExternalVerdictIntake::Incomplete`].
//! 2. The record frames another incarnation → [`ExternalVerdictIntake::WrongIncarnation`].
//! 3. `watchdog_epoch` below the epoch the caller already accepted →
//!    [`ExternalVerdictIntake::Stale`].
//! 4. Only what survives all three reaches [`ExternalVerdictIntake::Usable`],
//!    and [`worsen`] then moves the held claim toward failure only.
//!
//! Everything else reads as [`ExternalRelayVerdict::Unknown`] — §-1.5 ①'s "no
//! authority". There is deliberately no `observed_at_epoch_ms` freshness rule
//! here; §5.4 wants watchdog ABSENCE surfaced as its own alarm, which is a
//! separate signal and is not this module.
//!
//! # Why the identity fields are trusted
//!
//! Identity here is the comparison of ALL THREE legs — `transcript_file_id`,
//! `spawn_nonce` and `generation_mtime_ns` — against the [`LedgerIncarnation`] the
//! caller framed: a match needs every leg, while a mismatch on any single one is
//! enough to reject.
//!
//! `generation_mtime_ns` contributes an incarnation key stamped by
//! `super::super::super::tmux_session_files::stamp_session_generation_marker`,
//! which TRIES to push a freshly stamped `.generation` inode past the previous
//! incarnation's mtime, escalating through `GENERATION_MTIME_BUMP_STEPS_NS`
//! (#5437, and #5439 for same-tick respawns). That push is best-effort, so
//! generation monotonicity is not a property this module may lean on:
//! `bump_generation_mtime_past_previous` warns and publishes the mtime it
//! OBSERVED — which may still equal the previous incarnation's — when `set_times`
//! fails, when re-reading the bumped mtime fails, or when every bump step is
//! truncated back onto the previous value, and the spawn continues either way
//! (`tmux_session_files::tests::unbumpable_generation_mtime_still_publishes_the_marker_and_warns_5437`
//! pins that path).
//!
//! What that leaves unguaranteed: two incarnations of the same session name can
//! reach this module with an EQUAL generation key, and separating them then rests
//! on the nonce and transcript-inode legs. An incarnation that also reused the
//! transcript inode and the nonce would compare equal here. This module only
//! READS the stamp; it does not make it monotone.
//!
//! # Who reads this, and what it may move
//!
//! #5071 T4-B6 wired the intake. [`super::composite::observe_relay_verdict`] is
//! the only caller outside this module's own tests: it resolves
//! [`external_verdict_path`] for the channel and hands that file to
//! [`classify_external_verdict_at`] with the incarnation the T4-B2c ledger
//! framed. It reaches this module only when that ledger read succeeded — a
//! channel with no readable ledger contributes
//! [`ExternalRelayVerdict::Unknown`] without the sidecar being opened at all.
//! `health/snapshot.rs` runs that composition once per mailbox channel while it
//! builds a DETAIL snapshot, so this code executes on the detail poll and
//! nowhere else in production.
//!
//! What the resulting [`ExternalRelayVerdict`] is then allowed to do depends on
//! the 4987 §5.1 `RelayVerdictSource` switch that `health/snapshot.rs` reads:
//!
//! * Under `Structural` — the compiled default — the composed verdict is
//!   published on the detail surface and changes no health polarity. The values
//!   this module produces are observable there and move nothing.
//! * Under `Composite` a composed verdict that does not permit health adds a
//!   `relay_verdict_*` entry to the snapshot's degraded reasons and worsens its
//!   status to `Degraded`. This module's value reaches that outcome whenever
//!   [`super::composite::compose_relay_verdict`] takes it as the deciding tier,
//!   which happens only on a STRICTLY worse rung than the in-band verdict.
//!
//! # What this still does not do
//!
//! * It composes nothing. 4987 §4.3-1's `RelayVerdict = worst(ReachabilityVerdict,
//!   ExternalRelayVerdict)` lives in [`super::composite`], and
//!   [`ExternalRelayVerdict`] is deliberately a separate type from
//!   [`super::verdict::ReachabilityVerdict`] so a caller cannot reach a final
//!   health verdict through this module alone.
//! * It authorizes no redelivery and no destructive action. The polarity above is
//!   the whole of the authority T4-B6 gave it: nothing here or in
//!   [`super::composite`] cancels a turn, kills a pane or process, or force-cleans
//!   a mailbox or in-flight row, and
//!   [`super::composite::RelayVerdict::authorizes_destructive_action`] returns
//!   false for the external tier by an exhaustive match rather than by
//!   convention. 4987 §7.1 / I15 keep destructive actions out of this tree by the
//!   convention plus source lint `super` describes.
//! * It never asserts health. §5.3 gives the out-of-band tier the power to assert
//!   failure and not the power to assert it away, which is why `ok` arrives here
//!   as [`ExternalRelayVerdict::NoLoss`] rather than as `Reachable`, and why
//!   `compose_relay_verdict` lets this tier displace the in-band one only
//!   upward.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::ledger::LedgerIncarnation;
use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Sidecar subtree of 4987 §5.2 — a sibling of `discord_reachability_ledger/`.
/// Host-local, therefore NOT a cluster authority (4987 §-1.5): a peer node reads
/// whatever its own watchdog published under its own runtime root, not this one.
const EXTERNAL_VERDICT_DIR: &str = "discord_external_relay_verdicts";

/// The `watchdog_state_version` this reader accepts. A record stamped with any
/// other version is not read rather than being migrated.
const EXTERNAL_VERDICT_STATE_VERSION: u32 = 1;

/// The only `source` this reader accepts. Mirrors
/// `relay_watchdog.py::EXTERNAL_VERDICT_SOURCE`.
const EXTERNAL_VERDICT_SOURCE: &str = "relay_watchdog";

/// Wire spellings of the `verdict` field. These ARE the format, so they live in
/// one table on each side; the Python half is
/// `relay_watchdog.py::EXTERNAL_VERDICT_BY_STATE`.
const VERDICT_OK: &str = "ok";
const VERDICT_DEGRADED: &str = "degraded";
const VERDICT_UNREACHABLE: &str = "unreachable";
const VERDICT_UNKNOWN: &str = "unknown";

/// What the external (Tier B) tier is entitled to say — 4987 §4.3 / §5.2.
///
/// A separate type from [`super::verdict::ReachabilityVerdict`] on purpose: the
/// watchdog matches normalized text against a bounded channel read, and §-1.4
/// requires positive incarnation-alive evidence before anything may spell
/// `Reachable`. Nothing here can spell it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum ExternalRelayVerdict {
    /// The tier said nothing usable — §-1.5 ①. Composition must leave the
    /// in-band verdict exactly as it was.
    Unknown,
    /// The watchdog matched every stale block it framed (watchdog `ok`). §5.3:
    /// this is not health evidence, and [`worsen`] cannot improve anything with
    /// it.
    NoLoss,
    /// Unmatched blocks exist but the last matched delivery is recent (watchdog
    /// `lagging`).
    Lagging { lost_blocks: u32 },
    /// Unmatched blocks exist and the last matched delivery is older than the
    /// watchdog's own gap bound (watchdog `gap`).
    Unreachable { lost_blocks: u32 },
}

impl ExternalRelayVerdict {
    /// Rank used by [`worsen`], or `None` when the tier said nothing.
    ///
    /// Spelled out per variant instead of derived from declaration order: a
    /// reordered enum must not silently reorder the authority, and a new variant
    /// is a compile error here rather than an unranked value that folds away.
    fn severity(self) -> Option<u8> {
        match self {
            Self::Unknown => None,
            Self::NoLoss => Some(0),
            Self::Lagging { .. } => Some(1),
            Self::Unreachable { .. } => Some(2),
        }
    }
}

/// Fold a freshly read external verdict into the one already held for the SAME
/// incarnation — 4987 §4.3-2 / §-1.5 ③.
///
/// Within one incarnation the external tier may move toward failure only. An
/// `observed` of equal or lower severity is dropped, so a watchdog that reports
/// `ok` after a `gap` does not lift its own earlier claim. `Unknown` neither
/// displaces a held claim nor becomes one.
///
/// The caller owns the incarnation scoping: this function compares severities
/// and does not look at identity. Pass `held = Unknown` when the incarnation
/// changed, or the previous incarnation's claim survives into the new one.
pub(in crate::services::discord) fn worsen(
    held: ExternalRelayVerdict,
    observed: ExternalRelayVerdict,
) -> ExternalRelayVerdict {
    match (held.severity(), observed.severity()) {
        (Some(held_rank), Some(observed_rank)) if observed_rank > held_rank => observed,
        (None, Some(_)) => observed,
        _ => held,
    }
}

/// How one sidecar read classified.
///
/// The absent/faulted split is the same shape `super::ledger::ledger_file_exists`
/// exists for: "the watchdog has not published for this channel yet" and "a
/// published file will not parse" are different facts, and merging them would
/// let a fault read as a quiet first sight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum ExternalVerdictIntake {
    /// No file at that path.
    Absent,
    /// A file exists but does not parse, or carries another `source`,
    /// `watchdog_state_version`, or `verdict` spelling.
    Unreadable,
    /// `read_complete` was false, or a required identity leg was absent. Both
    /// mean the record cannot be gated, which §-1.5 ① resolves as no authority.
    Incomplete,
    /// The record frames an incarnation other than the caller's.
    WrongIncarnation,
    /// `watchdog_epoch` regressed below the epoch the caller already accepted.
    /// The watchdog's epoch does not survive the loss of its own state file, so
    /// this arm is reachable in production; it costs the tier its authority
    /// rather than granting it a stale one.
    Stale {
        accepted_epoch: u64,
        record_epoch: u64,
    },
    /// Identity-matched, complete and non-regressed.
    Usable {
        verdict: ExternalRelayVerdict,
        watchdog_epoch: u64,
    },
}

impl ExternalVerdictIntake {
    /// What the external tier is entitled to say from this read.
    ///
    /// Every arm but `Usable` is [`ExternalRelayVerdict::Unknown`]: §-1.5 ①, ②
    /// and ③ all mean "no authority". The arms are spelled out rather than
    /// collapsed to a catch-all so a new classification has to choose.
    pub(in crate::services::discord) fn verdict(&self) -> ExternalRelayVerdict {
        match self {
            Self::Usable { verdict, .. } => *verdict,
            Self::Absent
            | Self::Unreadable
            | Self::Incomplete
            | Self::WrongIncarnation
            | Self::Stale { .. } => ExternalRelayVerdict::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct SidecarFileId {
    dev: u64,
    ino: u64,
}

/// The sidecar record. Unknown keys are ignored, which is what makes §-1.5's
/// additive schema change safe in both directions.
#[derive(Debug, Clone, Deserialize)]
struct ExternalVerdictRecord {
    verdict: String,
    source: String,
    watchdog_state_version: u32,
    #[serde(default)]
    lost_blocks: u32,
    #[serde(default)]
    generation_mtime_ns: Option<i64>,
    #[serde(default)]
    spawn_nonce: Option<String>,
    #[serde(default)]
    transcript_file_id: Option<SidecarFileId>,
    #[serde(default)]
    watchdog_epoch: u64,
    #[serde(default)]
    read_complete: bool,
}

fn external_verdict_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(EXTERNAL_VERDICT_DIR))
}

/// `<runtime_root>/discord_external_relay_verdicts/<provider>/<channel_id>.json`.
pub(in crate::services::discord) fn external_verdict_path(
    provider: &ProviderKind,
    channel_id: u64,
) -> Option<PathBuf> {
    external_verdict_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

fn parse_verdict(record: &ExternalVerdictRecord) -> Option<ExternalRelayVerdict> {
    match record.verdict.as_str() {
        VERDICT_OK => Some(ExternalRelayVerdict::NoLoss),
        VERDICT_DEGRADED => Some(ExternalRelayVerdict::Lagging {
            lost_blocks: record.lost_blocks,
        }),
        VERDICT_UNREACHABLE => Some(ExternalRelayVerdict::Unreachable {
            lost_blocks: record.lost_blocks,
        }),
        // The writer's own fail-closed spelling for a state it could not map.
        VERDICT_UNKNOWN => Some(ExternalRelayVerdict::Unknown),
        // An unrecognized spelling is not silently the mildest one.
        _ => None,
    }
}

/// Whether the record frames `expected`, or `None` when a required leg is absent.
///
/// Compares the transcript `(dev, ino)`, the `.generation` stamp and the spawn
/// nonce. The sidecar carries no tmux session name, so this gate does not
/// distinguish two sessions that shared a transcript inode AND a generation
/// stamp AND a spawn nonce; a session name was not among the fields §-1.5 added,
/// and inventing one here would fork the schema.
///
/// A record `spawn_nonce` of `null` does not match an expected `Some`: 4987
/// §-1.3 forbids forging the marker's absence into a wildcard, and
/// `super::ledger::LedgerIncarnation` states the same rule for its own field. An
/// absent JSON key and an explicit `null` are the same value here — both mean no
/// nonce marker was readable.
///
/// A non-positive `generation_mtime_ns` counts as absent, matching
/// `super::observation::capture_watcher_incarnation`, which refuses to build an
/// incarnation from one.
fn frames_incarnation(
    record: &ExternalVerdictRecord,
    expected: &LedgerIncarnation,
) -> Option<bool> {
    let generation_mtime_ns = record.generation_mtime_ns.filter(|ns| *ns > 0)?;
    let file_id = record.transcript_file_id.as_ref()?;
    let identity = expected.identity();
    Some(
        generation_mtime_ns == expected.generation_mtime_ns
            && record.spawn_nonce == expected.spawn_nonce
            && file_id.dev == identity.dev
            && file_id.ino == identity.ino,
    )
}

/// Read and classify one sidecar against the caller's incarnation.
///
/// `accepted_epoch` is the `watchdog_epoch` the caller has already accepted FOR
/// `expected`; pass `None` for a first sight of this incarnation. Carrying an
/// epoch across an incarnation change would make a fresh incarnation's first
/// publish look stale, which is why the caller scopes it.
///
/// Opens one file and reads no clock. It takes no lock: the writer publishes by
/// atomic rename, so a concurrent reader sees the old bytes or the new bytes.
pub(in crate::services::discord) fn classify_external_verdict_at(
    path: &Path,
    expected: &LedgerIncarnation,
    accepted_epoch: Option<u64>,
) -> ExternalVerdictIntake {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ExternalVerdictIntake::Absent;
        }
        Err(_) => return ExternalVerdictIntake::Unreadable,
    };
    let Ok(record) = serde_json::from_str::<ExternalVerdictRecord>(&raw) else {
        return ExternalVerdictIntake::Unreadable;
    };
    if record.source != EXTERNAL_VERDICT_SOURCE
        || record.watchdog_state_version != EXTERNAL_VERDICT_STATE_VERSION
    {
        return ExternalVerdictIntake::Unreadable;
    }
    let Some(verdict) = parse_verdict(&record) else {
        return ExternalVerdictIntake::Unreadable;
    };
    if !record.read_complete {
        return ExternalVerdictIntake::Incomplete;
    }
    match frames_incarnation(&record, expected) {
        None => ExternalVerdictIntake::Incomplete,
        Some(false) => ExternalVerdictIntake::WrongIncarnation,
        Some(true) => match accepted_epoch {
            Some(accepted) if record.watchdog_epoch < accepted => ExternalVerdictIntake::Stale {
                accepted_epoch: accepted,
                record_epoch: record.watchdog_epoch,
            },
            _ => ExternalVerdictIntake::Usable {
                verdict,
                watchdog_epoch: record.watchdog_epoch,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::health::reachability::discovery::TranscriptFileId;
    use std::path::PathBuf;
    use tempfile::TempDir;

    const GENERATION: i64 = 1_785_321_000_000_000_000;

    fn incarnation() -> LedgerIncarnation {
        LedgerIncarnation::new(
            "AgentDesk-claude-adk-cc".to_string(),
            GENERATION,
            Some("nonce-a".to_string()),
            TranscriptFileId {
                dev: 17,
                ino: 4_242,
            },
        )
    }

    /// The full published shape, so every test mutates one field of a record
    /// that would otherwise be `Usable`.
    fn record_json(overrides: &[(&str, &str)]) -> String {
        let mut fields: Vec<(String, String)> = vec![
            ("verdict".into(), "\"unreachable\"".into()),
            ("observed_at_epoch_ms".into(), "1785321543885".into()),
            ("source".into(), "\"relay_watchdog\"".into()),
            ("watchdog_state_version".into(), "1".into()),
            ("reason".into(), "\"gap\"".into()),
            ("lost_blocks".into(), "3".into()),
            ("last_delivered_ts".into(), "1785320700000".into()),
            ("generation_mtime_ns".into(), GENERATION.to_string()),
            ("spawn_nonce".into(), "\"nonce-a\"".into()),
            (
                "transcript_file_id".into(),
                "{\"dev\": 17, \"ino\": 4242}".into(),
            ),
            ("watchdog_epoch".into(), "7".into()),
            ("read_complete".into(), "true".into()),
        ];
        for (key, value) in overrides {
            match fields.iter_mut().find(|(name, _)| name == key) {
                Some(entry) => entry.1 = (*value).to_string(),
                None => fields.push(((*key).to_string(), (*value).to_string())),
            }
        }
        let body = fields
            .iter()
            .map(|(key, value)| format!("  \"{key}\": {value}"))
            .collect::<Vec<_>>()
            .join(",\n");
        format!("{{\n{body}\n}}")
    }

    fn write(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("sidecar.json");
        fs::write(&path, body).expect("write sidecar fixture");
        path
    }

    fn classify(overrides: &[(&str, &str)], accepted_epoch: Option<u64>) -> ExternalVerdictIntake {
        let dir = TempDir::new().expect("tempdir");
        let path = write(&dir, &record_json(overrides));
        classify_external_verdict_at(&path, &incarnation(), accepted_epoch)
    }

    #[test]
    fn a_complete_same_incarnation_record_is_usable() {
        assert_eq!(
            classify(&[], None),
            ExternalVerdictIntake::Usable {
                verdict: ExternalRelayVerdict::Unreachable { lost_blocks: 3 },
                watchdog_epoch: 7,
            }
        );
    }

    #[test]
    fn an_incomplete_read_carries_no_authority() {
        let intake = classify(&[("read_complete", "false")], None);
        assert_eq!(intake, ExternalVerdictIntake::Incomplete);
        assert_eq!(intake.verdict(), ExternalRelayVerdict::Unknown);
    }

    #[test]
    fn a_missing_read_complete_key_is_incomplete() {
        // The key defaults to false rather than to "the writer must have read
        // everything", so a pre-S6 record cannot acquire authority by omission.
        let dir = TempDir::new().expect("tempdir");
        let path = write(
            &dir,
            r#"{"verdict": "unreachable", "source": "relay_watchdog",
                "watchdog_state_version": 1, "reason": "gap", "lost_blocks": 3}"#,
        );
        let intake = classify_external_verdict_at(&path, &incarnation(), None);
        assert_eq!(intake, ExternalVerdictIntake::Incomplete);
        assert_eq!(intake.verdict(), ExternalRelayVerdict::Unknown);
    }

    #[test]
    fn each_absent_identity_leg_is_incomplete_not_a_mismatch() {
        for override_field in [
            ("generation_mtime_ns", "null"),
            ("generation_mtime_ns", "0"),
            ("transcript_file_id", "null"),
        ] {
            assert_eq!(
                classify(&[override_field], None),
                ExternalVerdictIntake::Incomplete,
                "leg {override_field:?}"
            );
        }
    }

    #[test]
    fn a_wrong_incarnation_is_ignored_on_every_identity_leg() {
        for override_field in [
            ("generation_mtime_ns", "1785321000000000001"),
            ("spawn_nonce", "\"nonce-b\""),
            ("spawn_nonce", "null"),
            ("transcript_file_id", "{\"dev\": 17, \"ino\": 4243}"),
            ("transcript_file_id", "{\"dev\": 18, \"ino\": 4242}"),
        ] {
            let intake = classify(&[override_field], None);
            assert_eq!(
                intake,
                ExternalVerdictIntake::WrongIncarnation,
                "leg {override_field:?}"
            );
            assert_eq!(intake.verdict(), ExternalRelayVerdict::Unknown);
        }
    }

    #[test]
    fn a_regressed_watchdog_epoch_is_ignored() {
        let intake = classify(&[("watchdog_epoch", "1")], Some(7));
        assert_eq!(
            intake,
            ExternalVerdictIntake::Stale {
                accepted_epoch: 7,
                record_epoch: 1,
            }
        );
        assert_eq!(intake.verdict(), ExternalRelayVerdict::Unknown);
    }

    #[test]
    fn a_republished_or_advanced_epoch_stays_usable() {
        for epoch in ["7", "8"] {
            assert!(
                matches!(
                    classify(&[("watchdog_epoch", epoch)], Some(7)),
                    ExternalVerdictIntake::Usable { .. }
                ),
                "epoch {epoch}"
            );
        }
    }

    #[test]
    fn a_malformed_or_foreign_sidecar_carries_no_authority() {
        let dir = TempDir::new().expect("tempdir");
        for body in [
            "{".to_string(),
            "null".to_string(),
            "[]".to_string(),
            record_json(&[("source", "\"someone_else\"")]),
            record_json(&[("watchdog_state_version", "2")]),
            record_json(&[("verdict", "\"probably_fine\"")]),
        ] {
            let path = write(&dir, &body);
            let intake = classify_external_verdict_at(&path, &incarnation(), None);
            assert_eq!(intake, ExternalVerdictIntake::Unreadable, "body {body}");
            assert_eq!(intake.verdict(), ExternalRelayVerdict::Unknown);
        }
    }

    #[test]
    fn the_writers_own_unknown_spelling_is_readable_but_silent() {
        let intake = classify(&[("verdict", "\"unknown\"")], None);
        assert_eq!(
            intake,
            ExternalVerdictIntake::Usable {
                verdict: ExternalRelayVerdict::Unknown,
                watchdog_epoch: 7,
            }
        );
        assert_eq!(intake.verdict(), ExternalRelayVerdict::Unknown);
    }

    #[test]
    fn an_absent_sidecar_is_distinct_from_a_broken_one() {
        let dir = TempDir::new().expect("tempdir");
        let missing = dir.path().join("never-published.json");
        assert_eq!(
            classify_external_verdict_at(&missing, &incarnation(), None),
            ExternalVerdictIntake::Absent
        );
        let broken = write(&dir, "{\"verdict\": ");
        assert_eq!(
            classify_external_verdict_at(&broken, &incarnation(), None),
            ExternalVerdictIntake::Unreadable
        );
    }

    #[test]
    fn a_directory_in_place_of_a_sidecar_is_unreadable_not_absent() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("sidecar.json");
        fs::create_dir(&path).expect("create directory in the sidecar's place");
        assert_eq!(
            classify_external_verdict_at(&path, &incarnation(), None),
            ExternalVerdictIntake::Unreadable
        );
    }

    #[test]
    fn the_external_tier_can_only_worsen_within_one_incarnation() {
        let unreachable = ExternalRelayVerdict::Unreachable { lost_blocks: 3 };
        let lagging = ExternalRelayVerdict::Lagging { lost_blocks: 1 };
        // Worsening is accepted in both steps of the ladder.
        assert_eq!(
            worsen(ExternalRelayVerdict::NoLoss, lagging),
            lagging,
            "ok -> lagging"
        );
        assert_eq!(
            worsen(lagging, unreachable),
            unreachable,
            "lagging -> unreachable"
        );
        // Improvement is dropped at every rung.
        assert_eq!(
            worsen(unreachable, ExternalRelayVerdict::NoLoss),
            unreachable,
            "unreachable -> ok"
        );
        assert_eq!(
            worsen(unreachable, lagging),
            unreachable,
            "unreachable -> lagging"
        );
        assert_eq!(worsen(lagging, ExternalRelayVerdict::NoLoss), lagging);
    }

    #[test]
    fn an_equal_severity_republish_does_not_replace_the_held_payload() {
        // Same rung, different payload: the held claim wins, so a re-read cannot
        // walk `lost_blocks` down while the rung stays put.
        let held = ExternalRelayVerdict::Unreachable { lost_blocks: 3 };
        assert_eq!(
            worsen(held, ExternalRelayVerdict::Unreachable { lost_blocks: 1 }),
            held
        );
    }

    #[test]
    fn unknown_neither_displaces_a_held_claim_nor_becomes_one() {
        let held = ExternalRelayVerdict::Unreachable { lost_blocks: 3 };
        assert_eq!(worsen(held, ExternalRelayVerdict::Unknown), held);
        assert_eq!(
            worsen(ExternalRelayVerdict::Unknown, ExternalRelayVerdict::Unknown),
            ExternalRelayVerdict::Unknown
        );
        assert_eq!(
            worsen(ExternalRelayVerdict::Unknown, held),
            held,
            "a first sight still lands"
        );
    }

    #[test]
    fn the_sidecar_path_is_keyed_by_provider_and_channel() {
        let path = external_verdict_path(&ProviderKind::Claude, 1234)
            .expect("a runtime root exists under test");
        assert!(
            path.ends_with("discord_external_relay_verdicts/claude/1234.json"),
            "unexpected sidecar path {path:?}"
        );
    }
}
