use super::*;

fn limits() -> ProfileLimits {
    ProfileLimits {
        concurrent_tasks: 2,
        quality_repairs: 3,
        design_review_rounds: 2,
    }
}

#[test]
fn three_repairs_are_allowed_and_each_cycle_counts_once() {
    let mut node = NodeRuntime::default();
    let mut counted = None;
    for cycle in 0..3 {
        assert_eq!(
            record_quality_failure(&mut node, &mut counted, cycle, &limits()),
            FailureDisposition::Repair
        );
        assert_eq!(node.repair_cycles, cycle + 1);
        // A second failure report for the same cycle is not another failure or another repair.
        assert_eq!(
            record_quality_failure(&mut node, &mut counted, cycle, &limits()),
            FailureDisposition::AlreadyCounted
        );
        assert_eq!(
            (node.quality_failures, node.repair_cycles),
            (cycle + 1, cycle + 1)
        );
    }
    assert_eq!(
        record_quality_failure(&mut node, &mut counted, 3, &limits()),
        FailureDisposition::Exhausted
    );
    assert_eq!(node.disposition, NodeDisposition::Failed);
    assert_eq!((node.quality_failures, node.repair_cycles), (4, 3));
    // A late report from an earlier cycle changes nothing.
    assert_eq!(
        record_quality_failure(&mut node, &mut counted, 1, &limits()),
        FailureDisposition::AlreadyCounted
    );
    assert_eq!(node.quality_failures, 4);
}
