//! The actual restart entry refuses unreadable, unplaced and foreign canonical executions.
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
};
use crate::services::discord::commands::restart_channel_for_tests;
use crate::services::discord::queue_io::cancel_backstop_test_support as wake_probe;
use crate::services::discord::queue_io::input_owner::admission::{
    Admission, AdmissionTicket, TurnBind, admit, exact_turn,
};
use crate::services::turn_orchestrator::{PendingQueueItem, v2_queue};
use poise::serenity_prelude as serenity;

#[test]
fn restart_common_refuses_future_unplaced_and_foreign_canonical_rows_pg() {
    let Some((rig, runtime)) = rig_child(
        "restart_refusal_tests::restart_common_refuses_future_unplaced_and_foreign_canonical_rows_pg",
    ) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        // A legacy row is no refusal: the greeting's own turn takes it (restart_channel_tests).
        let cases = [(0, "future-schema"), (2, "unplaced-pending"), (3, "foreign-owner")];
        for (index, case) in cases {
            let node = Node::bind(&rig, HostedState::Bound, Some(DISPATCH)).await;
            let mut record = rig.record_as(
                (&node.shared.token_hash, &node.key),
                HostedState::Bound,
                "claude",
            );
            let raw = match index {
                0 => {
                    let mut future = serde_json::to_value(&record).unwrap();
                    future["schema"] = json!(9917);
                    Some(future)
                }
                2 => {
                    record.state = HostedState::Pending;
                    record.location = None;
                    record.expected = None;
                    Some(serde_json::to_value(&record).unwrap())
                }
                3 => {
                    record.owner.owner_node = "foreign-restart-owner".into();
                    record.location.as_mut().unwrap().execution_node =
                        record.owner.owner_node.clone();
                    Some(serde_json::to_value(&record).unwrap())
                }
                _ => unreachable!(),
            };
            sqlx::query("UPDATE sessions SET hosted_execution=$2, claude_session_id=$3, raw_provider_session_id=$3 WHERE session_key=$1")
                .bind(&node.key)
                .bind(raw.clone())
                .bind("019ee441-173c-735c-9a24-812800000002")
                .execute(&node.pool).await.unwrap();
            let channel_key = CHANNEL.to_string();
            let lookup = load_hosted_execution_pg(&node.pool, HostedLookupKey::Canonical {
                provider: "claude",
                identity: CanonicalSessionIdentity {
                    kind: SessionIdentityKind::DiscordChannel,
                    discord_token_hash: &node.shared.token_hash,
                    channel_id: &channel_key,
                },
            }).await;
            let HostedLookup::Found(found) = lookup else {
                panic!("{case}: fixture must resolve one canonical row: {lookup:?}");
            };
            match (index, &found.record) {
                (0, HostedRecord::Unknown(value)) => assert_eq!(value["schema"], json!(9917)),
                (2, HostedRecord::Known(known)) => {
                    assert_eq!(known.state, HostedState::Pending);
                    assert!(known.location.is_none() && known.expected.is_none());
                }
                (3, HostedRecord::Known(known)) => {
                    assert_eq!(known.state, HostedState::Bound);
                    assert_eq!(known.owner.owner_node, "foreign-restart-owner");
                    assert_eq!(known.location.as_ref().unwrap().execution_node,
                        known.owner.owner_node);
                    assert_eq!(crate::config::session_hosts::local_node().as_deref(), Some(herdr::NODE));
                    assert_ne!(known.owner.owner_node, herdr::NODE);
                }
                _ => panic!("{case}: fixture has the wrong HostedRecord class: {:?}", found.record),
            }
            let persistence = crate::services::discord::queue_persistence_context(
                &node.shared, &ProviderKind::Claude, channel(),
            );
            let mailbox = node.shared.mailbox(channel());
            mailbox.v2_ensure_channel(&persistence, channel()).await.unwrap();
            let original: PendingQueueItem = serde_json::from_value(json!({
                "author_id":7,"message_id":61,"channel_id":CHANNEL,
                "source_message_ids":[61],"queued_generation":1,
                "merge_consecutive":false,"text":"restart refusal preserves this original"
            })).unwrap();
            let ticket = AdmissionTicket::new();
            assert!(matches!(admit(&mailbox, &original, ticket.clone()).await, Admission::Accepted(_)));
            node.start(51, &format!("restart-refusal-{case}"), Some(DISPATCH)).await;
            let active = test_turn::active(&node.shared, channel()).await.unwrap();
            let previous_wake = wake_probe::set_owner_wake_for_tests(channel(), Some(Arc::new(|| {})));
            let before_row: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE session_key=$1")
                .bind(&node.key).fetch_one(&node.pool).await.unwrap();
            assert_eq!(before_row["hosted_execution"], raw.clone().unwrap_or(Value::Null));
            assert_eq!(before_row["status"], json!("turn_active"));
            assert_eq!(before_row["active_dispatch_id"], json!(DISPATCH));
            assert_eq!(before_row["claude_session_id"], json!("019ee441-173c-735c-9a24-812800000002"));
            assert_eq!(before_row["raw_provider_session_id"], before_row["claude_session_id"]);
            let before_dispatches: Value = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(d) ORDER BY d.id), '[]'::jsonb) FROM task_dispatches d")
                .fetch_one(&node.pool).await.unwrap();
            let before_owner = mailbox.v2_request(|slot| slot.as_ref().ok_or("owner missing")?.json()).await.unwrap();
            assert_eq!(before_owner["items"].as_array().unwrap().len(), 1);
            let (queue_path, _) = v2_queue(&persistence, channel()).unwrap();
            let before_bytes = std::fs::read(&queue_path).unwrap();
            let before_writes = rig.writes();
            assert!(before_writes.is_empty(), "{case}: fixture starts without physical writes");
            let before_held = herdr::held();
            let data = crate::services::discord::Data {
                shared: node.shared.clone(), token: "restart-refusal-fixture".into(),
                provider: ProviderKind::Claude,
            };
            let http = Arc::new(serenity::Http::new(""));
            let call = restart_channel_for_tests(&http, &data, channel(),
                ("restart-refusal", "!restart", 900_812_820 + index as u64));
            let replies = tokio::time::timeout(Duration::from_secs(10), call)
                .await.expect("the actual restart common entry completes").unwrap();
            let final_reply = replies.last().expect("a refusal is visible to the user");
            let expected = match index {
                2 => "세션을 재시작하지 않았습니다. 새 세션의 생성 응답을 아직 확인하지 못했습니다.\n메시지를 보내면 기존 경로가 이 상태를 해결합니다.",
                _ => "세션을 재시작하지 않았습니다. 현재 세션을 안전하게 확인할 수 없습니다.",
            };
            assert_eq!(final_reply, expected, "{case}: {replies:?}");
            assert!(replies.iter().all(|reply| !reply.contains("손실")),
                "{case}: a request that ends nothing warns of no lost turn: {replies:?}");
            assert!(replies.iter().all(|reply|
                !reply.contains("종료를 확인했습니다") && !reply.contains("Ready") &&
                !reply.contains("인사말 turn을 시작했습니다")), "{case}: {replies:?}");
            let after_row: Value = sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE session_key=$1")
                .bind(&node.key).fetch_one(&node.pool).await.unwrap();
            assert_eq!(after_row, before_row, "{case}: raw row, selector, status and dispatch are unchanged");
            let after_dispatches: Value = sqlx::query_scalar("SELECT COALESCE(jsonb_agg(to_jsonb(d) ORDER BY d.id), '[]'::jsonb) FROM task_dispatches d")
                .fetch_one(&node.pool).await.unwrap();
            assert_eq!(after_dispatches, before_dispatches, "{case}: no dispatch field changed");
            let after_owner = mailbox.v2_request(|slot| slot.as_ref().ok_or("owner missing")?.json()).await.unwrap();
            assert_eq!(after_owner, before_owner, "{case}: queue custody is unchanged");
            assert_eq!(std::fs::read(&queue_path).unwrap(), before_bytes, "{case}: durable queue bytes are unchanged");
            assert_eq!(exact_turn(&mailbox, &ticket, &original).await.unwrap(), TurnBind::NotArmed,
                "{case}: the preserved ticket never started");
            let after_active = test_turn::active(&node.shared, channel()).await.unwrap();
            assert!(Arc::ptr_eq(&active, &after_active), "{case}: the active actor is preserved");
            assert!(!active.cancelled.load(Ordering::SeqCst), "{case}: the active turn was not cancelled");
            assert_eq!(herdr::held(), before_held, "{case}: the input hold is unchanged");
            assert_eq!(rig.writes(), before_writes, "{case}: no physical input or close");
            wake_probe::set_owner_wake_for_tests(channel(), previous_wake);
            node.finish().await;
        }
    });
}
