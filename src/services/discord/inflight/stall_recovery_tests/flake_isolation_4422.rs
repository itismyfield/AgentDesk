//! #4422 deterministic regression for the flaky text-log severity witness.

use super::*;
use crate::services::discord::outbound::delivery_record as dr;
use crate::services::observability::InvariantSeverity;

#[test]
fn legit_reset_records_monotonic_violation_at_typed_warn_4422() {
    let _serialized = monotonic_3358_test_mutex()
        .lock()
        .unwrap_or_else(|error| error.into_inner());

    // A same-turn RetryBoundary reset is permitted under both authority modes.
    // Observe the typed decision synchronously; formatted tracing output is
    // deliberately not part of this test's correctness witness.
    for authority_on in [false, true] {
        let temp = TempDir::new().unwrap();
        let channel_id = 44_220_100 + authority_on as u64;
        let mut reset = seed_watcher_stream_state(
            temp.path(),
            channel_id,
            "AgentDesk-claude-4422-warn",
            "streamed answer body",
            120,
        );
        reset.full_response = String::new();
        reset.response_sent_offset = 0;
        let path = inflight_state_path(temp.path(), &ProviderKind::Claude, channel_id);

        let (tripwire, events) = super::super::invariant_test_capture::capture(|| {
            let _authority = dr::authority_test_seam::force(authority_on);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                validate_inflight_state_for_save(
                    temp.path(),
                    &path,
                    &reset,
                    "src/services/discord/inflight.rs:test",
                );
            }))
        });
        assert!(
            tripwire.is_err() || cfg!(not(debug_assertions)),
            "the production monotonic tripwire must fire when debug assertions are enabled"
        );

        let severities: Vec<_> = events
            .iter()
            .filter(|event| event.invariant == "response_sent_offset_monotonic")
            .map(|event| event.severity)
            .collect();
        assert_eq!(
            severities,
            [InvariantSeverity::Warn],
            "legitimate reset must expose exactly one typed WARN invariant (authority_on={authority_on}): {events:?}"
        );
    }

    // Non-reset backward progress with authority disabled persists bad state and
    // therefore remains ERROR. This control keeps the WARN assertion non-vacuous.
    let temp = TempDir::new().unwrap();
    let channel_id = 44_220_199;
    let mut regression = seed_watcher_stream_state(
        temp.path(),
        channel_id,
        "AgentDesk-claude-4422-error",
        "the full committed answer",
        200,
    );
    regression.full_response = "stale".to_string();
    regression.response_sent_offset = 3;
    let path = inflight_state_path(temp.path(), &ProviderKind::Claude, channel_id);

    let (tripwire, events) = super::super::invariant_test_capture::capture(|| {
        let _authority = dr::authority_test_seam::force(false);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            validate_inflight_state_for_save(
                temp.path(),
                &path,
                &regression,
                "src/services/discord/inflight.rs:test",
            );
        }))
    });
    assert!(
        tripwire.is_err() || cfg!(not(debug_assertions)),
        "the production monotonic tripwire must fire when debug assertions are enabled"
    );
    let severities: Vec<_> = events
        .iter()
        .filter(|event| event.invariant == "response_sent_offset_monotonic")
        .map(|event| event.severity)
        .collect();
    assert_eq!(
        severities,
        [InvariantSeverity::Error],
        "genuine non-reset backward progress must expose exactly one typed ERROR invariant: {events:?}"
    );
}
