use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::recovery_engine::o_cut_recorder::start_watching;
use crate::services::tui_o::channel_policy::{Adoption, BodyCheck};
use crate::services::tui_o::cutover::test_override;

const BODY: &str = "ADK-C1A-watcher-task-response-body";

/// A watcher task response that sends no body (a preparation failure, a live claim held by the
/// sink, an already delivered or sent-but-uncommitted response, or an owned response whose bot
/// identity lookup fails before any chunk post) leaves a pending adoption.
#[tokio::test]
async fn a_watcher_task_response_that_sends_no_body_leaves_a_pending_adoption() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::a_watcher_task_response_that_sends_no_body_leaves_a_pending_adoption"
    )) {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let cases = [
        ("unprepared", 4_325_410u64),
        ("wait", 4_325_411),
        ("delivered", 4_325_412),
        ("sent", 4_325_413),
        ("identity", 4_325_414),
    ];
    let channels: Vec<_> = cases.iter().map(|&(_, ch)| (ch, ClaudeTui)).collect();
    let _candidates = test_override::force_candidates(&channels);
    for (case, channel) in cases {
        let session = format!("AgentDesk-claude-c1a-{case}");
        let jsonl = temp.path().join(format!("{case}.jsonl"));
        let _tui = test_override::bind_claude_tui_session(&session, &jsonl.to_string_lossy());
        let turn_key = task_delivery::durable_response_turn_key(
            channel, "claude", &session, 0, "", None, 4_300, BODY,
        );
        if !matches!(case, "unprepared" | "identity") {
            let claim = task_delivery::claim_task_response_delivery(
                None,
                channel,
                "claude",
                &session,
                &format!("c1a-{case}-event"),
                &turn_key,
                channel + 1,
                task_delivery::ResponseDeliveryOwner::Sink,
            )
            .await;
            let Ok(task_delivery::ResponseDeliveryClaimOutcome::Owned(claim)) = claim else {
                panic!("{case}: the sink owns the response first: {claim:?}");
            };
            match case {
                "delivered" => task_delivery::mark_task_response_delivered(None, &claim).await,
                "sent" => task_delivery::record_task_response_sent_bounded(None, &claim).await,
                _ => Ok(()),
            }
            .unwrap();
        }
        // A fresh context claims the response for the watcher; the recorder answers no bot user.
        let context = (case == "identity").then(|| {
            task_delivery::TaskNotificationContext::from_stream_json(
                &serde_json::json!({
                    "type": "system", "subtype": "task_notification", "task_id": "c1a-identity",
                    "tool_use_id": "toolu-c1a-identity", "status": "completed",
                    "summary": "background work", "task_notification_kind": "background"
                }),
                &crate::services::session_backend::StreamLineState::new(),
            )
            .expect("task context")
        });
        let check = BodyCheck::watch(channel, BODY);
        let recorder = start_watching(channel, check.clone(), false).await;
        let (mut placeholder, mut restored, mut last_edit) = (None, false, String::new());
        let (mut retry, mut visible, mut present, mut response_claim) = (false, false, false, None);
        apply_watcher_task_response(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
            ChannelId::new(channel),
            &session,
            TaskNotificationKind::Background,
            context.as_ref(),
            &turn_key,
            None,
            None,
            4_300,
            BODY,
            false,
            WatcherTaskResponseLocals {
                placeholder_msg_id: &mut placeholder,
                placeholder_from_restored_inflight: &mut restored,
                last_edit_text: &mut last_edit,
                retry_terminal_delivery_from_offset: &mut retry,
                tui_direct_anchor_terminal_body_visible: &mut visible,
                tui_direct_anchor_or_lease_present_for_lifecycle: &mut present,
                task_response_claim: &mut response_claim,
            },
        )
        .await;
        if case == "identity" {
            let row = task_delivery::claim_existing_task_response_delivery(
                None,
                channel,
                "claude",
                &session,
                &turn_key,
                task_delivery::ResponseDeliveryOwner::Watcher,
            )
            .await;
            assert!(
                matches!(row, Ok(Some(_))),
                "the watcher owned the response: {row:?}"
            );
            assert!(
                retry,
                "a failed identity lookup keeps the frontier for a retry"
            );
        }
        let shown = recorder.contents();
        assert!(!shown.iter().any(|c| c.contains(BODY)), "{case}: {shown:?}");
        check.assert_settled();
        assert_eq!(check.adoption(), Adoption::Pending, "{case}");
    }
}
