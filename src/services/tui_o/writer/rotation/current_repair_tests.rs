use super::*;

#[test]
fn checkpoint_ahead_of_a_valid_log_prefix_requires_unknown() {
    let a = source("a", 1);
    let logged = named(vec![bind(1, "pane", None, BindingTarget::Source(a))]);
    assert!(
        matches!(Current::of(Some(&logged), Some(2)), Current::Unknown),
        "a complete JSON prefix does not cover the already-durable checkpoint"
    );
}

#[test]
fn checkpoint_before_tail_still_uses_the_full_logs_last_bound() {
    let a = source("a", 1);
    let b = source("b", 2);
    let c = source("c", 3);
    let logged = named(vec![
        bind(1, "pane", None, BindingTarget::Source(a.clone())),
        bind(2, "pane", Some(&a), BindingTarget::Source(b.clone())),
        bind(3, "pane", Some(&b), BindingTarget::Source(c.clone())),
    ]);
    known(&logged, 1, &stored(&c));
    assert!(Current::of(Some(&logged), Some(1)).historical(&stored(&a)));
    assert!(Current::of(Some(&logged), Some(1)).historical(&stored(&b)));
}
