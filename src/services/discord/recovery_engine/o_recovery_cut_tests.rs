//! Recovery relays on a channel whose TUI body O posts: only the body-free marker is shown,
//! a captured range is consumed without a send, and no delivery evidence is written.

use super::completion_delivery::{
    RECOVERED_WITHOUT_TEXT, relay_captured_recovery_terminal_notice, relay_recovery_body_notice,
    relay_recovery_body_to_placeholder,
};
use super::o_cut_recorder::{start, start_watching};
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::tui_o::cutover::test_override::force_channels;

const BODY: &str = "ADK-A14B-recovered-body";

fn recovery_state(
    channel: u64,
    current_msg_id: u64,
    kind: RuntimeHandoffKind,
) -> inflight::InflightTurnState {
    let mut state = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        7,
        channel + 1,
        current_msg_id,
        "prompt".to_string(),
        None,
        Some(format!("AgentDesk-claude-o-cut-{channel}")),
        None,
        None,
        64,
    );
    state.full_response = BODY.to_string();
    state.runtime_kind = Some(kind);
    state
}

fn no_delivery_record(channel: u64) -> bool {
    super::super::outbound::delivery_record::delivery_record_path(&ProviderKind::Claude, channel)
        .is_none_or(|path| !path.exists())
}

#[tokio::test(flavor = "current_thread")]
async fn o_delegated_recovery_body_posts_only_the_marker_without_evidence() {
    with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let _o = force_channels(&[
            (9_425_001, RuntimeHandoffKind::ClaudeTui),
            (9_425_002, RuntimeHandoffKind::ClaudeTui),
            (9_425_004, RuntimeHandoffKind::ClaudeTui),
        ]);
        for (channel, placeholder) in [(9_425_001u64, 0u64), (9_425_002, 9_425_902)] {
            let recorder = start(channel).await;
            let state = recovery_state(channel, placeholder, RuntimeHandoffKind::ClaudeTui);
            let text = interrupted_recovery_message(&state, &state.full_response);
            let marker = interrupted_recovery_message(&state, "");
            let outcome = relay_recovery_body_notice(
                &recorder.http,
                &shared,
                &ProviderKind::Claude,
                &state,
                &text,
            )
            .await;
            assert_eq!(
                outcome,
                RecoveryRelayOutcome::Delivered,
                "placeholder={placeholder}"
            );
            let contents = recorder.contents();
            assert_eq!(
                contents,
                vec![marker],
                "placeholder={placeholder}: only the marker is shown"
            );
            assert!(
                no_delivery_record(channel),
                "placeholder={placeholder}: no Legacy evidence"
            );
        }

        for (channel, kind) in [
            (9_425_003, RuntimeHandoffKind::LegacyTmuxWrapper),
            (9_425_005, RuntimeHandoffKind::ClaudeTui),
        ] {
            let recorder = start(channel).await;
            let state = recovery_state(channel, 0, kind);
            let text = interrupted_recovery_message(&state, &state.full_response);
            relay_recovery_body_notice(
                &recorder.http,
                &shared,
                &ProviderKind::Claude,
                &state,
                &text,
            )
            .await;
            assert!(
                recorder
                    .contents()
                    .iter()
                    .any(|content| content.contains(BODY)),
                "an unlisted {kind:?} channel keeps relaying the recovered body"
            );
        }

        let channel = 9_425_004;
        let recorder = start(channel).await;
        let mut state = recovery_state(channel, 0, RuntimeHandoffKind::ClaudeTui);
        state.runtime_kind = None;
        let text = interrupted_recovery_message(&state, &state.full_response);
        let outcome = relay_recovery_body_notice(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
            &state,
            &text,
        )
        .await;
        assert_eq!(outcome, RecoveryRelayOutcome::TransientFailure);
        assert!(
            recorder.calls().is_empty(),
            "an unknown kind on a listed channel is held for retry"
        );
    })
    .await;
}

/// A recovery notice with no answer (the restart notice or the no-text notice) leaves a pending
/// adoption; the notice carrying the recovered body ends it first and shows that body once.
#[tokio::test(flavor = "current_thread")]
async fn only_a_recovered_body_ends_a_pending_adoption() {
    use crate::services::tui_o::channel_policy::{Adoption, BodyCheck};
    with_isolated_runtime_root(|| async move {
        const CHANNEL: u64 = 9_425_021;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let _pending = crate::services::tui_o::cutover::test_override::force_candidates(&[(
            CHANNEL,
            RuntimeHandoffKind::ClaudeTui,
        )]);
        let check = BodyCheck::watch(CHANNEL, BODY);
        let mut empty = recovery_state(CHANNEL, 0, RuntimeHandoffKind::ClaudeTui);
        empty.full_response = " \n".to_string();
        let notice = interrupted_recovery_message(&empty, &empty.full_response);
        let recorder = start_watching(CHANNEL, check.clone(), false).await;
        relay_recovery_body_notice(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
            &empty,
            &notice,
        )
        .await;
        let channel = ChannelId::new(CHANNEL);
        let no_text = RECOVERED_WITHOUT_TEXT;
        relay_recovery_body_to_placeholder(
            &recorder.http,
            &shared,
            &empty,
            channel,
            None,
            no_text,
            None,
        )
        .await;
        assert_eq!(recorder.contents(), vec![notice, no_text.to_string()]);
        check.assert_settled();
        assert_eq!(check.adoption(), Adoption::Pending);

        let state = recovery_state(CHANNEL, 0, RuntimeHandoffKind::ClaudeTui);
        let text = interrupted_recovery_message(&state, &state.full_response);
        relay_recovery_body_notice(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
            &state,
            &text,
        )
        .await;
        let shown = recorder.contents();
        assert_eq!(
            shown.iter().filter(|c| c.contains(BODY)).count(),
            1,
            "{shown:?}"
        );
        check.assert_settled();
        assert_eq!(check.adoption(), Adoption::Released);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn o_delegated_captured_recovery_range_is_consumed_without_send() {
    with_isolated_runtime_root(|| async move {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let _o = force_channels(&[(9_425_011, RuntimeHandoffKind::ClaudeTui)]);
        let channel = 9_425_011;
        let recorder = start(channel).await;
        let state = recovery_state(channel, 0, RuntimeHandoffKind::ClaudeTui);
        let delivery = relay_captured_recovery_terminal_notice(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
            &state,
            BODY,
        )
        .await;
        assert_eq!(delivery.outcome, RecoveryRelayOutcome::Delivered);
        assert!(
            delivery.pending_anchor.is_none(),
            "no anchor evidence for O's body"
        );
        assert!(
            recorder.calls().is_empty(),
            "no Discord request for O's body"
        );
        assert!(no_delivery_record(channel));

        let channel = 9_425_012;
        let recorder = start(channel).await;
        let state = recovery_state(channel, 0, RuntimeHandoffKind::LegacyTmuxWrapper);
        relay_captured_recovery_terminal_notice(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
            &state,
            BODY,
        )
        .await;
        assert!(
            recorder
                .contents()
                .iter()
                .any(|content| content.contains(BODY)),
            "an unlisted channel keeps relaying the captured range"
        );
    })
    .await;
}
