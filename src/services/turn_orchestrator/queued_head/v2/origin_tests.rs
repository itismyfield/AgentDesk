// Windows has no directory fsync, so the queued V2 owner these tests adopt refuses every commit.
#![cfg(unix)]

use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::discord_pending_queue_root;
use crate::services::turn_orchestrator::checked_purge::ExpectedQueue;
use crate::services::turn_orchestrator::queued_head::v2_types::{
    AttemptIdentity, AttemptSource, PreparedPayload, TargetIdentity,
};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, ChannelMailboxRegistry};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::{Value, json};
use std::path::PathBuf;

fn original(channel: u64, id: u64) -> crate::services::turn_orchestrator::PendingQueueItem {
    serde_json::from_value(
        json!({"author_id":1,"message_id":id,"source_message_ids":[id],
        "channel_id":channel,"queued_generation":5,"text":format!("input {id}")}),
    )
    .unwrap()
}

/// The provenance a headless start keeps, naming `origin` when it has one.
fn provenance(origin: Option<&str>) -> Value {
    let metadata = origin.map(|o| json!({"human_input": {"origin_id": o}}));
    json!({"origin":"headless","metadata":metadata})
}

async fn owner(registry: &ChannelMailboxRegistry, channel: u64) -> (ChannelMailboxHandle, PathBuf) {
    let dir = discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("owed");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let handle = registry.handle(ChannelId::new(channel));
    handle.v2_adopt(path.clone(), "owed".into()).await.unwrap();
    (handle, path)
}

async fn admit(handle: &ChannelMailboxHandle, channel: u64, id: u64, origin: Option<&str>) {
    let ticket = format!("ticket-{id}");
    let admitted =
        handle.v2_admit_ticket_with_context(original(channel, id), ticket, provenance(origin));
    admitted.await.unwrap();
}

fn on_disk(path: &PathBuf) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn owed(snapshot: &Value) -> Vec<(u64, String, String)> {
    let list = snapshot["owed_drop_receipts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    (list.iter())
        .map(|o| {
            (
                o["message_id"].as_u64().unwrap(),
                o["origins"][0].to_string(),
                o["outcome"].to_string(),
            )
        })
        .collect()
}

fn waiting(channel: u64, origin: &str) -> Option<Waiting> {
    waiting_origin("claude", channel, origin).unwrap()
}

/// A cancel or clear owes each removed origin in its removing commit, never an origin-less input;
/// only an exact ack clears it, and an empty set keeps the file in the shape an older build reads.
#[tokio::test]
async fn removed_origins_are_owed_by_the_removing_commit_until_acked() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_301);
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 11, Some("o-11")).await;
    admit(&handle, channel, 12, Some("o-12")).await;
    admit(&handle, channel, 13, None).await;
    assert_eq!(waiting(channel, "o-11"), Some(Waiting::Accepted));
    assert_eq!(waiting(channel, "o-unknown"), None);

    let cancel = handle.cancel_queued_primary_message(MessageId::new(11), ctx());
    assert!(cancel.await.removed.is_some());
    let disk = on_disk(&path);
    assert_eq!(
        owed(&disk),
        [(11, "\"o-11\"".into(), "\"cancelled\"".into())]
    );
    let Some(Waiting::Owed(first)) = waiting(channel, "o-11") else {
        panic!("a cancelled origin is owed in the file");
    };
    assert_eq!(waiting(channel, "o-12"), Some(Waiting::Accepted));

    let cleared = handle.clear(ctx()).await;
    assert_eq!(cleared.discarded_message_ids.len(), 2);
    let disk = on_disk(&path);
    let ids: Vec<u64> = owed(&disk).iter().map(|o| o.0).collect();
    assert_eq!(ids, [11, 12], "the origin-less input owes nothing");
    assert!(disk["items"].as_array().unwrap().is_empty());

    let ack =
        handle.v2_request(move |slot| slot.as_mut().ok_or("no owner")?.ack_owed_drops(&[first]));
    ack.await.unwrap();
    assert_eq!(waiting(channel, "o-11"), None);
    assert!(matches!(waiting(channel, "o-12"), Some(Waiting::Owed(_))));
    let rest = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    let rest = rest.await.unwrap();
    let ack = handle.v2_request(move |slot| slot.as_mut().ok_or("no owner")?.ack_owed_drops(&rest));
    ack.await.unwrap();
    assert!(on_disk(&path).get("owed_drop_receipts").is_none());
}

#[tokio::test]
async fn a_clear_removes_opaque_admissions_without_attempt_cancellation_debt() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_308);
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 81, None).await;
    let mut opaque = original(channel, 82);
    opaque.source_message_ids = vec![81, 82];
    let admitted = handle
        .v2_admit_ticket_with_context(opaque, "opaque-82".into(), provenance(Some("o-82")))
        .await
        .unwrap();
    assert!(matches!(
        admitted,
        super::super::source_admission::Admitted::RetainedOpaque
    ));
    let before = on_disk(&path);
    assert_eq!(before["retained_inputs"].as_array().unwrap().len(), 1);
    assert_eq!(waiting(channel, "o-82"), Some(Waiting::Accepted));

    let cleared = handle.clear(ctx()).await;
    assert!(cleared.persistence.failed.is_empty());
    assert!(!cleared.persistence.count_unknown);
    let after = on_disk(&path);
    assert!(after["items"].as_array().unwrap().is_empty());
    assert!(after["retained_inputs"].as_array().unwrap().is_empty());
    assert!(owed(&after).is_empty());
    assert_eq!(waiting(channel, "o-82"), None);
}

#[tokio::test]
async fn a_preserving_clear_keeps_the_running_origins_receipt_unsettled() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_309);
    let handle = registry.handle(ChannelId::new(channel));
    let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    assert!(
        handle
            .try_start_turn(
                token.clone(),
                poise::serenity_prelude::UserId::new(1),
                MessageId::new(91),
            )
            .await
    );
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 91, Some("o-running")).await;
    admit(&handle, channel, 92, Some("o-waiting")).await;
    let key = handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let key = owner.reserve(
                attempt("nonce-running"),
                PreparedPayload {
                    frame: "frame".into(),
                    digest: "digest".into(),
                    attribution: json!({}),
                    followers: Vec::new(),
                },
            )?;
            drop(owner.arm(&key, 0)?);
            Ok(key)
        })
        .await
        .unwrap();

    let cleared = handle.discard_waiting_or_refused(ctx()).await.unwrap();
    assert!(cleared.persistence.failed.is_empty());
    assert!(!cleared.persistence.count_unknown);
    assert!(cleared.removed_token.is_none());
    assert_eq!(cleared.discarded_message_ids, [MessageId::new(92)]);
    assert!(std::sync::Arc::ptr_eq(
        handle.snapshot().await.cancel_token.as_ref().unwrap(),
        &token
    ));
    let after = on_disk(&path);
    assert_eq!(after["items"].as_array().unwrap().len(), 1);
    assert_eq!(after["head_injection"]["identity"]["primary"], 91);
    assert!(after["retained_inputs"].as_array().unwrap().is_empty());
    assert_eq!(
        owed(&after),
        [(92, "\"o-waiting\"".into(), "\"cancelled\"".into())]
    );
    assert_eq!(waiting(channel, "o-running"), Some(Waiting::Accepted));
    handle
        .v2_request(move |slot| slot.as_mut().ok_or("no owner")?.settle(&key, true, 1))
        .await
        .unwrap();
    let settled = on_disk(&path);
    assert_eq!(settled["admission_binds"][0]["at"]["kind"], "settled");
    assert!(
        settled["terminal_identities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|terminal| terminal["identity"]["primary"] == 91)
    );
    assert_eq!(waiting(channel, "o-running"), None);
}

fn attempt(nonce: &str) -> AttemptIdentity {
    AttemptIdentity {
        nonce: nonce.into(),
        provider: "claude".into(),
        target: TargetIdentity {
            host: "tmux".into(),
            owner: "owed-pane".into(),
            root: "/test".into(),
            execution_nonce: "execution".into(),
        },
        source: AttemptSource::Existing {
            session_id: "native".into(),
            transcript: "/test/rollout".into(),
            file: [1, 2],
            offset: 0,
        },
    }
}

/// Reserves the head under `nonce` and returns the owner's refusal of it as proven unsent.
async fn refuse_head(
    handle: &ChannelMailboxHandle,
    channel: u64,
    nonce: &str,
    veto: &str,
) -> Value {
    let attempt = attempt(nonce);
    let key = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let payload = PreparedPayload {
            frame: "frame".into(),
            digest: "digest".into(),
            attribution: json!({}),
            followers: Vec::new(),
        };
        let key = owner.reserve(attempt, payload)?;
        drop(owner.arm(&key, 0)?);
        serde_json::to_value(&key).map_err(|e| e.to_string())
    });
    let key = key.await.unwrap();
    let veto = veto.to_string();
    let target = json!({"host":"none","_attempt":key.clone()});
    let (_, proof) = crate::services::claude_tui::owner_input::capture_scope(
        "claude".into(),
        target,
        channel,
        std::sync::Arc::new(|_| Ok(())),
        std::sync::Arc::new(|_, _| Ok(())),
        std::sync::Arc::new(|_| Ok(())),
        || crate::services::claude_tui::owner_input::vetoed(&veto),
    );
    let proof = proof.expect("a veto before any key proves not-sent");
    let retain_key = key.clone();
    let retained = handle.v2_request(move |slot| {
        let key = serde_json::from_value(retain_key).map_err(|e| e.to_string())?;
        slot.as_mut()
            .ok_or("no owner")?
            .retain_not_sent(&key, proof)
    });
    retained.await.unwrap();
    key
}

/// A permanent refusal owes a failed receipt as it is retained; a retryable one owes nothing
/// until the person is told, after which it is never resubmitted.
#[tokio::test]
async fn a_refused_input_owes_failed_once_nothing_will_resubmit_it() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_302);
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 21, Some("o-21")).await;
    refuse_head(&handle, channel, "nonce-21", "InvalidInput").await;
    let disk = on_disk(&path);
    assert_eq!(owed(&disk), [(21, "\"o-21\"".into(), "\"failed\"".into())]);

    admit(&handle, channel, 22, Some("o-22")).await;
    let key = refuse_head(&handle, channel, "nonce-22", "pane_busy").await;
    assert_eq!(
        owed(&on_disk(&path)).len(),
        1,
        "a retryable refusal owes nothing yet"
    );
    assert_eq!(waiting(channel, "o-22"), Some(Waiting::Accepted));
    let told = handle.v2_request(move |slot| {
        let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
        slot.as_mut().ok_or("no owner")?.notice_enqueued(&key)
    });
    told.await.unwrap();
    let ids: Vec<(u64, String)> = owed(&on_disk(&path))
        .into_iter()
        .map(|o| (o.0, o.2))
        .collect();
    assert_eq!(ids, [(21, "\"failed\"".into()), (22, "\"failed\"".into())]);
}

/// The purge a forced cancel runs owes what it drains, while the attempted head stays unowed.
#[tokio::test]
async fn a_purge_owes_untried_origins_and_leaves_the_attempted_head() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_303);
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 31, Some("o-31")).await;
    admit(&handle, channel, 32, Some("o-32")).await;
    let reserve = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let payload = PreparedPayload {
            frame: "frame".into(),
            digest: "digest".into(),
            attribution: json!({}),
            followers: Vec::new(),
        };
        let key = owner.reserve(attempt("nonce-31"), payload)?;
        drop(owner.arm(&key, 0)?);
        Ok(())
    });
    reserve.await.unwrap();
    let fingerprint = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.fingerprint());
    let fingerprint = fingerprint.await.unwrap();
    let expected = ExpectedQueue::of(&handle.snapshot().await).with_v2(fingerprint);
    let purged = handle
        .try_purge_queue_if_unchanged(ctx(), expected)
        .await
        .unwrap();
    let drained: Vec<u64> = purged.receipt.iter().map(|i| i.message_id.get()).collect();
    assert_eq!(drained, [32]);
    assert_eq!(
        owed(&on_disk(&path)),
        [(32, "\"o-32\"".into(), "\"cancelled\"".into())]
    );
    assert_eq!(waiting(channel, "o-31"), Some(Waiting::Accepted));
}

fn ctx() -> crate::services::turn_orchestrator::QueuePersistenceContext {
    let provider = crate::services::provider::ProviderKind::Claude;
    crate::services::turn_orchestrator::QueuePersistenceContext::new(&provider, "owed", None)
}

/// A file the reader cannot parse is an error, never an origin no queue holds.
#[test]
fn an_unreadable_queue_file_is_an_error_not_an_absent_origin() {
    let _root = TestRuntimeRootGuard::new();
    let dir = discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("torn");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("7340304.json"), b"{\"version\":2,\"items\":[").unwrap();
    assert!(waiting_origin("claude", 7_340_304, "o-1").is_err());
    assert_eq!(waiting_origin("claude", 7_340_305, "o-1"), Ok(None));
}

/// An origin an inflight row still names while its owner already owes it failed reads that
/// terminal through the route: a removal not yet in the database outranks a running row.
#[tokio::test(flavor = "current_thread")]
async fn an_owed_terminal_outranks_a_running_row_naming_the_same_origin_pg() {
    let _root = TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_306);
    let insert = crate::db::agents::insert_agent_channels_for_tests;
    insert(&pool, "owed-agent", Some(&channel.to_string()), None).await;
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 61, Some("o-both")).await;
    refuse_head(&handle, channel, "nonce-61", "InvalidInput").await;
    let at = on_disk(&path)["owed_drop_receipts"][0]["at_ms"]
        .as_i64()
        .unwrap();
    let inflight = crate::config::runtime_root()
        .unwrap()
        .join("runtime/discord_inflight/claude");
    std::fs::create_dir_all(&inflight).unwrap();
    let row = json!({"version": 1, "provider": "claude", "user_msg_id": 61,
        "input_origin_ids": ["o-both"]});
    std::fs::write(inflight.join(format!("{channel}.json")), row.to_string()).unwrap();

    let config = crate::config::Config::default();
    let broadcast = crate::eventbus::new_broadcast();
    let app = crate::server::routes::api_router_with_pg(
        crate::engine::PolicyEngine::new(&config).unwrap(),
        config,
        broadcast.clone(),
        crate::eventbus::spawn_batch_flusher(broadcast),
        None,
        Some(pool.clone()),
    );
    let request = axum::http::Request::builder()
        .uri("/agents/owed-agent/input-result?origin_id=o-both")
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:50000".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(app, request).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await;
    let body: Value = serde_json::from_slice(&bytes.unwrap()).unwrap();
    let confirmed = body["terminal"]["confirmed_at"]
        .as_str()
        .unwrap_or_default();
    let confirmed = chrono::DateTime::parse_from_rfc3339(confirmed).map(|t| t.timestamp_millis());
    assert_eq!(
        (body["terminal"]["outcome"].as_str(), confirmed),
        (Some("failed"), Ok(at)),
        "{body}"
    );
    pool.close().await;
    db.drop().await;
}

/// Telling a refused input owes at most once: a permanent refusal was owed when retained, and a
/// repeated notice owes nothing after the receipt is settled, so its time stays the first one.
#[tokio::test(flavor = "current_thread")]
async fn a_repeated_notice_never_owes_a_settled_refusal_again_pg() {
    let _root = TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_307);
    let (handle, path) = owner(&registry, channel).await;
    let id = ChannelId::new(channel);
    let settle = || crate::services::discord::settle_v2_drops(Some(&pool), &handle, "claude", id);
    let confirmed = |message: u64| {
        let sql = "SELECT terminal_confirmed_at FROM turns WHERE turn_id = $1";
        let read = sqlx::query_scalar(sql).bind(format!("discord:{channel}:{message}"));
        async { read.fetch_one(&pool).await.unwrap() }
    };
    let tell = |key: Value| {
        handle.v2_request(move |slot| {
            let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
            slot.as_mut().ok_or("no owner")?.notice_enqueued(&key)
        })
    };
    for (message, veto) in [(71, "InvalidInput"), (72, "pane_busy")] {
        admit(&handle, channel, message, Some(&format!("o-{message}"))).await;
        let key = refuse_head(&handle, channel, &format!("nonce-{message}"), veto).await;
        if veto == "pane_busy" {
            tell(key.clone()).await.unwrap();
        }
        settle().await;
        assert!(owed(&on_disk(&path)).is_empty(), "{veto}");
        let first: chrono::DateTime<chrono::Utc> = confirmed(message).await;
        for _ in 0..2 {
            tell(key.clone()).await.unwrap();
            assert_eq!(owed(&on_disk(&path)), [], "{veto}");
        }
        settle().await;
        assert_eq!(confirmed(message).await, first, "{veto}");
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn a_preserving_clear_cancels_protected_sources_that_were_never_armed() {
    let _root = TestRuntimeRootGuard::new();
    for (channel, reserve) in [(7_340_310, false), (7_340_311, true)] {
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(ChannelId::new(channel));
        let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            handle
                .try_start_turn(
                    token,
                    poise::serenity_prelude::UserId::new(1),
                    MessageId::new(101)
                )
                .await
        );
        let (handle, path) = owner(&registry, channel).await;
        admit(&handle, channel, 101, Some("o-unarmed")).await;
        if reserve {
            handle
                .v2_request(|slot| {
                    slot.as_mut().ok_or("no owner")?.reserve(
                        attempt("unarmed"),
                        PreparedPayload {
                            frame: "frame".into(),
                            digest: "digest".into(),
                            attribution: json!({}),
                            followers: Vec::new(),
                        },
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let cleared = handle.discard_waiting_or_refused(ctx()).await.unwrap();
        assert!(cleared.persistence.failed.is_empty());
        assert_eq!(cleared.discarded_message_ids, [MessageId::new(101)]);
        assert!(
            !cleared
                .persistence
                .not_waiting
                .contains(&MessageId::new(101))
        );
        let after = on_disk(&path);
        assert!(after["items"].as_array().unwrap().is_empty());
        assert!(after["head_injection"].is_null());
        assert_eq!(
            owed(&after),
            [(101, "\"o-unarmed\"".into(), "\"cancelled\"".into())]
        );
    }
}

#[tokio::test]
async fn a_clear_does_not_owe_cancelled_after_failed_or_uncertain_attempts() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry, 7_340_312).await;
    for (id, veto) in [(111, "InvalidInput"), (112, "pane_busy")] {
        admit(&handle, 7_340_312, id, Some(&format!("o-{id}"))).await;
        let key = refuse_head(&handle, 7_340_312, &format!("n-{id}"), veto).await;
        if id == 112 {
            handle
                .v2_request(move |slot| {
                    let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
                    slot.as_mut().ok_or("no owner")?.notice_enqueued(&key)
                })
                .await
                .unwrap();
        }
    }
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let owed = owner.owed_drops();
            owner.ack_owed_drops(&owed)
        })
        .await
        .unwrap();
    admit(&handle, 7_340_312, 113, Some("o-uncertain")).await;
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let key = owner.reserve(
                attempt("uncertain"),
                PreparedPayload {
                    frame: "frame".into(),
                    digest: "digest".into(),
                    attribution: json!({}),
                    followers: Vec::new(),
                },
            )?;
            drop(owner.arm(&key, 0)?);
            owner.tick(owner.settle_due().ok_or("no due")?)
        })
        .await
        .unwrap();
    admit(&handle, 7_340_312, 114, Some("o-retryable")).await;
    refuse_head(&handle, 7_340_312, "retryable", "pane_busy").await;
    let cleared = handle.clear(ctx()).await;
    assert!(cleared.persistence.failed.is_empty());
    assert_eq!(
        owed(&on_disk(&path)),
        [(114, "\"o-retryable\"".into(), "\"cancelled\"".into())]
    );
}
