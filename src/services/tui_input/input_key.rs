//! Row-key namespaces of the input ledger: Discord snowflakes below the synthetic base, and the
//! external receipt range at its bottom, which no Legacy id generator reaches.

use sha2::{Digest, Sha256};

/// Ids at or above this are synthetic; real Discord snowflakes stay below it.
pub const SYNTHETIC_KEY_BASE: u64 = 8_000_000_000_000_000_000;
pub const EXTERNAL_KEY_BASE: u64 = SYNTHETIC_KEY_BASE;
pub const EXTERNAL_KEY_SPAN: u64 = 1 << 58;
/// Voice (9e18) and headless (9.1e18) generators start above this.
pub const EXTERNAL_KEY_END: u64 = EXTERNAL_KEY_BASE + EXTERNAL_KEY_SPAN;

pub fn is_external_key(key: u64) -> bool {
    (EXTERNAL_KEY_BASE..EXTERNAL_KEY_END).contains(&key)
}

/// A namespace test on the key alone; Discord provenance is proven by the intake, not here.
pub fn is_discord_key(key: u64) -> bool {
    key != 0 && key < SYNTHETIC_KEY_BASE
}

/// The v1 key of an external origin: only the channel, source and origin id, each length-prefixed,
/// so a retry of the same origin always lands on the same row.
pub fn external_key_v1(channel: u64, source: &str, origin_id: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"adk.http-input.v1\0");
    hasher.update(channel.to_be_bytes());
    for part in [source, origin_id] {
        let len = u32::try_from(part.len()).unwrap_or(u32::MAX);
        hasher.update(len.to_be_bytes());
        hasher.update(part.as_bytes());
    }
    let digest = hasher.finalize();
    let mut head = [0; 8];
    head.copy_from_slice(&digest[..8]);
    EXTERNAL_KEY_BASE + (u64::from_be_bytes(head) & (EXTERNAL_KEY_SPAN - 1))
}
