//! #5464 (#5071 T5) S1 — relay-authority cohort admission and rollout
//! provenance.
//!
//! Cohort membership behind `runtime.relay_authority_mode` and
//! `runtime.relay_authority_cohort_percent` is decided in exactly one place,
//! `admits(mode, percent, channel_id)`, so no consumer grows a second,
//! divergent notion of "is this channel in the cohort". Shipped defaults
//! (`Legacy`, `0`) admit no channel; the only production reader in S1 is the
//! health block below.
//!
//! `cohort_bucket` uses FNV-1a rather than `DefaultHasher`/`RandomState` so
//! cohort membership means the same thing across restarts and releases;
//! `cohort_bucket_is_pinned_to_a_fixed_vector` pins the mapping, and the
//! `cohort_bucket_spreads_*` tests measure its uniformity across two id
//! shapes (not a census of live channel ids).

use std::sync::atomic::{AtomicU16, Ordering};

use serde::Serialize;

use crate::config::RelayAuthorityMode;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Stable `channel_id -> 0..100` bucket.
///
/// Snowflakes carry timestamp bits high and a per-shard sequence low, so
/// neither `id % 100` nor a byte slice spreads evenly; FNV-1a avalanches all
/// eight bytes first to earn the modulo.
pub(crate) fn cohort_bucket(channel_id: u64) -> u8 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in channel_id.to_be_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    (hash % 100) as u8
}

/// The cohort width actually in force for a configured `percent`.
///
/// Fail-open: an out-of-range width clamps to "everyone" rather than a
/// silently narrow cohort (#5464 T5 S1 follow-up 1, #5071 T5 A6). Every
/// clamp site funnels through here so the width in force has exactly one
/// definition, matching what the health block publishes.
pub(crate) fn effective_cohort_percent(percent: u8) -> u8 {
    let effective = percent.min(100);
    if effective != percent {
        // One line per distinct out-of-range value: `admits` runs this per
        // channel, so an unconditional warn would spam per admission check.
        static LAST_WARNED: AtomicU16 = AtomicU16::new(u16::MAX);
        if LAST_WARNED.swap(u16::from(percent), Ordering::Relaxed) != u16::from(percent) {
            tracing::warn!(
                configured = percent,
                effective,
                "relay_authority_cohort_percent out of range; clamped to full cohort"
            );
        }
    }
    effective
}

/// The single relay-authority cohort predicate.
///
/// Both operands are vetoes and both defaults deny: a mode that doesn't
/// consult the cohort is out regardless of width, and a width of `0` is out
/// regardless of mode. `percent` is clamped via `effective_cohort_percent`
/// before the comparison.
pub(crate) fn admits(mode: RelayAuthorityMode, percent: u8, channel_id: u64) -> bool {
    mode.consults_cohort() && cohort_bucket(channel_id) < effective_cohort_percent(percent)
}

/// The relay-authority cohort question for a call site that ENFORCES.
///
/// The mode predicate is `governs_destructive_authority`, not
/// `records_authority_observations`: `Observe` must stay behaviour-identical
/// to `Legacy` for every consumer that is not the AC3 recorder. Callers read
/// this ONCE per decision and pass the answer down.
pub(crate) fn enforcement_admits(channel_id: u64) -> bool {
    let (mode, percent) = crate::config_live_reload::current()
        .map(|config| {
            (
                config.runtime.relay_authority_mode,
                config.runtime.relay_authority_cohort_percent,
            )
        })
        .unwrap_or_default();
    mode.governs_destructive_authority() && admits(mode, percent, channel_id)
}

/// Content fingerprint of the live cohort configuration (design §5.2).
///
/// `config_live_reload` keeps no generation counter, so rollout windows are
/// correlated by this fingerprint instead of a monotonic stage number;
/// `segment_events` in `scripts/relay_authority_rollout_report.py` separates
/// same-fingerprint windows via interleaved samples carrying a different one.
///
/// Host-independent by design, so a part-way rollout across hosts shreds the
/// window rather than silently merging it (legA r3c P2-3). Any knob added to
/// the cohort decision MUST join the canonical string below.
pub(crate) fn cohort_fingerprint(mode: RelayAuthorityMode, percent: u8) -> String {
    let canonical = format!(
        "mode={mode:?};percent={}",
        effective_cohort_percent(percent)
    );
    let mut hash = FNV_OFFSET_BASIS;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Read-only rollout provenance for `/api/health/detail`.
///
/// Live triage only; the AC3 promotion gate reads the JSONL event log a
/// later slice writes, never this block (design §5.3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RelayAuthorityRolloutReport {
    /// The live mode, lowercased exactly as `agentdesk.yaml` spells it.
    pub(crate) mode: RelayAuthorityMode,
    /// The live cohort width AFTER the same clamp `admits` applies, so an
    /// operator reading this block sees the width that is actually in force.
    pub(crate) cohort_percent: u8,
    /// The width exactly as `agentdesk.yaml` spells it, BEFORE the clamp.
    pub(crate) cohort_percent_configured: u8,
    /// `true` exactly when the two widths above differ. Published so an
    /// operator alert can watch it without re-encoding the clamp rule.
    pub(crate) cohort_percent_clamped: bool,
    /// Fingerprint of the two fields above; a later slice's JSONL correlation key.
    pub(crate) cohort_fingerprint: String,
}

/// Build the rollout block from the live config.
///
/// Before `config_live_reload::install` publishes the boot config, this
/// reports the shipped `Legacy/0` dial. A config that fails to parse instead
/// keeps the last-known-good snapshot via `reload_from_path` — fail-stale,
/// not fail-closed.
pub(crate) fn rollout_report() -> RelayAuthorityRolloutReport {
    let (mode, percent) = crate::config_live_reload::current()
        .map(|config| {
            (
                config.runtime.relay_authority_mode,
                config.runtime.relay_authority_cohort_percent,
            )
        })
        .unwrap_or_default();
    rollout_report_for(mode, percent)
}

/// The report builder, split from the live-config read above so a test can
/// drive a dial position this process isn't actually running under.
fn rollout_report_for(mode: RelayAuthorityMode, percent: u8) -> RelayAuthorityRolloutReport {
    let effective = effective_cohort_percent(percent);
    RelayAuthorityRolloutReport {
        mode,
        cohort_percent: effective,
        cohort_percent_configured: percent,
        cohort_percent_clamped: effective != percent,
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

    /// One plausible guild's epoch base for both id fixtures below.
    const SNOWFLAKE_BASE: u64 = 1_234_567_890_123_456_789;
    /// Width of a snowflake's worker + process + sequence fields; everything
    /// above it is the timestamp.
    const LOW_22_MASK: u64 = (1 << 22) - 1;

    /// Discord snowflakes strided by `2^22` (worker/process/sequence field
    /// width), so each sample advances only the TIMESTAMP field while the low
    /// bits hold still — the harder shape for the hash, since every bit the
    /// modulo could key on sits in the high half. `low_bit_ids` complements.
    fn snowflake_ids(count: u64) -> impl Iterator<Item = u64> {
        (0..count).map(|index| SNOWFLAKE_BASE + index * (LOW_22_MASK + 1))
    }

    /// The complement of `snowflake_ids`: consecutive ids from the same base,
    /// so the low 22 bits carry all the variation and the timestamp field
    /// never advances.
    fn low_bit_ids(count: u64) -> impl Iterator<Item = u64> {
        (0..count).map(|index| SNOWFLAKE_BASE + index)
    }

    /// Under the shipped dial, `enforcement_admits` answers `false` for every
    /// channel, so the watcher's rowless soft-terminal relaxation cannot be
    /// taken without a config change. `Observe` alone must not be enough
    /// either, or it would change the behaviour the AC3 evidence describes.
    #[test]
    fn shipped_defaults_admit_no_channel_to_the_enforcement_cohort() {
        let defaults = crate::config::RuntimeSettingsConfig::default();
        assert_eq!(defaults.relay_authority_mode, RelayAuthorityMode::Legacy);
        assert_eq!(defaults.relay_authority_cohort_percent, 0);
        assert!(
            !RelayAuthorityMode::Observe.governs_destructive_authority(),
            "the observing mode must not be able to enforce",
        );
        for channel_id in snowflake_ids(2_000) {
            assert!(
                !enforcement_admits(channel_id),
                "channel {channel_id} was admitted to the enforcement cohort by the shipped dial"
            );
        }
    }

    /// Under the shipped defaults no channel is in the cohort, so no consumer
    /// a later slice adds can take the new path without a config change.
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

    /// Each operand vetoes alone, so a half-configured rollout is still a
    /// no-op: moving only the mode, or only the width, admits nobody.
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

    /// Closes design §8 L-12 ("bucket spread is a claim, not a measurement")
    /// for the timestamp-strided shape `snowflake_ids` builds.
    ///
    /// Loose bound — asserts the hash avalanches, not that it's cryptographic.
    /// A raw `% 100` fails outright: stride `2^22`, and `2^22 % 100 == 4`, so
    /// a raw modulo reaches only 25 of 100 buckets.
    #[test]
    fn cohort_bucket_spreads_snowflake_ids_across_all_buckets() {
        const SAMPLES: u64 = 100_000;
        let expected = SAMPLES as f64 / 100.0;
        let mut counts = [0u32; 100];
        for (index, channel_id) in snowflake_ids(SAMPLES).enumerate() {
            // Assert the fixture's own shape rather than merely describing it.
            assert_eq!(
                channel_id & LOW_22_MASK,
                SNOWFLAKE_BASE & LOW_22_MASK,
                "sample {index} moved the low 22 bits; this fixture must vary the timestamp only"
            );
            assert_eq!(
                channel_id >> 22,
                (SNOWFLAKE_BASE >> 22) + index as u64,
                "sample {index} did not advance the timestamp field by exactly one tick"
            );
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

        // A 10% cohort must actually admit about 10% of the population.
        let admitted = snowflake_ids(SAMPLES)
            .filter(|id| admits(RelayAuthorityMode::Observe, 10, *id))
            .count();
        let share = admitted as f64 / SAMPLES as f64;
        assert!(
            (0.085..=0.115).contains(&share),
            "a 10% cohort admitted {share:.4} of the population"
        );
    }

    /// Complement of `cohort_bucket_spreads_snowflake_ids_across_all_buckets`:
    /// ids sharing one timestamp tick, varying only in the low bits. Widens
    /// coverage rather than discriminating — a raw `% 100` also spreads
    /// consecutive ids evenly.
    #[test]
    fn cohort_bucket_spreads_ids_that_move_only_in_the_low_bits() {
        const SAMPLES: u64 = 100_000;
        let expected = SAMPLES as f64 / 100.0;
        let mut counts = [0u32; 100];
        for (index, channel_id) in low_bit_ids(SAMPLES).enumerate() {
            assert_eq!(
                channel_id >> 22,
                SNOWFLAKE_BASE >> 22,
                "sample {index} left the fixture's single timestamp tick"
            );
            assert_eq!(
                channel_id & LOW_22_MASK,
                (SNOWFLAKE_BASE & LOW_22_MASK) + index as u64,
                "sample {index} did not advance the low 22 bits by exactly one"
            );
            counts[cohort_bucket(channel_id) as usize] += 1;
        }
        for (bucket, count) in counts.iter().enumerate() {
            let deviation = (f64::from(*count) - expected).abs() / expected;
            assert!(
                deviation < 0.25,
                "bucket {bucket} holds {count} of {SAMPLES} low-bit samples \
                 (expected ~{expected}); deviation {deviation:.3} exceeds the 0.25 bound"
            );
        }
        let admitted = low_bit_ids(SAMPLES)
            .filter(|id| admits(RelayAuthorityMode::Observe, 10, *id))
            .count();
        let share = admitted as f64 / SAMPLES as f64;
        assert!(
            (0.085..=0.115).contains(&share),
            "a 10% cohort admitted {share:.4} of the low-bit population"
        );
    }

    /// Cohort membership must survive a restart and a release; a changed hash
    /// must break this test instead of silently re-rolling every channel.
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

    /// Design §5.2 makes this string the JSONL window key; it interpolates
    /// the derived `Debug` (`mode=Legacy`), not the serde spelling
    /// (`"legacy"`) — a switch to `Display` would silently re-key the fleet.
    #[test]
    fn cohort_fingerprint_is_pinned_to_a_fixed_vector() {
        for (mode, percent, expected) in [
            (RelayAuthorityMode::Legacy, 0u8, "18a16fbe4259fa89"),
            (RelayAuthorityMode::Observe, 25, "5ec9884a77557ba9"),
            (RelayAuthorityMode::Enforce, 100, "d1d48477e7e326bd"),
        ] {
            assert_eq!(
                cohort_fingerprint(mode, percent),
                expected,
                "the {mode:?}/{percent} fingerprint moved; every rollout window \
                 emitted under the old canonical form loses its correlation key"
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

    /// With no live config loaded, the block must report the dormant dial
    /// rather than guessing.
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
                "cohort_percent_configured": 0,
                "cohort_percent_clamped": false,
                "cohort_fingerprint": cohort_fingerprint(RelayAuthorityMode::Legacy, 0),
            })
        );
    }

    /// The clamp stays and both widths are published (#5464 T5 S1 follow-up 1).
    /// `u8` parsing refuses `256+`, so `101..=255` is the entire reachable
    /// typo space and every value collapses to the same cohort — why the
    /// pre-clamp value must be carried separately.
    #[test]
    fn rollout_report_publishes_the_configured_width_beside_the_clamped_one() {
        for percent in [101u8, 200, 255] {
            let report = rollout_report_for(RelayAuthorityMode::Enforce, percent);
            assert_eq!(
                report.cohort_percent, 100,
                "the effective width of {percent} is the clamped one"
            );
            assert_eq!(
                report.cohort_percent_configured, percent,
                "the configured width must survive the clamp into the health block"
            );
            assert!(
                report.cohort_percent_clamped,
                "a configured width of {percent} was altered and must say so"
            );
            assert_eq!(
                serde_json::to_value(&report).expect("serialize rollout report"),
                serde_json::json!({
                    "mode": "enforce",
                    "cohort_percent": 100,
                    "cohort_percent_configured": percent,
                    "cohort_percent_clamped": true,
                    "cohort_fingerprint": cohort_fingerprint(RelayAuthorityMode::Enforce, percent),
                }),
                "the published block must carry the configured width and the clamp flag"
            );
        }

        // In-range widths must not be flagged as clamped.
        for percent in [0u8, 1, 25, 99, 100] {
            let report = rollout_report_for(RelayAuthorityMode::Observe, percent);
            assert_eq!(report.cohort_percent, percent);
            assert_eq!(report.cohort_percent_configured, percent);
            assert!(
                !report.cohort_percent_clamped,
                "an in-range width of {percent} must not be flagged as clamped"
            );
        }
    }

    /// The published effective width and the width `admits` gates on are the
    /// same number, in and out of the typo band. A second clamp rule
    /// elsewhere would let the health block disagree with the live cohort.
    #[test]
    fn the_published_effective_width_is_the_width_admission_gates_on() {
        for percent in [0u8, 1, 25, 99, 100, 101, 200, 255] {
            let report = rollout_report_for(RelayAuthorityMode::Observe, percent);
            let published = report.cohort_percent;
            for channel_id in snowflake_ids(500) {
                assert_eq!(
                    admits(RelayAuthorityMode::Observe, percent, channel_id),
                    cohort_bucket(channel_id) < published,
                    "channel {channel_id} admission disagreed with the published \
                     effective width {published} (configured {percent})"
                );
            }
        }
    }
}
