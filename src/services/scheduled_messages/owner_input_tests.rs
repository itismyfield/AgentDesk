use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::discord_pending_queue_root;
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

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
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(poise::serenity_prelude::ChannelId::new(123456789));
    let path = discord_pending_queue_root()
        .unwrap()
        .join("scheduled-owner.json");
    handle
        .v2_adopt(path.clone(), "scheduled-owner".into())
        .await
        .unwrap();
    owner_input::OWNER.scope(handle.clone(),async {
        let result=start_agent_turn(&pool,&HealthRegistry::new(),&fire).await.unwrap();
        assert!(matches!(result,AgentTurnStartDisposition::Accepted));
        let (turn,committed,started,error):(String,bool,Option<DateTime<Utc>>,String)=sqlx::query_as("SELECT turn_id,launch_committed_at IS NOT NULL,turn_started_at,error FROM scheduled_message_deliveries WHERE id=$1")
            .bind(&fire.delivery_id).fetch_one(&pool).await.unwrap();
        assert!(committed);assert!(started.is_none());
        let input=ChannelMailboxHandle::restore_owner_input(serde_json::from_str(error.strip_prefix("owner-input:").unwrap()).unwrap()).unwrap();
        assert!(input.original["text"].as_str().unwrap().contains("Frozen snapshot"));
        assert!(input.context["scheduled_snapshot_session_label"].is_string());
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
        handle.v2_arm_head_for_tests("actual-scheduled-attempt").await.unwrap();
        assert!(owner_input::bind_observer(&mut *pool.acquire().await.unwrap(),&running,&handle,"stale-attempt",None).await.is_err());
        assert!(owner_input::bind_observer(&mut *pool.acquire().await.unwrap(),&running,&handle,"actual-scheduled-attempt",None).await.unwrap());
        assert!(!poll_agent_delivery(&pool,running.clone()).await,"a listing from before the observer bind cannot start its timeout clock");
        let running=db::list_running_agent_deliveries_pg(&pool,"owner-poller",LEASE_SECS,10).await.unwrap().pop().unwrap();
        handle.v2_request(|slot|{let owner=slot.as_mut().ok_or("owner absent")?;let key=owner.active_attempt()?;owner.settle(&key,true,1000)}).await.unwrap();
        assert!(poll_agent_delivery(&pool,running).await);
        let state:(String,Option<String>)=sqlx::query_as("SELECT status,turn_outcome FROM scheduled_message_deliveries WHERE id=$1").bind(&fire.delivery_id).fetch_one(&pool).await.unwrap();
        assert_eq!(state,("sent".into(),Some("delivered".into())));
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM session_transcripts WHERE turn_id=$1").bind(&turn).fetch_one(&pool).await.unwrap();assert_eq!(count,0,"snapshot outcome settles without a channel transcript");
        let original=serde_json::from_value(input.original.clone()).unwrap();
        assert!(owner_input::start_owned(&pool,&fire,&turn,original,input.context.clone()).await.is_err(),"stale claim cannot update intent or readmit");
    }).await;
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(snapshot["admission_sequence"], 1);
    pool.close().await;
    db.drop().await;
}
