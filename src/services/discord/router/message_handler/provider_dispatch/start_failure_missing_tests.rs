//! A normalized start failure settles its exact members once, never retaining them for retry.
use super::*;
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

#[tokio::test]
async fn a_repaired_start_failure_discards_only_its_claim_and_notices_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8212001);
    let mailbox = shared.mailbox(channel);
    let queue = crate::config::runtime_root()
        .unwrap()
        .join("runtime/missing-failure.json");
    mailbox
        .v2_adopt(queue, "missing-failure".into())
        .await
        .unwrap();
    for id in [1, 2] {
        let item: PendingQueueItem = serde_json::from_value(json!({
            "author_id":7, "message_id":id, "channel_id":channel.get(),
            "queued_generation":1, "merge_consecutive":true, "text":format!("original {id}")
        }))
        .unwrap();
        mailbox
            .v2_admit_ticket(item, format!("missing-{id}"))
            .await
            .unwrap();
    }
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let claim = claim_turn(&mailbox, front.1.message_id, &front)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.followers().len(),
        1,
        "the failure owns the merged originals"
    );
    let recovery = MissingRecovery::default();
    recovery.mark();
    let failure = Failure::claimed(&mailbox, &claim).with_recovery(recovery);
    let again = failure.clone();
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    let _http = crate::services::discord::shared_state::test_rest::install(api.http.clone());
    assert_eq!(
        failure
            .commit(
                Kind::Opaque,
                "new execution unavailable".into(),
                Ok(None),
                Some(shared.clone()),
                channel,
                None
            )
            .await
            .unwrap(),
        Some(true)
    );
    let later: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":3,"channel_id":channel.get(),"queued_generation":1,"text":"later"}))
    .unwrap();
    mailbox
        .v2_admit_ticket(later.clone(), "later".into())
        .await
        .unwrap();
    assert_eq!(
        again
            .commit(
                Kind::Opaque,
                "same failure".into(),
                Ok(None),
                Some(shared),
                channel,
                None
            )
            .await
            .unwrap(),
        None
    );
    let state = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert_eq!(state["items"], json!([later]));
    assert_eq!(state["retained_inputs"], json!([]));
    assert_eq!(
        api.take()
            .iter()
            .filter(|call| call.contains("이 입력을 폐기했습니다"))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_discarded_claims_failed_reason_is_retried_once_by_the_existing_outbox_pg() {
    use poise::serenity_prelude::HttpBuilder;
    use std::sync::atomic::{AtomicBool, Ordering};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (_db, pool) = crate::services::discord::host_defer_gate::tests::postgres().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = ChannelId::new(8212202);
    let mailbox = shared.mailbox(channel);
    mailbox
        .v2_adopt(
            crate::config::runtime_root()
                .unwrap()
                .join("runtime/reason-retry.json"),
            "reason-retry".into(),
        )
        .await
        .unwrap();
    for id in [1, 2] {
        let item:PendingQueueItem = serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel.get(),"queued_generation":1,"merge_consecutive":true,"text":format!("original {id}")})).unwrap();
        mailbox
            .v2_admit_ticket(item, format!("reason-{id}"))
            .await
            .unwrap();
    }
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let claim = claim_turn(&mailbox, front.1.message_id, &front)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.followers().len(), 1);
    let recovery = MissingRecovery::default();
    recovery.mark();
    let failure = Failure::claimed(&mailbox, &claim).with_recovery(recovery);
    let failing = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(std::sync::Mutex::new(
        Vec::<(bool, serde_json::Value)>::new(),
    ));
    let (post_fails, recorded) = (failing.clone(), calls.clone());
    let app=axum::Router::new().fallback(axum::routing::any(move |body:axum::body::Bytes| {
        let (post_fails,recorded)=(post_fails.clone(),recorded.clone());
        async move {
            let value:serde_json::Value=serde_json::from_slice(&body).unwrap_or_default();
            let failed=post_fails.load(Ordering::SeqCst);
            if value["content"].as_str().is_some_and(|s| s.contains("이 입력을 폐기했습니다")) {recorded.lock().unwrap().push((!failed,value));}
            if failed {return (axum::http::StatusCode::FORBIDDEN,axum::Json(json!({"code":50013,"message":"fixture transport unavailable"})));}
            (axum::http::StatusCode::OK,axum::Json(json!({"id":"900002","channel_id":"8212202","content":"reason","author":{"id":"42","username":"fixture","discriminator":"0001","avatar":null},"timestamp":"2026-11-17T00:00:00+00:00","edited_timestamp":null,"tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],"embeds":[],"pinned":false,"type":0})))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let _http = crate::services::discord::shared_state::test_rest::install(http);
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    shared.settings.write().await.allowed_channel_ids = vec![channel.get()];
    registry.register("claude".into(), shared.clone()).await;
    assert_eq!(
        failure
            .clone()
            .commit(
                Kind::Opaque,
                "new execution unavailable".into(),
                Ok(None),
                Some(shared.clone()),
                channel,
                None
            )
            .await
            .unwrap(),
        Some(true)
    );
    let state = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert_eq!(state["items"], json!([]));
    assert_eq!(state["retained_inputs"], json!([]));
    assert_eq!(
        calls.lock().unwrap().iter().filter(|(ok, _)| *ok).count(),
        0
    );
    let rows = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT id,status,content FROM message_outbox WHERE reason_code='hosted.recovery_refusal'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        1,
        "a failed final send must hand the reason to the existing outbox"
    );
    assert_eq!(rows[0].1, "pending");
    assert!(!rows[0].2.contains("original 1"));
    let later:PendingQueueItem=serde_json::from_value(json!({"author_id":7,"message_id":3,"channel_id":channel.get(),"queued_generation":1,"text":"later"})).unwrap();
    mailbox
        .v2_admit_ticket(later.clone(), "later".into())
        .await
        .unwrap();
    assert_eq!(
        failure
            .clone()
            .commit(
                Kind::Opaque,
                "repeated failure".into(),
                Ok(None),
                Some(shared.clone()),
                channel,
                None
            )
            .await
            .unwrap(),
        None
    );
    failing.store(false, Ordering::SeqCst);
    let pool = Arc::new(pool);
    assert_eq!(
        crate::server::outbox_worker::drain_message_outbox_once(&pool, &registry, "reason-retry")
            .await,
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM message_outbox WHERE id=$1")
            .bind(rows[0].0)
            .fetch_one(pool.as_ref())
            .await
            .unwrap(),
        "sent"
    );
    assert_eq!(
        crate::server::outbox_worker::drain_message_outbox_once(&pool, &registry, "reason-repeat")
            .await,
        0
    );
    assert_eq!(
        failure
            .commit(
                Kind::Opaque,
                "repeated again".into(),
                Ok(None),
                Some(shared),
                channel,
                None
            )
            .await
            .unwrap(),
        None
    );
    let state = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert_eq!(state["items"], json!([later]));
    assert_eq!(state["retained_inputs"], json!([]));
    let calls = calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|(ok, _)| *ok).count(), 1);
    assert!(calls.len() >= 2);
    let nonce = &calls[0].1["nonce"];
    assert!(nonce.is_string());
    for (_, body) in calls.iter() {
        assert_eq!(&body["nonce"], nonce);
        assert_eq!(body["enforce_nonce"], true);
        assert_eq!(body["message_reference"]["message_id"], "1");
        assert_eq!(body["message_reference"]["fail_if_not_exists"], false);
    }
    server.abort();
}
