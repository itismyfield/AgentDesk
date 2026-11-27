// The owner's retry leaf runs through the unix-only Claude owner input.
#![cfg(unix)]

use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::runtime_registry::RestoreRuntimeRegistry;
use crate::services::discord::runtime_store::discord_pending_queue_root;
use crate::services::discord::{SharedData, shared_data_without_policy_for_tests};
use crate::services::engine_runtime::health_port::OwnerInputRuntimePort;
use crate::services::engine_runtime::health_port::owner_input::OwnerInput;
use crate::services::scheduled_messages::owner_input::AgentTurnStartDisposition;
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::ChannelId;
use std::cell::Cell;
use std::sync::Arc;

thread_local! {
    static SCHEDULED_WAKES: Cell<usize> = const { Cell::new(0) };
}

fn count_scheduled_wake(shared: &Arc<SharedData>, channel: ChannelId) {
    assert!(shared.policy.engine.is_none());
    assert_eq!(channel.get(), 123456789);
    SCHEDULED_WAKES.with(|count| count.set(count.get() + 1));
}

struct ResetBootstrapGeneration;

impl Drop for ResetBootstrapGeneration {
    fn drop(&mut self) {
        crate::services::discord::runtime_store::set_process_generation_for_tests(None);
    }
}

#[tokio::test]
async fn postgres_accepted_snapshot_waits_for_exact_owner_then_settles_recorded_outcome() {
    let _root = TestRuntimeRootGuard::new();
    let (db, pool) = create_test_pool("smsg_owner", "accepted snapshot outcome").await;
    let message = insert_one_shot_agent_message(&pool, "owner-agent", "fail").await;
    let digest = context_snapshot::compute_content_digest(
        "Frozen snapshot",
        0,
        0,
        &context_snapshot::SnapshotIntent::default(),
    );
    sqlx::query("INSERT INTO scheduled_message_context_snapshots(id,source_channel_id,transcript_frontier,rendered_context,pair_count,content_digest) VALUES('owner-snapshot','123456789',0,'Frozen snapshot',0,$1)").bind(digest).execute(&pool).await.unwrap();
    sqlx::query("UPDATE scheduled_messages SET context_strategy='snapshot',context_snapshot_id='owner-snapshot' WHERE id=$1").bind(&message.id).execute(&pool).await.unwrap();
    sqlx::query(
        "UPDATE agents SET provider='claude',discord_channel_cc='123456789' WHERE id='owner-agent'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let fire = claim_one(&pool, "owner-worker").await;
    let _restore_registry = RestoreRuntimeRegistry::capture();
    let _restore_generation = ResetBootstrapGeneration;
    let registry = Arc::new(HealthRegistry::new());
    let shared = shared_data_without_policy_for_tests(
        &registry,
        Some(pool.clone()),
        Some(count_scheduled_wake),
    );
    registry.register("claude".into(), shared).await;
    let handle = registry.owner_mailbox("claude", 123456789).await.unwrap();
    let path = discord_pending_queue_root()
        .unwrap()
        .join("scheduled-owner.json");
    handle
        .v2_adopt(path.clone(), "scheduled-owner".into())
        .await
        .unwrap();
    SCHEDULED_WAKES.with(|count| count.set(0));
    let result = start_agent_turn(&pool, registry.as_ref(), &fire)
        .await
        .unwrap();
    assert!(matches!(result, AgentTurnStartDisposition::Accepted));
    assert_eq!(SCHEDULED_WAKES.with(Cell::get), 1);
    owner_input::OWNER.scope(handle.clone(),async {
        let (turn,committed,started,error):(String,bool,Option<DateTime<Utc>>,String)=sqlx::query_as("SELECT turn_id,launch_committed_at IS NOT NULL,turn_started_at,error FROM scheduled_message_deliveries WHERE id=$1")
            .bind(&fire.delivery_id).fetch_one(&pool).await.unwrap();
        assert!(committed);assert!(started.is_none());
        let input=OwnerInput::restore(serde_json::from_str(error.strip_prefix("owner-input:").unwrap()).unwrap()).unwrap();
        assert_eq!(input.admission_committed, Some(true));
        assert!(input.original["text"].as_str().unwrap().contains("Frozen snapshot"));
        assert!(input.context["scheduled_snapshot_session_label"].is_string());
        // The legacy start's source, channel hint and snapshot label, kept for the owner's start.
        assert_eq!(input.context["source"], OUTBOX_SOURCE);
        assert!(input.context["channel_name_hint"].is_string());
        assert_eq!(input.context["synthetic_label"], input.context["scheduled_snapshot_session_label"]);
        let running=db::list_running_agent_deliveries_pg(&pool,"owner-poller",LEASE_SECS,10).await.unwrap().pop().expect("accepted input must be released to the poller in this tick");
        let empty_registry=ChannelMailboxRegistry::default();
        let empty=empty_registry.handle(poise::serenity_prelude::ChannelId::new(123456789));
        let empty_path=path.with_file_name("empty-scheduled-owner.json");
        empty.v2_adopt(empty_path.clone(),"scheduled-owner".into()).await.unwrap();
        let before=std::fs::read(&empty_path).unwrap();
        assert!(!owner_input::OWNER.scope(empty,poll_agent_delivery(&pool,running.clone())).await);
        assert_eq!(before,std::fs::read(&empty_path).unwrap(),"known scheduled admission cannot be recreated after owner loss");
        assert!(turn_completion::record(&pool,&turn,turn_completion::ScheduledTurnOutcome::Delivered).await.unwrap().is_evidence());
        assert!(!poll_agent_delivery(&pool,running.clone()).await,"recorded output alone cannot bypass the owner bind");
        #[cfg(unix)]
        assert!(!bootstrap_held_by_modal(&handle,"bootstrap-attempt",poll_agent_delivery(&pool,running.clone())).await,"an attempt in its bootstrap window is no observer's");
        let after_held = poll_agent_delivery(&pool,running.clone()).await;
        assert!(!after_held,"a stale completion must not settle A after its modal hold");
        let (attempt,frames)=retry_leaf(&handle,"actual-scheduled-attempt").await;
        assert_eq!(frames,["frame-actual-scheduled-attempt"],"the retry's own payload, once");
        assert!(!poll_agent_delivery(&pool,running.clone()).await,"the observer binds the armed attempt before it reads");
        let error:String=sqlx::query_scalar("SELECT error FROM scheduled_message_deliveries WHERE id=$1").bind(&fire.delivery_id).fetch_one(&pool).await.unwrap();
        assert!(error.contains("actual-scheduled-attempt"),"{error}");
        assert!(owner_input::bind_observer(&mut *pool.acquire().await.unwrap(),&running,&handle,"stale-attempt",None).await.is_err());
        assert!(owner_input::bind_observer(&mut *pool.acquire().await.unwrap(),&running,&handle,"actual-scheduled-attempt",None).await.unwrap());
        assert!(!poll_agent_delivery(&pool,running.clone()).await,"a listing from before the observer bind cannot start its timeout clock");
        let running=db::list_running_agent_deliveries_pg(&pool,"owner-poller",LEASE_SECS,10).await.unwrap().pop().unwrap();
        handle.v2_request(move|slot|{let owner=slot.as_mut().ok_or("owner absent")?;let key=serde_json::from_value(attempt).map_err(|e|e.to_string())?;owner.settle(&key,true,1000)}).await.unwrap();
        assert!(poll_agent_delivery(&pool,running).await);
        let state:(String,Option<String>)=sqlx::query_as("SELECT status,turn_outcome FROM scheduled_message_deliveries WHERE id=$1").bind(&fire.delivery_id).fetch_one(&pool).await.unwrap();
        assert_eq!(state,("sent".into(),Some("delivered".into())));
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM session_transcripts WHERE turn_id=$1").bind(&turn).fetch_one(&pool).await.unwrap();assert_eq!(count,0,"snapshot outcome settles without a channel transcript");
        let original=serde_json::from_value(input.original.clone()).unwrap();
        assert!(owner_input::start_owned(&pool,&fire,&handle,&turn,(original,input.namespace.clone(),input.context.clone())).await.is_err(),"stale claim cannot update intent or readmit");
    }).await;
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(snapshot["admission_sequence"], 1);
    pool.close().await;
    db.drop().await;
}

/// Sends a bootstrap retry through the actual cold Claude leaf and reads the renderer's receipts.
pub(crate) async fn retry_leaf(
    mailbox: &crate::services::turn_orchestrator::ChannelMailboxHandle,
    nonce: &str,
) -> (serde_json::Value, Vec<String>) {
    use crate::services::claude_tui::input::submission::tests::{frame_renderer, screen, wait};
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    let fixture = Fixture::after_shared_test_env_lock();
    let session = format!("retry-{}", uuid::Uuid::new_v4().simple());
    let execution = uuid::Uuid::new_v4().simple().to_string();
    let frame = format!("frame-{nonce}");
    frame_renderer(
        &fixture,
        &session,
        &frame,
        &screen(""),
        &screen(&frame),
        &screen(""),
    );
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let context = json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),
        "execution_nonce":execution,"tmux_session":session,"channel_id":7272791,
        "owner_runtime_root":root,"host":"owner-retry-test","expected_native_session_id":null,
        "launch_mode":"fresh","provider_root":null});
    let _prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            serde_json::from_value(context).unwrap(),
        )
        .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&session, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &execution).unwrap();
    let attempt = json!({"nonce":nonce,"provider":"claude","target":{"host":"tmux","owner":session,"root":root,"execution_nonce":execution},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let reserved_frame = frame.clone();
    mailbox
        .v2_request(move |slot| {
            let digest = format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(reserved_frame.as_bytes())
            );
            let payload = json!({"frame":reserved_frame,"digest":digest,"attribution":{}});
            slot.as_mut()
                .ok_or("no owner")?
                .reserve(
                    serde_json::from_value(attempt).map_err(|e| e.to_string())?,
                    serde_json::from_value(payload).map_err(|e| e.to_string())?,
                )
                .map(drop)
        })
        .await
        .unwrap();
    let ticket = Arc::new(Mutex::new(None));
    let transcript = fixture.root.path().join("not-created.jsonl");
    crate::services::claude_tui::readiness::test_support::pre_source(&session, &transcript);
    let pane_name = session.clone();
    let submitted_ticket = ticket.clone();
    let leaf = move |frame: String| {
        let submitted = crate::services::claude::first_input::submit_created(
            &transcript.display().to_string(),
            0,
            &session,
            &frame,
        )
        .map_err(|error| format!("{error:?}"))?;
        *submitted_ticket.lock().unwrap() = Some(submitted);
        Ok::<(), String>(())
    };
    let (identity, outcome) = mailbox
        .v2_cold_observed(1, Arc::new(|| 1), leaf)
        .await
        .unwrap();
    assert_eq!(outcome.result, Ok(()));
    assert!(outcome.not_sent.is_none());
    let pane = fixture.root.path().join(&pane_name);
    if ticket.lock().unwrap().is_some() {
        wait("retry payload and Enter receipts", || {
            pane.join("enters").exists()
        });
    }
    let received = std::fs::read_to_string(pane.join("frame")).unwrap_or_default();
    assert_eq!(received, frame, "B's actual payload");
    assert_eq!(
        std::fs::read(pane.join("payloads")).unwrap().len(),
        1,
        "B content once"
    );
    assert_eq!(
        std::fs::read(pane.join("enters")).unwrap().len(),
        1,
        "B Enter once"
    );
    assert!(ticket.lock().unwrap().is_some(), "B's real submit ticket");
    (identity, vec![received])
}

/// Reserves the head as `nonce` and arms it in its bootstrap window, runs `poll` there, then a
/// modal veto ends it unsent and holds the original for its retry.
#[cfg(unix)]
async fn bootstrap_held_by_modal<T>(
    handle: &ChannelMailboxHandle,
    nonce: &str,
    poll: impl std::future::Future<Output = T>,
) -> T {
    let attempt = serde_json::json!({"nonce":nonce,"provider":"claude","target":{"host":"tmux","owner":"bootstrap-pane","root":"/test","execution_nonce":"execution"},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let reserved = handle.v2_request(move |slot| {
        let payload = serde_json::json!({"frame":"frame","digest":"digest","attribution":{}});
        let attempt = serde_json::from_value(attempt).map_err(|e| e.to_string())?;
        let payload = serde_json::from_value(payload).map_err(|e| e.to_string())?;
        let owner = slot.as_mut().ok_or("no owner")?;
        owner.reserve(attempt, payload).map(drop)
    });
    reserved.await.unwrap();
    let (armed, in_window) = tokio::sync::oneshot::channel();
    let (polled, release) = std::sync::mpsc::channel::<()>();
    let owner = handle.clone();
    let cold = tokio::spawn(async move {
        let effect = move |_| {
            let _ = armed.send(());
            let _ = release.recv();
            crate::services::claude_tui::owner_input::vetoed("Modal");
            Ok::<(), String>(())
        };
        owner
            .v2_cold_observed(0, std::sync::Arc::new(|| 0), effect)
            .await
    });
    if in_window.await.is_err() {
        panic!("cold: {:?}", cold.await.unwrap().map(|r| r.0));
    }
    let during = poll.await;
    polled.send(()).unwrap();
    let (identity, outcome) = cold.await.unwrap().unwrap();
    let proof = outcome
        .not_sent
        .expect("a modal veto proves nothing was sent");
    let held = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let key = serde_json::from_value(identity).map_err(|e| e.to_string())?;
        owner.hold_not_sent(&key, proof)
    });
    held.await.unwrap();
    during
}

#[tokio::test]
async fn start_failure_retained_schedule_closes_without_fallback_pg() {
    let _root = TestRuntimeRootGuard::new();
    let (db, pool) = create_test_pool("smsg_owner_rejected", "start failure terminal close").await;
    let message = insert_one_shot_agent_message(&pool, "rejected-agent", "fail").await;
    sqlx::query("UPDATE agents SET provider='claude',discord_channel_cc='123456789' WHERE id='rejected-agent'").execute(&pool).await.unwrap();
    let fire = claim_one(&pool, "owner-worker").await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(poise::serenity_prelude::ChannelId::new(123456789));
    handle
        .v2_adopt(
            discord_pending_queue_root()
                .unwrap()
                .join("rejected-owner.json"),
            "scheduled-owner".into(),
        )
        .await
        .unwrap();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "5").unwrap();
    owner_input::OWNER.scope(handle.clone(),async {
        assert!(matches!(start_agent_turn(&pool,&HealthRegistry::new(),&fire).await.unwrap(),AgentTurnStartDisposition::Accepted));
        let running=db::list_running_agent_deliveries_pg(&pool,"owner-poller",LEASE_SECS,10).await.unwrap().pop().unwrap();
        assert_eq!(handle.v2_request(|slot| {
            let owner=slot.as_mut().ok_or("owner")?;let front=owner.front().ok_or("front")?;
            owner.grant_front(&front)?;owner.claim_turn(front.1.message_id,&front).ok_or("claim")?;
            let kind=serde_json::from_value(serde_json::json!("endpoint_not_local")).map_err(|e|e.to_string())?;
            owner.start_failed(&front,owner.claim_number(),std::slice::from_ref(&front.1),None,kind,"typed refusal")
        }).await.unwrap(),Some(true));
        let mut stale=running.clone();stale.turn_id=Some("other-turn".into());
        owner_input::close_start_rejected(&pool,&stale,"wrong turn").await.unwrap();
        stale=running.clone();stale.claim_token="other-claim".into();
        owner_input::close_start_rejected(&pool,&stale,"wrong claim").await.unwrap();
        let status:String=sqlx::query_scalar("SELECT status FROM scheduled_message_deliveries WHERE id=$1").bind(&fire.delivery_id).fetch_one(&pool).await.unwrap();assert_eq!(status,"running");
        let (done,mut completed)=tokio::sync::mpsc::unbounded_channel();
        assert!(!owner_input::REJECTION_TEST.scope((pool.clone(),done),poll_agent_delivery(&pool,running)).await);
        assert!(tokio::time::timeout(std::time::Duration::from_secs(10),completed.recv()).await.unwrap().unwrap());
        let state:(String,Option<i64>,String)=sqlx::query_as("SELECT status,fallback_outbox_id,error FROM scheduled_message_deliveries WHERE id=$1").bind(&fire.delivery_id).fetch_one(&pool).await.unwrap();
        assert_eq!((&*state.0,state.1),("failed",None));assert!(state.2.starts_with("start_rejected: "));
        let state:(String,Option<String>)=sqlx::query_as("SELECT status,in_flight_delivery_id FROM scheduled_messages WHERE id=$1").bind(&message.id).fetch_one(&pool).await.unwrap();assert_eq!(state,("failed".into(),None));
    }).await;
    pool.close().await;
    db.drop().await;
}
