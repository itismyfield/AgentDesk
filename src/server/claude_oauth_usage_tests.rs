use super::rate_limit_fetch::parse_claude_oauth_usage_buckets;

#[test]
fn maps_fable_weekly_scoped_limit_without_synthesizing_missing_sonnet() {
    let data = serde_json::json!({
        "five_hour": {"utilization": 5.0, "resets_at": "2026-07-24T00:00:00Z"},
        "seven_day": {"utilization": 58.0, "resets_at": "2026-07-28T00:00:00Z"},
        "seven_day_sonnet": null,
        "limits": [{
            "kind": "weekly_scoped",
            "percent": 55.0,
            "resets_at": "2026-07-28T00:00:00Z",
            "scope": {"model": {"display_name": "Fable"}}
        }]
    });

    let buckets = parse_claude_oauth_usage_buckets(&data);
    assert_eq!(buckets.len(), 3);
    assert_eq!(buckets[2]["name"], "7d Fable");
    assert_eq!(buckets[2]["used"], 55);
}

#[test]
fn omits_fable_bucket_when_percent_is_missing_null_or_non_numeric() {
    for invalid_percent in [
        serde_json::Value::Null,
        serde_json::json!("55"),
        serde_json::json!({"renamed": 55.0}),
    ] {
        let mut fable = serde_json::json!({
            "kind": "weekly_scoped",
            "resets_at": "2026-07-28T00:00:00Z",
            "scope": {"model": {"display_name": "Fable"}}
        });
        if !invalid_percent.is_object() {
            fable["percent"] = invalid_percent;
        }
        let data = serde_json::json!({"limits": [fable]});

        assert!(
            parse_claude_oauth_usage_buckets(&data).is_empty(),
            "invalid Fable usage must not synthesize 0%: {data}"
        );
    }
}

#[test]
fn selects_numeric_fable_from_multiple_scoped_limits() {
    let data = serde_json::json!({
        "limits": [
            {
                "kind": "weekly_scoped",
                "percent": 73.0,
                "scope": {"model": {"display_name": "Sonnet"}}
            },
            {
                "kind": "weekly_scoped",
                "percent": null,
                "scope": {"model": {"display_name": "Fable"}}
            },
            {
                "kind": "weekly_scoped",
                "percent": 41.5,
                "scope": {"model": {"display_name": "Fable"}}
            }
        ]
    });

    let buckets = parse_claude_oauth_usage_buckets(&data);
    assert_eq!(buckets.len(), 1);
    assert_eq!(buckets[0]["name"], "7d Fable");
    assert_eq!(buckets[0]["utilization"], 41.5);
    assert_eq!(buckets[0]["used"], 41);
}
