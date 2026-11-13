use super::tests::{CHANNEL, enabled_runtime};
use super::*;

fn payload(step: usize) -> CheckpointPayload {
    CheckpointPayload::compact(
        "claude",
        format!("goal {step}"),
        format!("progress {step}"),
        "keep decisions",
        vec![format!("src/step_{step}.rs")],
        format!("next {step}"),
        "continue",
    )
}

fn assert_source(packet: &str, step: usize) {
    for expected in [
        format!("Goal: goal {step}\n"),
        format!("Progress: progress {step}\n"),
        format!("Files: src/step_{step}.rs\n"),
        format!("Next: next {step}\n"),
    ] {
        assert!(
            packet.contains(&expected),
            "missing {expected:?} in {packet}"
        );
    }
}

#[test]
fn restore_preserves_last_real_progress_at_eight_and_nine_events() {
    for count in [9, 8] {
        let mut runtime = enabled_runtime();
        for step in 1..=count {
            runtime.note_owner_progress(CHANNEL, payload(step)).unwrap();
        }
        let source_id = runtime.events(CHANNEL).last().unwrap().id.clone();
        for _ in 0..10 {
            let plan = runtime.restore_owner(CHANNEL, true, "available").unwrap();
            assert_source(&plan.packet, count);
            assert!(plan.packet.contains(&source_id));
        }
        assert_eq!(
            runtime
                .events(CHANNEL)
                .iter()
                .filter(|event| event.kind == CheckpointEventKind::Complete)
                .count(),
            0
        );
    }
}

#[test]
fn restore_preserves_real_complete_inside_recent_window() {
    let mut runtime = enabled_runtime();
    runtime.note_owner_progress(CHANNEL, payload(1)).unwrap();
    let complete = runtime
        .append_event(
            CHANNEL,
            "monitoring",
            CheckpointEventKind::Complete,
            payload(2),
        )
        .unwrap();
    let plan = runtime.restore_owner(CHANNEL, true, "done").unwrap();
    assert_source(&plan.packet, 2);
    assert!(plan.packet.contains(&complete.id));
    assert_eq!(
        runtime
            .events(CHANNEL)
            .iter()
            .filter(|event| event.kind == CheckpointEventKind::Complete)
            .count(),
        1
    );
}

#[test]
fn restore_ignores_legacy_empty_completes() {
    for progress in ["fallback complete", "fallback failed"] {
        let mut runtime = enabled_runtime();
        runtime.note_owner_progress(CHANNEL, payload(1)).unwrap();
        runtime
            .append_event(
                CHANNEL,
                "monitoring",
                CheckpointEventKind::Complete,
                CheckpointPayload::compact("monitoring", "", progress, "", Vec::new(), "", ""),
            )
            .unwrap();
        let plan = runtime.restore_owner(CHANNEL, true, "available").unwrap();
        assert_source(&plan.packet, 1);
    }
}
