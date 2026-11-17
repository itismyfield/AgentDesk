use super::*;
use crate::services::tui_turn_state::idle_compaction_tests::{end_turn, tail};

#[test]
fn a_captured_busy_source_admits_after_idle_compaction_metadata() {
    let fx = BindingFixture::new();
    let source = live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    pane(&rig);
    let hosted = observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    let context = hosted.context.as_ref().unwrap();
    let observation = super::super::observe(
        (CHANNEL, LOGICAL, N1),
        (
            context.target.clone(),
            context.writer.clone(),
            context.attempt_target.clone(),
        ),
        |_| true,
    );
    assert_eq!(observation.state(), BusyState::Busy);
    let request = Request {
        session: LOGICAL,
        transcript: &source.path,
        source: "fixture",
        author: "fixture-author",
        name: "Fixture",
        nonce: "a8214001",
        text: "the held input",
    };
    assert!(observation.admits(&request));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.path)
        .unwrap();
    for mut record in std::iter::once(end_turn()).chain(tail()) {
        record["sessionId"] = json!(source.session_id);
        writeln!(file, "{record}").unwrap();
    }
    assert_eq!(
        observation.context.as_ref().unwrap().read().state,
        BusyState::Idle,
        "the same authoritative source is Idle after only the completed tail"
    );
    assert!(
        observation.admits(&request),
        "the captured busy transaction can admit its unchanged source after idle compaction"
    );
    assert!(rig.sends().is_empty(), "the observer itself sends no keys");
}
