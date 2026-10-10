use super::o_seed_observation::{
    Guard, key_totals, raw_process_totals, record_event, record_parser_response,
};
use super::{load_inflight_state, load_inflight_state_read_only_result};
use crate::services::provider::ProviderKind;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_seed_observation_raw_totals_include_unguarded_and_other_keys() {
    let channel = 63_251_191;
    let before = raw_process_totals();
    let _ = load_inflight_state_read_only_result(&ProviderKind::Gemini, channel);
    assert_eq!(
        key_totals(&ProviderKind::Gemini, channel).readonly_inflight_load_calls,
        1
    );
    assert!(
        raw_process_totals().readonly_inflight_load_calls > before.readonly_inflight_load_calls
    );
    let guard = Guard::new(&ProviderKind::Claude, channel);
    tokio::spawn(async move {
        let _ = load_inflight_state(&ProviderKind::Gemini, channel);
        let _ = load_inflight_state_read_only_result(&ProviderKind::Gemini, channel);
        record_event(&ProviderKind::Gemini, channel, "final_joined");
    })
    .await
    .unwrap();
    let raw = guard.raw_process_snapshot();
    assert!(raw.writable_inflight_load_calls >= 1);
    assert!(raw.readonly_inflight_load_calls >= 1);
    assert!(raw.event_count("final_joined") >= 1);
    let keyed = guard.snapshot();
    assert_eq!(keyed.writable_inflight_load_calls, 0);
    assert_eq!(keyed.readonly_inflight_load_calls, 0);
    assert_eq!(keyed.event_count("final_joined"), 0);
    assert_eq!(
        key_totals(&ProviderKind::Gemini, channel).writable_inflight_load_calls,
        1
    );
    assert_eq!(
        key_totals(&ProviderKind::Gemini, channel).readonly_inflight_load_calls,
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn o_seed_observation_key_baselines_and_named_events_follow_tasks() {
    let channel = 63_251_192;
    record_event(&ProviderKind::Codex, channel, "outer_eof");
    record_parser_response(&ProviderKind::Codex, channel, "before guard");
    let guard = Guard::new(&ProviderKind::Codex, channel);
    tokio::spawn(async move {
        record_event(&ProviderKind::Codex, channel, "watcher_initialized");
        record_event(&ProviderKind::Codex, channel, "decoder_initialized");
        record_event(&ProviderKind::Codex, channel, "outer_eof");
        record_event(&ProviderKind::Codex, channel, "inner_eof");
        record_event(&ProviderKind::Codex, channel, "checkpoint");
        record_event(&ProviderKind::Codex, channel, "capsule_adopted");
        record_event(&ProviderKind::Codex, channel, "monitor_started");
        record_event(&ProviderKind::Codex, channel, "no_result_actual");
        record_event(&ProviderKind::Codex, channel, "final_joined");
        record_parser_response(&ProviderKind::Codex, channel, "actual parsed response");
    })
    .await
    .unwrap();
    let keyed = guard.snapshot();
    assert!(keyed.watcher_initialization_observed);
    assert!(keyed.stream_decoder_initialization_observed);
    assert_eq!(keyed.parser_responses, ["actual parsed response"]);
    assert!(
        guard
            .raw_process_snapshot()
            .parser_responses
            .iter()
            .any(|response| response == "actual parsed response")
    );
    for event in [
        "outer_eof",
        "inner_eof",
        "checkpoint",
        "capsule_adopted",
        "monitor_started",
        "no_result_actual",
        "final_joined",
    ] {
        assert_eq!(keyed.event_count(event), 1);
        assert!(guard.raw_process_snapshot().event_count(event) >= 1);
    }
    assert_eq!(
        key_totals(&ProviderKind::Codex, channel).event_count("outer_eof"),
        2
    );
}
