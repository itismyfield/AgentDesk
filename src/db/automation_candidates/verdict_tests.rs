use super::*;

#[test]
fn crashed_always_discards() {
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.95),
            false,
            "crashed",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
    assert_eq!(
        compute_verdict(None, None, true, "crashed", MetricDirection::LowerIsBetter),
        "discard"
    );
}

#[test]
fn timeout_always_discards() {
    assert_eq!(
        compute_verdict(
            Some(0.8),
            Some(0.9),
            false,
            "timeout",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
}

#[test]
fn simplification_always_keeps() {
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.5),
            true,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "keep"
    );
    assert_eq!(
        compute_verdict(None, None, true, "ok", MetricDirection::HigherIsBetter),
        "keep"
    );
}

#[test]
fn lower_metric_improvement_keeps() {
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.8),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "keep"
    );
    assert_eq!(
        compute_verdict(
            Some(1.0),
            Some(0.0),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "keep"
    );
}

#[test]
fn higher_metric_improvement_keeps() {
    assert_eq!(
        compute_verdict(
            Some(0.8),
            Some(0.9),
            false,
            "ok",
            MetricDirection::HigherIsBetter
        ),
        "keep"
    );
}

#[test]
fn metric_regression_or_equal_discards() {
    assert_eq!(
        compute_verdict(
            Some(0.8),
            Some(0.9),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
    assert_eq!(
        compute_verdict(
            Some(0.9),
            Some(0.8),
            false,
            "ok",
            MetricDirection::HigherIsBetter
        ),
        "discard"
    );
    assert_eq!(
        compute_verdict(
            Some(0.5),
            Some(0.5),
            false,
            "ok",
            MetricDirection::LowerIsBetter
        ),
        "discard"
    );
}

#[test]
fn no_metrics_discards() {
    assert_eq!(
        compute_verdict(None, None, false, "ok", MetricDirection::LowerIsBetter),
        "discard"
    );
    assert_eq!(
        compute_verdict(Some(0.8), None, false, "ok", MetricDirection::LowerIsBetter),
        "discard"
    );
    assert_eq!(
        compute_verdict(None, Some(0.8), false, "ok", MetricDirection::LowerIsBetter),
        "discard"
    );
}

#[test]
fn parses_metric_direction_aliases() {
    assert_eq!(
        MetricDirection::parse(Some("higher")),
        MetricDirection::HigherIsBetter
    );
    assert_eq!(
        MetricDirection::parse(Some("higher_is_better")),
        MetricDirection::HigherIsBetter
    );
    assert_eq!(
        MetricDirection::parse(Some("lower")),
        MetricDirection::LowerIsBetter
    );
    assert_eq!(MetricDirection::parse(None), MetricDirection::LowerIsBetter);
}

#[test]
fn final_iteration_boundary() {
    assert!(!is_final_iteration(9));
    assert!(is_final_iteration(10));
    assert!(is_final_iteration(11));
}

#[test]
fn card_update_guards_require_exactly_one_row() {
    assert!(ensure_one_card_row_affected(1, "update card", "card-1").is_ok());

    let missing = ensure_one_card_row_affected(0, "update card", "card-1")
        .expect_err("zero-row updates must fail");
    assert!(missing.contains("affected 0 rows"));

    let duplicated = ensure_one_card_row_affected(2, "update card", "card-1")
        .expect_err("multi-row updates must fail");
    assert!(duplicated.contains("affected 2 rows"));
}
