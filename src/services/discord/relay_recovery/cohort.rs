//! #5464 (#5071 T5) S1 — relay-authority cohort admission and rollout
//! provenance.
//!
//! The AC2-R warrant (design r3 §1.1) is rolled out per channel behind two
//! `runtime.*` knobs: `relay_authority_mode` decides whether the warrant is
//! computed at all, and `relay_authority_cohort_percent` decides how much of
//! the channel population it applies to. Every future consumer asks the same
//! question here — `admits(mode, percent, channel_id)` — so a slice can never
//! grow a second, divergent notion of "is this channel in the cohort".
//!
//! **This slice admits nobody.** The shipped defaults are `Legacy` and `0`, and
//! `admits` is a conjunction, so either default alone answers `false` for every
//! channel. The only production reader in S1 is the health block below.
//!
//! The bucket function is deliberately NOT `DefaultHasher`/`RandomState`:
//! cohort membership has to mean the same thing in every process and across
//! every release, otherwise a restart reshuffles the cohort and the AC3
//! promotion window (≥7 days, design §5.3) never accumulates a stable
//! denominator. FNV-1a is written out here so the mapping is pinned by this
//! file rather than by a std implementation detail, and
//! `cohort_bucket_is_pinned_to_a_fixed_vector` fails if it ever moves.
//!
//! Uniformity is *measured* here, not asserted — r1 L-4 / r2 L-10 / r3 §8 L-12
//! carried "the bucket spread is a design claim and not a measurement" as an
//! open limit for three rounds. `cohort_bucket_spreads_snowflake_ids_across_all_buckets`
//! closes it for the Discord snowflake shape that actually reaches this function.

use serde::Serialize;

use crate::config::RelayAuthorityMode;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Stable `channel_id -> 0..100` bucket.
///
/// Discord snowflakes carry their timestamp in the HIGH bits and a per-shard
/// sequence in the LOW ones, so neither `id % 100` nor a byte-slice spreads
/// evenly — ids minted close together share high bytes, and a quiet shard's low
/// bytes barely move. Avalanching all eight through FNV-1a first earns the modulo.
pub(crate) fn cohort_bucket(channel_id: u64) -> u8 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in channel_id.to_be_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    (hash % 100) as u8
}

/// The single relay-authority cohort predicate.
///
/// Both operands are vetoes, and both defaults are the denying value: a mode
/// that does not consult the cohort is out regardless of the width, and a width
/// of `0` is out regardless of the mode (`bucket < 0` is false for every
/// bucket). `percent` is clamped rather than rejected so an out-of-range
/// operator value fails toward "everyone", which is visible in the health
/// block, instead of wrapping into a silently narrow cohort.
///
/// `dead_code` is allowed narrowly: S1 has **no** production caller by design —
/// that absence is what makes the slice a deployment no-op — and the attribute
/// keeps `cohort_bucket` and the three `RelayAuthorityMode` predicates
/// reachable, so the dormant chain adds nothing to the repo's dead-code debt.
/// S2 is the first caller; dropping this attribute belongs to that slice.
#[allow(dead_code)]
pub(crate) fn admits(mode: RelayAuthorityMode, percent: u8, channel_id: u64) -> bool {
    mode.consults_cohort() && cohort_bucket(channel_id) < percent.min(100)
}

/// Content fingerprint of the live cohort configuration (design §5.2).
///
/// `config_live_reload` keeps no generation counter (r3 §5.2, measured), so
/// rollout stages cannot be numbered monotonically. This fingerprints the
/// settings instead: two windows at the same dial position share a fingerprint
/// even if the operator moved the dial away and back (declared limit L-7 — the
/// promotion script separates such windows by file/timestamp order). Any knob
/// added to the cohort decision MUST join the canonical string below, or two
/// materially different rollout windows become indistinguishable in AC3.
pub(crate) fn cohort_fingerprint(mode: RelayAuthorityMode, percent: u8) -> String {
    let canonical = format!("mode={mode:?};percent={}", percent.min(100));
    let mut hash = FNV_OFFSET_BASIS;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Read-only rollout provenance for `/api/health/detail`.
///
/// Live triage only. The AC3 promotion gate reads the JSONL event log that a
/// later slice writes, never this block (design §5.3): a health poll is a
/// sample of *now* and cannot answer a 7-day window question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RelayAuthorityRolloutReport {
    /// The live mode, lowercased exactly as `agentdesk.yaml` spells it.
    pub(crate) mode: RelayAuthorityMode,
    /// The live cohort width AFTER the same clamp `admits` applies, so an
    /// operator reading this block sees the width that is actually in force
    /// rather than the raw value they typed.
    pub(crate) cohort_percent: u8,
    /// Fingerprint of the two fields above; a later slice's JSONL correlation key.
    pub(crate) cohort_fingerprint: String,
}

/// Build the rollout block from the live config.
///
/// A config that cannot be read at all reports the shipped defaults, which is
/// also the safe answer: an unreadable config is one that admits nobody.
pub(crate) fn rollout_report() -> RelayAuthorityRolloutReport {
    let (mode, percent) = crate::config_live_reload::current()
        .map(|config| {
            (
                config.runtime.relay_authority_mode,
                config.runtime.relay_authority_cohort_percent,
            )
        })
        .unwrap_or_default();
    RelayAuthorityRolloutReport {
        mode,
        cohort_percent: percent.min(100),
        cohort_fingerprint: cohort_fingerprint(mode, percent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [RelayAuthorityMode; 3] = [
        RelayAuthorityMode::Legacy,
        RelayAuthorityMode::Observe,
        RelayAuthorityMode::Enforce,
    ];

    /// Discord snowflakes for a plausible guild: one epoch base, incremented
    /// the way consecutively created channels actually are (the worker/process
    /// /sequence low bits move, the timestamp high bits barely do).
    fn snowflake_ids(count: u64) -> impl Iterator<Item = u64> {
        (0..count).map(|index| 1_234_567_890_123_456_789u64 + index * 4_194_304)
    }

    /// The S1 deployment no-op proof, stated as the property that makes it one:
    /// under the SHIPPED defaults no channel is in the cohort, so no consumer a
    /// later slice adds can take the new path without a config change.
    #[test]
    fn shipped_defaults_admit_no_channel_to_the_relay_authority_cohort() {
        let defaults = crate::config::RuntimeSettingsConfig::default();
        assert_eq!(defaults.relay_authority_mode, RelayAuthorityMode::Legacy);
        assert_eq!(defaults.relay_authority_cohort_percent, 0);

        for channel_id in snowflake_ids(5_000) {
            assert!(
                !admits(
                    defaults.relay_authority_mode,
                    defaults.relay_authority_cohort_percent,
                    channel_id,
                ),
                "channel {channel_id} admitted under shipped defaults"
            );
        }
    }

    /// Each operand vetoes on its own, so a half-configured rollout is still a
    /// no-op. Moving only the mode admits nobody, and moving only the width
    /// admits nobody.
    #[test]
    fn either_dial_left_at_its_default_admits_nobody() {
        for channel_id in snowflake_ids(1_000) {
            for mode in MODES {
                assert!(
                    !admits(mode, 0, channel_id),
                    "{mode:?} admitted {channel_id} at cohort width 0"
                );
            }
            for percent in [0u8, 1, 50, 99, 100, 255] {
                assert!(
                    !admits(RelayAuthorityMode::Legacy, percent, channel_id),
                    "Legacy admitted {channel_id} at cohort width {percent}"
                );
            }
        }
    }

    #[test]
    fn full_width_admits_every_channel_in_a_consuming_mode() {
        for channel_id in snowflake_ids(1_000) {
            for mode in [RelayAuthorityMode::Observe, RelayAuthorityMode::Enforce] {
                assert!(admits(mode, 100, channel_id));
                // Out-of-range widths clamp to 100 rather than wrapping.
                assert!(admits(mode, 255, channel_id));
            }
        }
    }

    /// Widening the dial may only add channels. A cohort that reshuffles as it
    /// grows invalidates every sample taken at the narrower width.
    #[test]
    fn admission_is_monotone_in_the_cohort_width() {
        for channel_id in snowflake_ids(200) {
            let mut previously_admitted = false;
            for percent in 0..=100u8 {
                let admitted = admits(RelayAuthorityMode::Observe, percent, channel_id);
                assert!(
                    admitted || !previously_admitted,
                    "channel {channel_id} left the cohort when it widened to {percent}"
                );
                previously_admitted = admitted;
            }
            assert!(previously_admitted);
        }
    }

    /// Closes design §8 L-12 ("the bucket spread is a claim, not a
    /// measurement") for the snowflake shape that reaches this function.
    ///
    /// The bound is deliberately loose — this asserts the hash avalanches, not
    /// that it is cryptographic. A `% 100` of the raw snowflake fails it
    /// outright: consecutive ids stride the low bits, so raw modulo lands on a
    /// handful of buckets.
    #[test]
    fn cohort_bucket_spreads_snowflake_ids_across_all_buckets() {
        const SAMPLES: u64 = 100_000;
        let expected = SAMPLES as f64 / 100.0;
        let mut counts = [0u32; 100];
        for channel_id in snowflake_ids(SAMPLES) {
            let bucket = cohort_bucket(channel_id);
            assert!(bucket < 100, "bucket {bucket} is out of range");
            counts[bucket as usize] += 1;
        }
        for (bucket, count) in counts.iter().enumerate() {
            let deviation = (f64::from(*count) - expected).abs() / expected;
            assert!(
                deviation < 0.25,
                "bucket {bucket} holds {count} of {SAMPLES} samples (expected ~{expected}); \
                 deviation {deviation:.3} exceeds the 0.25 uniformity bound"
            );
        }

        // A 10% cohort must actually be about 10% of the population, which is
        // the property the rollout plan reads the dial as promising.
        let admitted = snowflake_ids(SAMPLES)
            .filter(|id| admits(RelayAuthorityMode::Observe, 10, *id))
            .count();
        let share = admitted as f64 / SAMPLES as f64;
        assert!(
            (0.085..=0.115).contains(&share),
            "a 10% cohort admitted {share:.4} of the population"
        );
    }

    /// Cohort membership must survive a restart and a release. A changed hash
    /// silently re-rolls every channel mid-rollout, so it has to break a test
    /// instead.
    #[test]
    fn cohort_bucket_is_pinned_to_a_fixed_vector() {
        for (channel_id, expected) in [
            (0u64, 5u8),
            (1, 94),
            (1_234_567_890_123_456_789, 2),
            (u64::MAX, 57),
        ] {
            assert_eq!(
                cohort_bucket(channel_id),
                expected,
                "cohort bucket for {channel_id} moved; every channel's membership changed"
            );
        }
    }

    #[test]
    fn fingerprint_separates_dial_positions_and_repeats_for_equal_ones() {
        let mut seen = std::collections::HashSet::new();
        for mode in MODES {
            for percent in [0u8, 1, 50, 100] {
                assert!(
                    seen.insert(cohort_fingerprint(mode, percent)),
                    "{mode:?}/{percent} collided with another dial position"
                );
            }
        }
        assert_eq!(
            cohort_fingerprint(RelayAuthorityMode::Observe, 25),
            cohort_fingerprint(RelayAuthorityMode::Observe, 25)
        );
        // The clamp is part of the canonical form, so the two widths that mean
        // the same thing share one fingerprint.
        assert_eq!(
            cohort_fingerprint(RelayAuthorityMode::Observe, 100),
            cohort_fingerprint(RelayAuthorityMode::Observe, 255)
        );
        assert_eq!(cohort_fingerprint(RelayAuthorityMode::Legacy, 0).len(), 16);
    }

    /// With no live config loaded — the state a unit test and a very early
    /// startup share — the block must report the dormant dial rather than
    /// guessing.
    #[test]
    fn rollout_report_without_a_live_config_reports_the_dormant_dial() {
        let report = rollout_report();
        assert_eq!(report.mode, RelayAuthorityMode::Legacy);
        assert_eq!(report.cohort_percent, 0);
        assert_eq!(
            report.cohort_fingerprint,
            cohort_fingerprint(RelayAuthorityMode::Legacy, 0)
        );
        assert_eq!(
            serde_json::to_value(&report).expect("serialize rollout report"),
            serde_json::json!({
                "mode": "legacy",
                "cohort_percent": 0,
                "cohort_fingerprint": cohort_fingerprint(RelayAuthorityMode::Legacy, 0),
            })
        );
    }
}
