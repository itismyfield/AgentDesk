/// Normalized provider and raw channel identity shared by storage and routing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnerIdentity {
    provider: String,
    raw_channel_id: String,
}

impl OwnerIdentity {
    pub(crate) fn new(provider: &str, raw_channel_id: &str) -> Self {
        Self {
            provider: provider.trim().to_lowercase(),
            raw_channel_id: raw_channel_id.trim().to_string(),
        }
    }

    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    pub(crate) fn raw_channel_id(&self) -> &str {
        &self.raw_channel_id
    }

    /// Stable cross-process key from the normalized UTF-8 components.
    pub(crate) fn advisory_key(&self) -> i64 {
        advisory_lock_key(&self.provider, &self.raw_channel_id)
    }
}

// FNV-1a uses the fixed domain tag and NUL-delimited normalized bytes.
fn advisory_lock_key(norm_provider: &str, norm_raw_channel_id: &str) -> i64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash: u64 = FNV_OFFSET_BASIS;
    let mut absorb = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };

    absorb(b"intake_session_owner\0");
    absorb(norm_provider.as_bytes());
    absorb(b"\0");
    absorb(norm_raw_channel_id.as_bytes());

    hash as i64
}

#[cfg(test)]
mod tests {
    use super::OwnerIdentity;

    #[test]
    fn normalization_and_advisory_key_match_golden_vectors() {
        let cases: &[(&str, &str, &str, &str, i64)] = &[
            (
                " \tCLAUDE\n",
                " 123456789\r\n",
                "claude",
                "123456789",
                -2_180_495_178_205_472_121,
            ),
            (
                "CoDeX",
                "123456789",
                "codex",
                "123456789",
                -2_362_791_939_951_047_754,
            ),
            (
                " CLAUDE ",
                " Room-A ",
                "claude",
                "Room-A",
                6_053_282_317_273_094_797,
            ),
            (
                "claude",
                "room-a",
                "claude",
                "room-a",
                -7_864_669_842_345_060_851,
            ),
            (
                " AGY ",
                " Room-A ",
                "agy",
                "Room-A",
                -3_823_882_967_403_318_382,
            ),
            (
                " Antigravity ",
                "Room-A",
                "antigravity",
                "Room-A",
                -8_109_466_031_086_017_191,
            ),
            (
                " DISCORD_O ",
                " Room-A ",
                "discord_o",
                "Room-A",
                -7_620_233_397_845_193_679,
            ),
            (
                "discord",
                "Room-A",
                "discord",
                "Room-A",
                -1_784_958_887_715_811_669,
            ),
            (
                "\u{2003}\u{130}X\u{a0}",
                "\u{a0}caf\u{e9}:\u{cc44}\u{b110}\u{2003}",
                "i\u{307}x",
                "caf\u{e9}:\u{cc44}\u{b110}",
                4_740_307_297_704_425_690,
            ),
            ("\t ", "\r\n", "", "", -6_733_598_978_886_797_470),
            ("ab", "c", "ab", "c", -5_396_786_273_725_835_292),
            ("a", "bc", "a", "bc", -8_081_602_356_512_389_188),
        ];
        for &(provider, raw_channel_id, expected_provider, expected_channel, expected_key) in cases
        {
            let identity = OwnerIdentity::new(provider, raw_channel_id);
            assert_eq!(identity.provider(), expected_provider);
            assert_eq!(identity.raw_channel_id(), expected_channel);
            assert_eq!(identity.advisory_key(), expected_key);
        }
    }

    #[test]
    fn provider_alias_spellings_and_raw_channel_case_remain_distinct() {
        let alias = OwnerIdentity::new(" AGY ", " Room-A ");
        assert_eq!(alias, OwnerIdentity::new("agy", "Room-A"));
        assert_ne!(alias, OwnerIdentity::new("antigravity", "Room-A"));
        assert_ne!(
            alias.advisory_key(),
            OwnerIdentity::new("antigravity", "Room-A").advisory_key()
        );
        assert_ne!(alias, OwnerIdentity::new("agy", "room-a"));
        assert_ne!(
            alias.advisory_key(),
            OwnerIdentity::new("agy", "room-a").advisory_key()
        );
        assert_ne!(
            OwnerIdentity::new("discord_o", "Room-A"),
            OwnerIdentity::new("discord", "Room-A"),
        );
    }
}
