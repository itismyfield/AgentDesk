use super::*;
use crate::services::discord::{queue_io::input_owner, queue_persistence_context};
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, PendingQueueItem,
    pending_queue_item_to_intervention,
};
use poise::serenity_prelude::ChannelId;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

const A: u64 = 1_490_000_000_000_000_100;
const B: u64 = 1_490_000_000_000_000_200;
const C: u64 = 1_490_000_000_000_000_300;
const D: u64 = 1_490_000_000_000_000_400;
const E: u64 = 1_490_000_000_000_000_500;
const S: u64 = 8_000_000_000_000_000_600;
fn channel() -> ChannelId {
    ChannelId::new(discord_mock::CHANNEL_ID)
}
fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":if id == S {1} else {7},
        "message_id":id,"channel_id":channel().get(),"source_message_ids":[id],
        "queued_generation":5,"created_at_wall_time_ms":1793923200000_u64,
        "merge_consecutive":true,"text":format!("앞부분 {id}\n{}","한글".repeat(70))}))
    .unwrap()
}
fn attempt(id: u64) -> Value {
    json!({"nonce":format!("cancel-{id}"),"provider":"claude",
        "target":{"host":"herdr","owner":"cancel-fixture","root":"/fixture","execution_nonce":"fixture"},
        "source":{"kind":"existing","session_id":"fixture","transcript":"/fixture/log","file":[1,2],"offset":0}})
}
fn payload(followers: Vec<PendingQueueItem>) -> Value {
    json!({"frame":"frame","digest":"digest","attribution":{},"followers":followers})
}
async fn empty_owner(data: &Data) -> (ChannelMailboxHandle, PathBuf) {
    let p = queue_persistence_context(&data.shared, &data.provider, channel());
    let (path, _) = crate::services::turn_orchestrator::v2_queue(&p, channel()).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let handle = data.shared.mailbox(channel());
    handle
        .v2_adopt(path.clone(), data.shared.token_hash.clone())
        .await
        .unwrap();
    (handle, path)
}
async fn snapshot(handle: &ChannelMailboxHandle) -> Value {
    handle
        .v2_request(|s| s.as_ref().ok_or("no owner")?.json())
        .await
        .unwrap()
}
async fn enqueue(handle: &ChannelMailboxHandle, ids: &[u64]) {
    for &id in ids {
        handle
            .v2_admit_ticket_with_context(
                item(id),
                format!("ticket-{id}"),
                json!({"origin":"headless",
            "metadata":{"human_input":{"origin_id":format!("origin-{id}")}}}),
            )
            .await
            .unwrap();
    }
}
async fn seeded(data: &Data) -> (ChannelMailboxHandle, PathBuf) {
    let (handle, path) = empty_owner(data).await;
    for id in [D, E, 700, 800, 900, 1000] {
        enqueue(&handle, &[id]).await;
        handle.v2_request(move |s| {
            let o=s.as_mut().ok_or("no owner")?;
            let key=o.reserve(serde_json::from_value(attempt(id)).unwrap(),serde_json::from_value(payload(vec![])).unwrap())?;
            drop(o.arm(&key,0)?);
            let mut input=o.snapshot.head_injection.take().unwrap();
            o.snapshot.items.clear();
            input.phase=serde_json::from_value(json!(if id==E {"unknown"} else {"reserved"})).unwrap();
            if [800,900].contains(&id) {input.attempt=None;}
            let (reason,disposition)=if id==E {(json!({"kind":"unconfirmed"}),Value::Null)}
                else if id==900 {(json!({"kind":"rejected","reason":"opaque"}),json!("opaque_unsent"))}
                else if id==800 {(json!({"kind":"rejected","reason":"start_rejected: fixture"}),Value::Null)}
                else {(json!({"kind":"rejected","reason":if id==1000 {"known_not_sent: vetoed: InvalidInput"}
                    else {"known_not_sent: synthetic_waits_idle"}}),json!("known_not_sent"))};
            o.snapshot.retained_inputs.push(serde_json::from_value(json!({"input":input,"reason":reason,
                "disposition":disposition,"notice":{"kind":"retained_retry_required","enqueued":id==700}})).map_err(|e|e.to_string())?);
            o.tick(0)
        }).await.unwrap();
    }
    enqueue(&handle, &[A]).await;
    handle
        .v2_request(|s| {
            let o = s.as_mut().ok_or("no owner")?;
            let key = o.reserve(
                serde_json::from_value(attempt(A)).unwrap(),
                serde_json::from_value(payload(vec![])).unwrap(),
            )?;
            drop(o.arm(&key, u64::MAX - 60000)?);
            Ok(())
        })
        .await
        .unwrap();
    enqueue(&handle, &[B, C, S]).await;
    (handle, path)
}
fn ids(state: &Value) -> Vec<u64> {
    state["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["message_id"].as_u64().unwrap())
        .collect()
}
fn content(result: &Value) -> String {
    result["replies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["data"]["content"].as_str().unwrap_or_default())
        .chain(
            result["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["content"].as_str().unwrap_or_default()),
        )
        .collect::<Vec<_>>()
        .join("\n")
}
async fn authorized() -> Data {
    let mut data = data(ProviderKind::Claude);
    Arc::get_mut(&mut data.shared).unwrap().token_hash = uuid::Uuid::new_v4().to_string();
    data.shared.settings.write().await.owner_user_id = Some(42);
    data
}
fn feedback(data: &Data, id: u64) {
    assert!(
        data.shared
            .turn_view_reconciler
            .ops()
            .iter()
            .any(|op| op.target.channel_id == channel()
                && op.target.message_id.get() == id
                && op.add
                && op.emoji
                    == crate::services::discord::queue_exit_feedback_emoji(
                        crate::services::turn_orchestrator::QueueExitKind::Cancelled
                    )),
        "cancel feedback for {id}"
    );
}
#[test]
fn cancel_queued_t1_registration() {
    let commands = run_bot_build_slash_commands();
    let c = commands.iter().find(|c| c.name == "cancel-queued").unwrap();
    let v = serde_json::to_value(c.create_as_slash_command().unwrap()).unwrap();
    assert_eq!(v["options"][0]["required"], false);
    assert_eq!(v["options"][0]["autocomplete"], true);
}
#[test]
fn cancel_queued_t2_v2_candidates_are_read_only_and_bounded() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        let (handle, path) = seeded(&data).await;
        let before = std::fs::read(&path).unwrap();
        let state = snapshot(&handle).await;
        let sequence = handle
            .v2_request(|s| Ok(s.as_ref().unwrap().cancel_sequence_for_tests()))
            .await
            .unwrap();
        let choices = rig
            .invoke(&data, "cancel-queued", Some(("message_id", "")))
            .await;
        let choices = choices["choices"].as_array().unwrap();
        assert_eq!(
            choices
                .iter()
                .map(|c| c["value"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [B.to_string(), C.to_string(), S.to_string(), D.to_string()]
        );
        for choice in choices {
            let name = choice["name"].as_str().unwrap();
            assert!(name.contains("11-06 09:00"), "KST: {name}");
            assert!(
                name.contains("앞부분")
                    && name.chars().count() <= 100
                    && !name.chars().any(char::is_control)
            );
        }
        assert!(
            choices
                .iter()
                .find(|c| c["value"] == D.to_string())
                .unwrap()["name"]
                .as_str()
                .unwrap()
                .contains("보관")
        );
        assert!(
            choices
                .iter()
                .find(|c| c["value"] == S.to_string())
                .unwrap()["name"]
                .as_str()
                .unwrap()
                .contains("합성")
        );
        assert_eq!(before, std::fs::read(&path).unwrap());
        assert_eq!(state, snapshot(&handle).await);
        assert_eq!(
            sequence,
            handle
                .v2_request(|s| Ok(s.as_ref().unwrap().cancel_sequence_for_tests()))
                .await
                .unwrap()
        );
        let mut cold = authorized().await;
        Arc::get_mut(&mut cold.shared).unwrap().token_hash = data.shared.token_hash.clone();
        let count = cold.shared.mailboxes.snapshot_all().await.len();
        assert_eq!(
            rig.invoke(&cold, "cancel-queued", Some(("message_id", "")))
                .await["choices"],
            json!([])
        );
        assert_eq!(count, cold.shared.mailboxes.snapshot_all().await.len());
        cold.shared.mailboxes.insert_silent_for_test(channel());
        let result = tokio::time::timeout(
            Duration::from_millis(2750),
            rig.invoke(&cold, "cancel-queued", Some(("message_id", ""))),
        )
        .await
        .unwrap();
        assert_eq!(result["choices"], json!([]));
    });
}
async fn seed_v1(data: &Data, ids: &[u64]) -> ChannelMailboxHandle {
    let handle = data.shared.mailbox(channel());
    for &id in ids {
        let mut i = pending_queue_item_to_intervention(item(id), SystemTime::now(), Instant::now());
        i.merge_consecutive = false;
        i.has_reply_boundary = true;
        assert!(
            handle
                .enqueue(
                    i,
                    queue_persistence_context(&data.shared, &data.provider, channel())
                )
                .await
                .enqueued
        );
    }
    handle
}
#[test]
fn cancel_queued_t3_v1_soft_candidates() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        seed_v1(&data, &(B..B + 30).collect::<Vec<_>>()).await;
        let all = rig
            .invoke(&data, "cancel-queued", Some(("message_id", "")))
            .await;
        assert_eq!(all["choices"].as_array().unwrap().len(), 25);
        assert!(
            all["choices"].as_array().unwrap().iter().all(|c| c["name"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 100)
        );
        assert_eq!(
            rig.invoke(&data, "cancel-queued", Some(("message_id", &B.to_string())))
                .await["choices"][0]["value"],
            B.to_string()
        );
    });
}
#[test]
fn cancel_queued_t4_scope_and_authorization() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        let (handle, path) = seeded(&data).await;
        let other = data.shared.mailbox(ChannelId::new(channel().get() + 1));
        other
            .v2_adopt(
                path.with_file_name(format!("{}.json", channel().get() + 1)),
                data.shared.token_hash.clone(),
            )
            .await
            .unwrap();
        other
            .v2_request(|s| {
                s.as_mut()
                    .unwrap()
                    .enqueue_fixture(&item(123456))
                    .map_err(String::from)
            })
            .await
            .unwrap();
        let choices = rig
            .invoke(&data, "cancel-queued", Some(("message_id", "")))
            .await;
        assert_eq!(choices["choices"].as_array().unwrap().len(), 4);
        assert!(!choices.to_string().contains("123456"));
        data.shared.settings.write().await.owner_user_id = Some(43);
        assert_eq!(
            rig.invoke(&data, "cancel-queued", Some(("message_id", "")))
                .await["choices"],
            json!([])
        );
        let wrong = Data {
            provider: ProviderKind::Codex,
            ..authorized().await
        };
        let (wrong_handle, wrong_path) = empty_owner(&wrong).await;
        enqueue(&wrong_handle, &[987654]).await;
        let wrong_bytes = std::fs::read(&wrong_path).unwrap();
        assert_eq!(
            rig.invoke(&wrong, "cancel-queued", Some(("message_id", "")))
                .await["choices"],
            json!([])
        );
        assert_eq!(std::fs::read(&wrong_path).unwrap(), wrong_bytes);
        assert_eq!(ids(&snapshot(&handle).await), [A, B, C, S]);
    });
}
#[test]
fn cancel_queued_t5_selected_id_cancels_only_that_original() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        let (handle, _) = seeded(&data).await;
        let choices = rig
            .invoke(&data, "cancel-queued", Some(("message_id", &B.to_string())))
            .await;
        let id = choices["choices"][0]["value"].as_str().expect("B choice");
        let result = rig.invoke_cancel(&data, Some(id)).await;
        assert!(content(&result).contains("취소했어요"), "{result}");
        assert_eq!(ids(&snapshot(&handle).await), [A, C, S]);
        feedback(&data, B);
    });
}
#[test]
fn cancel_queued_t6_single_cancel_reports_storage_and_mailbox_errors() {
    isolated(async {
        for unreachable in [false, true] {
            let rig = Rig::new().await;
            let data = authorized().await;
            let (handle, _) = seeded(&data).await;
            if unreachable {
                data.shared.mailboxes.insert_unreachable_for_test(channel());
            } else {
                handle
                    .v2_request(|s| {
                        s.as_mut().unwrap().cancel_fault_for_tests(true);
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
            let reply = content(&rig.invoke_cancel(&data, Some(&B.to_string())).await);
            assert!(
                reply.contains("취소") && reply.contains("확인하지 못"),
                "{reply}"
            );
            assert!(!reply.contains("이미 처리"), "{reply}");
        }
    });
}
#[test]
fn cancel_queued_t7_clear_retained_is_durable_quiet_and_preserves_evidence() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        let (handle, path) = seeded(&data).await;
        let before = snapshot(&handle).await;
        let result = rig.invoke_cancel(&data, None).await;
        let reply = content(&result);
        assert!(
            reply.contains("9개") && reply.contains("앞부분") && reply.contains("유지 1"),
            "{result}"
        );
        let state = snapshot(&handle).await;
        assert_eq!(ids(&state), [A]);
        assert_eq!(state["head_injection"], before["head_injection"]);
        for entry in state["retained_inputs"].as_array().unwrap() {
            assert_eq!(entry["notice"], json!({"kind":"cleared","enqueued":true}));
        }
        let uncertain = |v: &Value| {
            v["retained_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["input"]["original"]["message_id"] == E)
                .unwrap()
                .clone()
        };
        let mut expected = uncertain(&before);
        expected["notice"] = json!({"kind":"cleared","enqueued":true});
        assert_eq!(uncertain(&state), expected);
        let cancelled: std::collections::BTreeSet<_> = state["owed_drop_receipts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|o| o["outcome"] == "cancelled")
            .map(|o| o["message_id"].as_u64().unwrap())
            .collect();
        assert_eq!(cancelled, [B, C, D, S].into_iter().collect());
        feedback(&data, B);
        feedback(&data, C);
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(20))
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        assert_eq!(
            input_owner::notice::drain_synchronously_for_tests(
                &pool,
                &handle,
                "claude",
                channel().get()
            )
            .await
            .unwrap(),
            0
        );
        handle
            .v2_adopt(path, data.shared.token_hash.clone())
            .await
            .unwrap();
        handle
            .v2_request(|s| {
                let o = s.as_mut().unwrap();
                o.recover()?;
                assert!(o.requeue_known_not_sent()?.is_empty());
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            input_owner::notice::drain_synchronously_for_tests(
                &pool,
                &handle,
                "claude",
                channel().get()
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(ids(&snapshot(&handle).await), [A]);
        let uncertain = uncertain(&before);
        let key = json!({"identity": uncertain["input"]["identity"], "attempt": uncertain["input"]["attempt"]});
        handle
            .v2_request(move |s| {
                s.as_mut()
                    .unwrap()
                    .settle(&serde_json::from_value(key).unwrap(), true, 0)
            })
            .await
            .unwrap();
        assert!(
            !snapshot(&handle).await["retained_inputs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["input"]["original"]["message_id"] == E)
        );
        assert!(content(&Rig::new().await.invoke_cancel(&data, None).await).contains("0개"));
    });
}
#[test]
fn cancel_queued_t8_v1_no_argument_reports_excerpts() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        let handle = seed_v1(&data, &[B, C]).await;
        let reply = content(&rig.invoke_cancel(&data, None).await);
        assert!(reply.contains("2개") && reply.contains("앞부분"), "{reply}");
        assert!(handle.snapshot().await.intervention_queue.is_empty());
    });
}
#[test]
fn cancel_queued_t9_clear_errors_never_become_zero_success() {
    isolated(async {
        for fault in [Some(true), Some(false), None] {
            let rig = Rig::new().await;
            let data = authorized().await;
            if let Some(fault) = fault {
                let (h, _) = seeded(&data).await;
                h.v2_request(move |s| {
                    s.as_mut().unwrap().cancel_fault_for_tests(fault);
                    Ok(())
                })
                .await
                .unwrap();
            } else {
                seed_v1(&data, &[B, C]).await;
                ChannelMailboxRegistry::fail_clear_removals(channel(), false, 2, false);
            }
            let reply = content(&rig.invoke_cancel(&data, None).await);
            assert!(reply.contains("확인하지 못"), "{reply}");
            assert!(
                !reply.contains("0개") && !reply.contains("비웠어요"),
                "{reply}"
            );
        }
    });
}
#[test]
fn cancel_queued_t10_invalid_explicit_id_never_clears() {
    isolated(async {
        let data = authorized().await;
        let (handle, path) = seeded(&data).await;
        let before = std::fs::read(&path).unwrap();
        for id in ["", "  ", "abc", "0"] {
            let rig = Rig::new().await;
            let reply = content(&rig.invoke_cancel(&data, Some(id)).await);
            assert!(reply.contains("유효한 큐 메시지 ID"), "{id:?}: {reply}");
            assert_eq!(before, std::fs::read(&path).unwrap());
        }
        assert_eq!(ids(&snapshot(&handle).await), [A, B, C, S]);
    });
}
#[test]
fn cancel_queued_t11_follower_cancel_revokes_claim_so_drive_progresses() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        let (handle, _) = empty_owner(&data).await;
        enqueue(&handle, &[A, B]).await;
        let front = handle
            .v2_request(|s| {
                let o = s.as_mut().unwrap();
                let f = o.front().unwrap();
                o.grant_front(&f)?;
                Ok(f)
            })
            .await
            .unwrap();
        let claim = input_owner::drive::claim_turn(&handle, A, &front)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claim.followers().len(), 1);
        assert!(
            content(&rig.invoke_cancel(&data, Some(&B.to_string())).await).contains("취소했어요")
        );
        let (front, number, followers) = (
            claim.front().clone(),
            claim.number(),
            claim.followers().to_vec(),
        );
        assert!(
            handle
                .v2_request(move |s| s.as_mut().unwrap().reserve_claimed(
                    (&front, number),
                    serde_json::from_value(attempt(A)).unwrap(),
                    serde_json::from_value(payload(followers)).unwrap()
                ))
                .await
                .is_err()
        );
        assert_eq!(
            input_owner::drive::grant_stalled_for_tests(handle.clone(), channel().get())
                .await
                .expect("A must progress after follower cancellation")
                .1
                .message_id,
            A
        );
    });
}
#[test]
fn cancel_queued_t12_clear_keeps_active_token_and_held_watch_cannot_restart() {
    isolated(async {
        let rig = Rig::new().await;
        let data = authorized().await;
        let handle = seed_v1(&data, &[B]).await;
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            handle
                .try_start_turn(
                    token.clone(),
                    serenity::UserId::new(7),
                    serenity::MessageId::new(42)
                )
                .await
        );
        let reply = content(&rig.invoke_cancel(&data, None).await);
        assert!(reply.contains("비웠어요"), "{reply}");
        assert!(
            handle
                .snapshot()
                .await
                .cancel_token
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(t, &token))
        );
        assert!(!token.cancelled.load(std::sync::atomic::Ordering::SeqCst));
        let data = authorized().await;
        let (handle, _) = empty_owner(&data).await;
        enqueue(&handle, &[S]).await;
        let v2_token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            handle
                .try_start_turn(
                    v2_token.clone(),
                    serenity::UserId::new(7),
                    serenity::MessageId::new(43)
                )
                .await
        );
        handle
            .v2_request(|s| {
                let o = s.as_mut().unwrap();
                let f = o.front().unwrap();
                o.grant_front(&f)?;
                o.hold_front(&f)
            })
            .await
            .unwrap();
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = seen.clone();
        let drive = Arc::new(input_owner::drive::Drive {
            mailbox: handle.clone(),
            channel: channel().get(),
            settled: None,
            handoff: Arc::new(move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { Ok(()) })
            }),
        });
        input_owner::held_watch::arm(
            channel().get(),
            Box::new(|| crate::services::claude_tui::busy_inject::Screen::Ready),
        );
        input_owner::held_watch::watch(drive, S);
        assert!(content(&Rig::new().await.invoke_cancel(&data, None).await).contains("1개"));
        assert!(
            handle
                .snapshot()
                .await
                .cancel_token
                .as_ref()
                .is_some_and(|t| Arc::ptr_eq(t, &v2_token))
        );
        assert!(!v2_token.cancelled.load(std::sync::atomic::Ordering::SeqCst));
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 0);
    });
}
