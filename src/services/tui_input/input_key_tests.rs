use super::input_key::*;

#[test]
fn discord_key_namespace_boundaries() {
    let cases = [
        (0, false),
        (1, true),
        (1_300_000_000_000_000_000, true),
        (SYNTHETIC_KEY_BASE - 1, true),
        (SYNTHETIC_KEY_BASE, false),
        (9_000_000_000_000_000_000, false),
        (9_100_000_000_000_000_000, false),
        (u64::MAX, false),
    ];
    for (key, discord) in cases {
        assert_eq!(is_discord_key(key), discord, "{key}");
    }
}
