use super::rate_limit_fetch::{parse_rate_limit_duration_seconds, parse_rate_limit_reset_value};

#[test]
fn parses_openai_relative_reset_duration() {
    assert_eq!(parse_rate_limit_duration_seconds("6m0s"), Some(360));
    assert_eq!(parse_rate_limit_duration_seconds("1s"), Some(1));
    assert_eq!(parse_rate_limit_reset_value("500ms", 1_000), Some(1_001));
}

#[test]
fn parses_rfc3339_reset_timestamp() {
    assert_eq!(
        parse_rate_limit_reset_value("2026-06-17T00:00:00Z", 1_000),
        Some(1_781_654_400)
    );
}
