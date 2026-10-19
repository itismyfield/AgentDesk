//! O closes only the observation named by a terminal rejection.

use super::*;
use crate::services::tui_o::writer::binding::terminal_tests::Fixture;

#[test]
fn logged_exact_rejection_closes_only_its_pending() {
    let h = Fixture::new();
    let (anchor, ups) = h.settle();
    let events = h.events();
    let (bound, named) = logged(&events).unwrap();
    assert_eq!(bound, [&h.source]);
    assert!(named.is_empty(), "a rejection names no source");

    for closes in [None, Some(anchor), Some(ups + 100)] {
        let mut changed = events.clone();
        let BindingRecord::Rejected { closes: target, .. } = &mut changed[2].record else {
            panic!("typed terminal projection");
        };
        *target = closes;
        let refused = logged(&changed).unwrap_err();
        assert_eq!(refused.hold, Hold::Binding);
        assert_eq!(refused.to_string(), format!("bind {ups} is still pending"));
    }

    let mut unrelated = events[1].clone();
    unrelated.seq = events.last().unwrap().seq + 1;
    let mut changed = events.clone();
    changed.push(unrelated.clone());
    let refused = logged(&changed).unwrap_err();
    assert_eq!(refused.hold, Hold::Binding);
    assert_eq!(
        refused.to_string(),
        format!("bind {} is still pending", unrelated.seq)
    );
    assert_eq!(
        logged(&events[..3]).unwrap_err().hold,
        Hold::Binding,
        "closing the UPS does not resolve the anchor"
    );
}
