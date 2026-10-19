//! Message-id namespaces: Discord snowflakes below the synthetic base, synthetic ids at or above it.

/// Ids at or above this are synthetic; real Discord snowflakes stay below it.
pub const SYNTHETIC_KEY_BASE: u64 = 8_000_000_000_000_000_000;

/// A namespace test on the key alone; Discord provenance is proven by the intake, not here.
pub fn is_discord_key(key: u64) -> bool {
    key != 0 && key < SYNTHETIC_KEY_BASE
}
