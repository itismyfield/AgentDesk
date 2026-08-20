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
//! open limit for three rounds. Two fixtures close it, each named for the
//! snowflake field it actually moves — and each asserting that shape instead of
//! only describing it:
//! `cohort_bucket_spreads_snowflake_ids_across_all_buckets` strides the
//! TIMESTAMP field (one `2^22` step per sample, so the worker/process/sequence
//! low 22 bits are identical across the whole population), and
//! `cohort_bucket_spreads_ids_that_move_only_in_the_low_bits` holds the
//! timestamp still and varies those low bits instead. The timestamp-strided
//! population is the harder input of the two — its worst bucket sits 7.7% off
//! expected against the 25% bound, where the low-bit population's worst bucket
//! sits 0.5% off — so stating the closure on the timestamp-adjacent shape is the
//! conservative reading. What is closed is those two measured shapes; neither is
//! an observed census of live channel ids, and a real guild's mix remains
//! unmeasured.

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
/// S1 shipped this with no production caller — `#[allow(dead_code)]` and all —
/// because that absence was what made the slice a deployment no-op. S2 is the
/// first caller (`authority_observation::observing_dial`), so the attribute is
/// gone: the dormancy argument is now carried by the dial's shipped values
/// rather than by the absence of a call site.
pub(crate) fn admits(mode: RelayAuthorityMode, percent: u8, channel_id: u64) -> bool {
    mode.consults_cohort() && cohort_bucket(channel_id) < percent.min(100)
}

/// Content fingerprint of the live cohort configuration (design §5.2).
///
/// `config_live_reload` keeps no generation counter (r3 §5.2, measured), so
/// rollout stages cannot be numbered monotonically. This fingerprints the
/// settings instead: two windows at the same dial position share a fingerprint
/// even if the operator moved the dial away and back (declared limit L-7). The
/// promotion script separates such windows by the interleaved samples the dial's
/// detour itself wrote — a sample carrying a different fingerprint sitting
/// between two samples of one fingerprint. File order and a bare timestamp gap
/// were both tried as the discriminator and both retired, because neither can
/// tell a detour from an idle night; the gap survives only as a fallback for the
/// two cases that leave no interleaved sample to find — the detour left the
/// observing set and so wrote nothing at all, or samples did exist during it and
/// were all lost while stranded. `segment_events` in
/// `scripts/relay_authority_rollout_report.py` carries both branches and why the
/// second is low-reachability (eviction bounds unpublished turns at one per
/// channel — legA r3c P2-4).
///
/// The fingerprint is deliberately host-independent, which means it witnesses
/// "the observed population disagreed about the dial", not "the dial moved":
/// during a part-way config rollout two hosts' samples interleave and shred the
/// window. That direction is fail-closed and is declared in `segment_events`
/// (legA r3c P2-3). Any knob added to the cohort decision MUST join the
/// canonical string below, or two materially different rollout windows become
/// indistinguishable in AC3.
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
/// The fallback covers one narrow state: `config_live_reload::current()` answers
/// `None` only before `config_live_reload::install` has published the boot
/// config — a unit test, or startup before the install point — and that window
/// reports the shipped `Legacy/0` dial, which is also the dormant answer.
///
/// A config that fails to parse is a *different* state and does not reach that
/// fallback. `config_live_reload::reload_from_path` answers
/// `ReloadOutcome::Rejected` and leaves the last-known-good snapshot installed,
/// so an invalid or half-written `agentdesk.yaml` keeps the dial that was in
/// force before the bad edit: if the operator had moved to `Observe`/`Enforce`,
/// this block keeps reporting that and a later slice's consumer keeps admitting
/// under it. The policy is fail-stale, not fail-closed — a broken edit is not a
/// way to take the cohort back to nobody.
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

    /// One plausible guild's epoch base for both id fixtures below.
    const SNOWFLAKE_BASE: u64 = 1_234_567_890_123_456_789;
    /// Width of a snowflake's worker + process + sequence fields; everything
    /// above it is the timestamp.
    const LOW_22_MASK: u64 = (1 << 22) - 1;

    /// Discord snowflakes strided by `2^22` — exactly the width of the
    /// worker/process/sequence field — so each sample advances the TIMESTAMP
    /// field by one tick while those low 22 bits hold the base's values for the
    /// whole population. That is a burst of channels minted one per tick by the
    /// same worker and process, and it is the harder of the two shapes for the
    /// hash: every bit the modulo could key on sits in the high half.
    ///
    /// `low_bit_ids` is the complementary fixture. Both callers assert the split
    /// they rely on rather than trusting this comment.
    fn snowflake_ids(count: u64) -> impl Iterator<Item = u64> {
        (0..count).map(|index| SNOWFLAKE_BASE + index * (LOW_22_MASK + 1))
    }

    /// The complement of `snowflake_ids`: consecutive ids from the same base, so
    /// the worker/process/sequence low bits carry every bit of the variation and
    /// the timestamp field never advances (the base's low-22 value plus `count`
    /// stays inside the 22-bit field for the counts used here).
    fn low_bit_ids(count: u64) -> impl Iterator<Item = u64> {
        (0..count).map(|index| SNOWFLAKE_BASE + index)
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
    /// measurement") for the timestamp-strided shape `snowflake_ids` builds —
    /// and pins that shape here, so the measurement and its description cannot
    /// drift apart.
    ///
    /// The bound is deliberately loose — this asserts the hash avalanches, not
    /// that it is cryptographic. A `% 100` of the raw snowflake fails it
    /// outright, and this fixture is why: the stride is `2^22`, and
    /// `2^22 % 100 == 4`, so a raw modulo walks the buckets four at a time and
    /// reaches only 25 of the 100 (deviation 3.0 against the 0.25 bound).
    /// Avalanching all eight bytes first is what earns the modulo.
    #[test]
    fn cohort_bucket_spreads_snowflake_ids_across_all_buckets() {
        const SAMPLES: u64 = 100_000;
        let expected = SAMPLES as f64 / 100.0;
        let mut counts = [0u32; 100];
        for (index, channel_id) in snowflake_ids(SAMPLES).enumerate() {
            // The fixture's own shape, asserted rather than described: the
            // worker/process/sequence low bits hold still and only the timestamp
            // field advances, one tick per sample.
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

    /// The other half of the snowflake, measured under the same bound: ids that
    /// share one timestamp tick and differ only in the
    /// worker/process/sequence low bits. Together with
    /// `cohort_bucket_spreads_snowflake_ids_across_all_buckets` this covers both
    /// fields instead of measuring one and describing the other.
    ///
    /// This case widens coverage; it does not discriminate. A raw `% 100` also
    /// spreads consecutive ids evenly, so the counterfactual that fails belongs
    /// to the timestamp-strided fixture, not to this one.
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

    /// The fingerprint's half of the AC3 correlation key, pinned the same way
    /// `cohort_bucket_is_pinned_to_a_fixed_vector` pins the bucket. Design §5.2
    /// makes this string the JSONL window key, so a moved fingerprint does not
    /// just re-roll a channel — it orphans every window already emitted under
    /// the old spelling.
    ///
    /// The canonical string interpolates the DERIVED `Debug` (`mode=Legacy`),
    /// which is not the serde wire spelling the health block publishes
    /// (`"legacy"`). Nothing but these vectors stops a cosmetic cleanup — a
    /// hand-written `Debug`, a switch to `Display`, or lowercasing the canonical
    /// form — from silently re-keying the whole fleet.
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
