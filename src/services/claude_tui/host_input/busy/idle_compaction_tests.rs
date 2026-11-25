use super::*;
use crate::services::claude_tui::busy_inject::Request;
use crate::services::claude_tui::{hook_server::HookEventKind, readiness};
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_turn_state::idle_compaction_tests::{compacted_tail, end_turn, tail};

struct ReadinessScope;

impl Drop for ReadinessScope {
    fn drop(&mut self) {
        readiness::tests::forget_session(LOGICAL);
    }
}

#[test]
fn a_captured_busy_source_admits_after_idle_compaction_metadata() {
    let fx = BindingFixture::new();
    readiness::tests::forget_session(LOGICAL);
    let readiness_scope = ReadinessScope;
    let source = live_source(&fx);
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    readiness::initialize();
    let launch: BindingContext = serde_json::from_value(json!({
        "schema":1,"provider":"claude","created_at":chrono::Utc::now(),
        "execution_nonce":N1,"tmux_session":LOGICAL,"channel_id":CHANNEL,
        "owner_runtime_root":crate::services::tmux_common::current_tmux_owner_marker(),
        "expected_native_session_id":source.session_id,"launch_mode":"fresh",
        "provider_root":fx.root.path().parent().unwrap()
    }))
    .unwrap();
    let prepared = PreparedIncarnation::create(launch.clone()).unwrap();
    pane_with_context(&rig, &prepared.path);
    readiness::note_hook(
        "claude",
        &HookEventKind::SessionStart,
        LOGICAL,
        &json!({"session_id":source.session_id,"transcript_path":source.path,"source":"startup"}),
        Some(&launch),
        Some(launch.created_at),
        false,
    );
    let hosted = observe_busy(CHANNEL, LOGICAL, &host(&rig), "observation-fixture");
    let context = hosted.context_for_test().unwrap();
    let observation = crate::services::claude_tui::host_input::busy::observe_herdr(
        (CHANNEL, LOGICAL, N1),
        (
            context.target_for_test().clone(),
            context.writer_for_test().to_owned(),
            context.attempt_target_for_test().clone(),
        ),
        |_| true,
    );
    assert_eq!(observation.state(), BusyState::Busy);
    let context = observation.context_for_test().unwrap();
    assert!(!context.read_for_test().idle_verified_for_test());
    assert!(readiness::observe(LOGICAL, Some(&source.path)).is_none());
    let request = Request {
        session: LOGICAL,
        transcript: &source.path,
        source: "fixture",
        author: "fixture-author",
        name: "Fixture",
        nonce: "a8214001",
        text: "the held input",
    };
    assert!(observation.admits_for_test(&request));
    let mut probe = observation.probe();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&source.path)
        .unwrap();
    for mut record in tail() {
        record["sessionId"] = json!(source.session_id);
        writeln!(file, "{record}").unwrap();
    }
    assert_eq!(context.read_for_test().state_for_test(), BusyState::Held);
    assert!(!context.read_for_test().idle_verified_for_test());
    assert!(readiness::observe(LOGICAL, Some(&source.path)).is_none());
    assert!(
        matches!(probe(), Screen::Unknown),
        "metadata without a completed reply cannot wake the captured probe"
    );
    for mut record in std::iter::once(end_turn()).chain(compacted_tail()) {
        record["sessionId"] = json!(source.session_id);
        writeln!(file, "{record}").unwrap();
    }
    assert_eq!(current(context).unwrap().id_for_test(), source);
    let reading = context.read_for_test();
    assert_eq!(reading.state_for_test(), BusyState::Idle);
    assert!(
        reading.idle_verified_for_test(),
        "the same source has verified Idle"
    );
    assert!(
        matches!(probe(), Screen::Ready),
        "the captured probe wakes for Idle delegation after the completed tail"
    );
    assert!(
        !observation.admits_for_test(&request),
        "the captured Busy writer delegates verified Idle instead of injecting into it"
    );
    assert!(rig.sends().is_empty(), "the observer itself sends no keys");
    drop(readiness_scope);
}
