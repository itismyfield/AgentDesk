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
async fn i8372_process_restart_rechecks_thread_before_relay_pg() {
    use crate::services::message_outbox::{
        OutboxEnqueueOutcome, OutboxMessage, enqueue_outbox_pg_with_exact_origin,
    };
    use crate::services::tui_o::cutover::{
        boot_snapshot, install_channel_lookup, record_thread_child,
        test_override::force_channel_lookups,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    with_isolated_runtime_root(|| async move {
        const PARENT: u64 = 8_372_601;
        const CHILD: u64 = 8_372_602;
        const NONCE: &str = "routine-process-recovery";
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "i8372_restart",
            "routine process recovery delivery",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let recorder = start(CHILD).await;
        let mut state = recovery_state(CHILD, 0, RuntimeHandoffKind::ProcessBackend);
        state.tmux_session_name = None;
        state.channel_name = Some("parent-display-name".to_owned());
        state.turn_nonce = Some(NONCE.into());
        state.born_generation = 41;
        inflight::save_inflight_state(&state).unwrap();
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            CHILD,
        );
        let mut wire = serde_json::to_value(&state).unwrap();
        wire["routine_presentation"] = serde_json::json!({"process_thread": {
            "parent_channel_id": PARENT, "channel_id": CHILD, "provider": "claude",
            "turn_nonce": NONCE,
        }});

        let dedupe_key = format!(
            "headless_delivery:v2|p=6:claude|c={CHILD}|u={}|g={}",
            state.user_msg_id, state.born_generation,
        );
        let target = format!("channel:{CHILD}");
        let seeded = enqueue_outbox_pg_with_exact_origin(
            &pool,
            OutboxMessage {
                target: &target,
                content: BODY,
                bot: "claude",
                source: "headless_turn",
                reason_code: Some("headless.delivery"),
                session_key: None,
            },
            &dedupe_key,
            None,
            None,
            None,
            Some(NONCE),
        )
        .await
        .unwrap();
        let OutboxEnqueueOutcome::Enqueued { id: sent_id } = seeded else {
            panic!("expected one seeded recovery delivery, got {seeded:?}");
        };
        let updated =
            sqlx::query("UPDATE message_outbox SET status='sent', sent_at=NOW() WHERE id=$1")
                .bind(sent_id)
                .execute(&pool)
                .await
                .unwrap();
        assert_eq!(updated.rows_affected(), 1);

        let thread: serenity::GuildChannel = serde_json::from_value(serde_json::json!({
            "id": CHILD.to_string(), "parent_id": PARENT.to_string(),
            "type": 11, "name": "routine results",
        }))
        .unwrap();
        let _lookups = force_channel_lookups();
        let reads = Arc::new(AtomicUsize::new(0));
        let (checked_thread, count) = (thread.clone(), reads.clone());
        install_channel_lookup(
            "claude",
            Arc::new(move |channel| {
                assert_eq!(channel, CHILD);
                count.fetch_add(1, Ordering::SeqCst);
                let found = checked_thread.clone();
                Box::pin(async move { Ok(Some(found)) })
            }),
        );

        let load = |wire: &serde_json::Value| {
            std::fs::write(&path, serde_json::to_vec(wire).unwrap()).unwrap();
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHILD).unwrap()
        };
        {
            let _boot = force_channels(&[(PARENT, RuntimeHandoffKind::ClaudeTui)]);
            assert!(boot_snapshot().unwrap().child(CHILD).is_none());
            let restored = load(&wire);
            assert_eq!(
                relay_recovery_body_notice(
                    &recorder.http,
                    &shared,
                    &ProviderKind::Claude,
                    &restored,
                    BODY,
                )
                .await,
                RecoveryRelayOutcome::Delivered,
            );
            assert!(
                recorder.calls().is_empty(),
                "sent outbox ACK must prevent a direct repost"
            );
            assert_eq!(
                reads.load(Ordering::SeqCst),
                1,
                "fresh boot must recheck the child"
            );
            assert!(
                recovery_tmux_session_name(&ProviderKind::Claude, &restored).is_none(),
                "a routine thread must not recover the parent display name as a pane"
            );
        }
        {
            let _boot = force_channels(&[(PARENT, RuntimeHandoffKind::ClaudeTui)]);
            let mut foreign = wire.clone();
            foreign["routine_presentation"]["process_thread"]["turn_nonce"] =
                serde_json::json!("another episode");
            let restored = load(&foreign);
            assert_eq!(
                relay_recovery_body_notice(
                    &recorder.http,
                    &shared,
                    &ProviderKind::Claude,
                    &restored,
                    BODY,
                )
                .await,
                RecoveryRelayOutcome::TransientFailure,
                "an invalid pin cannot fall through to an unlisted channel on fresh boot",
            );
        }
        {
            let _boot = force_channels(&[(PARENT, RuntimeHandoffKind::ClaudeTui)]);
            assert!(record_thread_child(&thread, false));
            for kind in [
                serde_json::Value::Null,
                serde_json::json!("future_provider_runtime"),
            ] {
                let mut older = wire.clone();
                older
                    .as_object_mut()
                    .unwrap()
                    .remove("routine_presentation");
                older["runtime_kind"] = kind;
                let restored = load(&older);
                assert_eq!(
                    relay_recovery_body_notice(
                        &recorder.http,
                        &shared,
                        &ProviderKind::Claude,
                        &restored,
                        BODY,
                    )
                    .await,
                    RecoveryRelayOutcome::TransientFailure,
                );
            }
        }
        assert!(recorder.calls().is_empty());
        let surviving = sqlx::query_as::<_, (i64, String)>(
            "SELECT id, status FROM message_outbox WHERE source = 'headless_turn' ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(surviving, vec![(sent_id, "sent".to_string())]);
        db.drop().await;
    })
    .await;
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
