use super::input_key::*;

#[test]
fn external_key_v1_vectors_and_encoding() {
    // Reference values computed independently of this implementation.
    assert_eq!(
        external_key_v1(101, "imessage", "fixture-guid-001"),
        8_002_841_980_997_189_983
    );
    assert_eq!(
        external_key_v1(102, "imessage", "fixture-guid-001"),
        8_134_095_443_858_821_159
    );
    // Length prefixes keep a moved boundary from aliasing; lengths count UTF-8 bytes.
    assert_eq!(external_key_v1(101, "ab", "c"), 8_195_692_330_986_398_471);
    assert_eq!(external_key_v1(101, "a", "bc"), 8_044_343_783_367_367_076);
    assert_eq!(
        external_key_v1(101, "imessage", "é"),
        8_129_832_477_465_469_935
    );
    for key in [
        external_key_v1(101, "imessage", "fixture-guid-001"),
        external_key_v1(u64::MAX, &"s".repeat(64), &"o".repeat(256)),
    ] {
        assert!(is_external_key(key) && !is_discord_key(key));
    }
}

#[test]
fn external_key_namespace_boundaries() {
    assert_eq!(EXTERNAL_KEY_END, 8_288_230_376_151_711_744);
    let cases = [
        (0, false, false),
        (1, false, true),
        (1_300_000_000_000_000_000, false, true),
        (SYNTHETIC_KEY_BASE - 1, false, true),
        (SYNTHETIC_KEY_BASE, true, false),
        (EXTERNAL_KEY_END - 1, true, false),
        (EXTERNAL_KEY_END, false, false),
        (9_000_000_000_000_000_000, false, false),
        (9_100_000_000_000_000_000, false, false),
        (u64::MAX, false, false),
    ];
    for (key, external, discord) in cases {
        assert_eq!(
            (is_external_key(key), is_discord_key(key)),
            (external, discord),
            "{key}"
        );
    }
}
