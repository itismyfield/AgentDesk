use crate::config::runtime_profile::InputFallbackScope;
use crate::services::cluster::intake_router_hook::owner_record::{
    OwnerIdentity, cleanup, snapshot::tests::Fixture,
};
use crate::services::cluster::intake_router_hook::pg_tests::ctx_for_channel;
use crate::services::cluster::intake_router_hook::*;

struct Config {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}
fn install(edit: impl FnOnce(&mut crate::config::ClusterIntakeRoutingConfig)) -> Config {
    let live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.yaml");
    std::fs::write(&path, "server: {}\n").unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let guards = [
        set("AGENTDESK_ROOT_DIR", root.path()),
        set("AGENTDESK_CONFIG", &path),
    ];
    reinstall(edit);
    Config {
        _guards: guards,
        _live: live,
        _root: root,
    }
}
/// Swaps the live config under the env lock `install` already holds.
fn reinstall(edit: impl FnOnce(&mut crate::config::ClusterIntakeRoutingConfig)) {
    let mut config = crate::config::Config::default();
    config.cluster.intake_routing.enabled = true;
    config.cluster.intake_routing.mode = crate::config::ClusterIntakeRoutingMode::Enforce;
    edit(&mut config.cluster.intake_routing);
    crate::config_live_reload::install(config);
}
fn active(channel: &str) -> impl FnOnce(&mut crate::config::ClusterIntakeRoutingConfig) {
    let channel = channel.to_owned();
    move |routing| {
        routing.input_fallback_enabled = true;
        routing.input_fallback_scope = InputFallbackScope::Pilot;
        routing.input_fallback_pilot_channel_ids = vec![channel];
        routing.input_fallback_pair_instance_ids = vec!["mac-a".into(), "mac-b".into()];
    }
}
fn entered(channel: &str) -> usize {
    super::ENTERED
        .lock()
        .unwrap()
        .iter()
        .filter(|c| *c == channel)
        .count()
}
fn shape(decision: &IntakeRouterDecision) -> String {
    match decision {
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            basis,
            ..
        } => format!("forwarded:{target_instance_id}:{basis:?}"),
        other => format!("{other:?}"),
    }
}
/// Every routing-owned column, so a path that reached the new code cannot hide behind the decision.
async fn routing_effects(pool: &PgPool) -> (i64, i64, i64, i64) {
    sqlx::query_as("SELECT (SELECT COUNT(*) FROM intake_outbox WHERE routing_state IS NOT NULL OR routing_plan IS NOT NULL OR routing_receipt_id IS NOT NULL),(SELECT COUNT(*) FROM intake_session_owners),(SELECT COUNT(*) FROM message_outbox WHERE source='routing_transition'),(SELECT COUNT(*) FROM intake_outbox WHERE replay_request_key IS NOT NULL)")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_off_runtime_never_enters_and_keeps_existing_placement_pg() {
    const CHANNEL: &str = "73369401";
    type Edit = Box<dyn FnOnce(&mut crate::config::ClusterIntakeRoutingConfig)>;
    let variants: [(&str, Edit); 5] = [
        ("default", Box::new(|_| {})),
        (
            "disabled-with-keys",
            Box::new(|r| {
                active(CHANNEL)(r);
                r.input_fallback_enabled = false;
            }),
        ),
        (
            "invalid-pair",
            Box::new(|r| {
                active(CHANNEL)(r);
                r.input_fallback_pair_instance_ids = vec!["mac-a".into(), "mac-a".into()];
            }),
        ),
        (
            "not-in-pilot",
            Box::new(|r| {
                active("other-channel")(r);
            }),
        ),
        ("pilot-control", Box::new(active(CHANNEL))),
    ];
    let mut baseline = None;
    for (name, edit) in variants {
        let _config = install(edit);
        let f = Fixture::new(CHANNEL).await;
        let before = entered(CHANNEL);
        let decision = try_route_intake(
            &f.pool,
            &ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL),
        )
        .await;
        let entries = entered(CHANNEL) - before;
        let effects = routing_effects(&f.pool).await;
        if name == "pilot-control" {
            assert_eq!(
                entries, 1,
                "the active control must reach the new path, or 0 proves nothing"
            );
            assert_eq!(
                (shape(&decision).as_str(), effects.1),
                ("forwarded:mac-a:NodeOverride", 1),
                "a managed same-target input is admitted under a PG owner"
            );
            f.finish().await;
            continue;
        } else {
            assert_eq!(entries, 0, "{name}: off must never enter availability/CAS");
            assert_eq!(
                effects,
                (0, 0, 0, 0),
                "{name}: off must not stage, own or notify"
            );
        }
        let observed = (shape(&decision), effects);
        match &baseline {
            None => baseline = Some(observed),
            Some(expected) => assert_eq!(
                &observed, expected,
                "{name}: placement must match the default configuration"
            ),
        }
        f.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_peer_and_return_commit_owner_admission_and_notice_together_pg() {
    const CHANNEL: &str = "72280700";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    let id = OwnerIdentity::new("claude", CHANNEL);
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW()-INTERVAL '10 minutes' WHERE instance_id='mac-a'")
        .execute(&f.pool).await.unwrap();
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let first = match try_route_intake(&f.pool, &ctx).await {
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            outbox_id,
            ..
        } => {
            assert_eq!(target_instance_id, "mac-b");
            outbox_id
        }
        other => panic!("{other:?}"),
    };
    let row: (String, String, Option<i64>, String) = sqlx::query_as("SELECT status,routing_state,owner_generation,target_instance_id FROM intake_outbox WHERE id=$1")
        .bind(first).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        row,
        ("pending".into(), "admitted".into(), Some(0), "mac-b".into())
    );
    let owner: (String, i64, Option<i64>) = sqlx::query_as("SELECT owner_instance_id,generation,routing_notice_outbox_id FROM intake_session_owners WHERE provider='claude' AND raw_channel_id=$1")
        .bind(CHANNEL).fetch_one(&f.pool).await.unwrap();
    assert_eq!((owner.0.as_str(), owner.1), ("mac-b", 0));
    let notice = owner
        .2
        .expect("the owner CAS must reserve its notice in the same transaction");
    let (reason, nonce): (String, String) = sqlx::query_as("SELECT m.reason_code,o.routing_notice->>'nonce' FROM message_outbox m JOIN intake_session_owners o ON o.routing_notice_outbox_id=m.id WHERE m.id=$1 AND m.source='routing_transition'")
        .bind(notice).fetch_one(&f.pool).await.unwrap();
    assert_eq!(reason, "routing.to_peer");

    let discord = crate::server::outbox_worker::origin_fixture::Discord::new().await;
    let registry = discord.registry().await;
    crate::server::outbox_worker::drain_message_outbox_once(
        &std::sync::Arc::new(f.pool.clone()),
        &registry,
        "fallback-route",
    )
    .await;
    let posts = discord.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].1["nonce"].as_str(), Some(nonce.as_str()));

    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW() WHERE instance_id='mac-a'")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(first)
        .execute(&f.pool)
        .await
        .unwrap();
    ctx.user_msg_id = "10000";
    ctx.user_text = "back home";
    let second = match try_route_intake(&f.pool, &ctx).await {
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            outbox_id,
            ..
        } => {
            assert_eq!(target_instance_id, "mac-a");
            outbox_id
        }
        other => panic!("{other:?}"),
    };
    let held: (String, bool) = sqlx::query_as(
        "SELECT routing_state,routing_plan IS NOT NULL FROM intake_outbox WHERE id=$1",
    )
    .bind(second)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        held,
        ("held".into(), true),
        "a live peer owner keeps the input in PG custody"
    );
    let request: (String, i64) = sqlx::query_as("SELECT routing_cleanup_state,(routing_cleanup->>'pending')::BIGINT FROM intake_session_owners WHERE provider='claude' AND generation=0")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(request, ("pending".into(), second));
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE generation=0")
        .execute(&f.pool).await.unwrap();
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    let returned: (String, i64, String) = sqlx::query_as("SELECT o.owner_instance_id,o.generation,m.content FROM intake_session_owners o JOIN message_outbox m ON m.id=o.routing_notice_outbox_id WHERE o.provider='claude' AND o.status='active'")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!((returned.0.as_str(), returned.1), ("mac-a", 1));
    assert!(returned.2.contains("연결이 돌아와"), "{}", returned.2);
    let admitted: String =
        sqlx::query_scalar("SELECT routing_state FROM intake_outbox WHERE id=$1")
            .bind(second)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(admitted, "admitted");
    let _ = id;
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_unadopted_live_session_keeps_existing_placement_pg() {
    const CHANNEL: &str = "73369403";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    sqlx::query("INSERT INTO sessions(session_key,provider,channel_id,instance_id,status) VALUES('claude/token/mac-b:remote-owner','claude',$1,'mac-b','idle')")
        .bind(CHANNEL).execute(&f.pool).await.unwrap();
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-a";
    let before = entered(CHANNEL);
    let _ = try_route_intake(&f.pool, &ctx).await;
    assert_eq!(entered(CHANNEL) - before, 1);
    let effects = routing_effects(&f.pool).await;
    assert_eq!(
        (effects.0, effects.1, effects.2),
        (0, 0, 0),
        "a remote session is never adopted or replaced from the gateway"
    );
    f.finish().await;
}

async fn offline(f: &Fixture, instance: &str) {
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW()-INTERVAL '10 minutes' WHERE instance_id=$1")
        .bind(instance).execute(&f.pool).await.unwrap();
}
async fn forwarded(f: &Fixture, ctx: &IntakeRouterContext<'_>) -> i64 {
    match try_route_intake(&f.pool, ctx).await {
        IntakeRouterDecision::Forwarded { outbox_id, .. } => outbox_id,
        other => panic!("{other:?}"),
    }
}
async fn notices(f: &Fixture) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source='routing_transition'")
        .fetch_one(&f.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_first_held_resumes_after_open_route_without_new_input_pg() {
    const CHANNEL: &str = "73369404";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    offline(&f, "mac-a").await;
    let open = crate::db::intake_outbox::insert_pending(
        &f.pool,
        &owner_record::output::tests::payload(CHANNEL, "7336940401", "mac-b"),
        1,
        None,
    )
    .await
    .unwrap();
    let bundle = f.bundle_ref(CHANNEL, "9999").await;
    let refs = [bundle.clone()];
    let sources = vec!["9998".to_string(), "9999".to_string()];
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    ctx.reply_context = Some("held reply context");
    ctx.replay_source_message_ids = Some(&sources);
    ctx.attachment_refs = &refs;
    let held = forwarded(&f, &ctx).await;
    let state: String = sqlx::query_scalar("SELECT routing_state FROM intake_outbox WHERE id=$1")
        .bind(held)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        state, "held",
        "an open route keeps the first switch in custody"
    );
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(open)
        .execute(&f.pool)
        .await
        .unwrap();
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    let row: (String, String, String, Option<String>, Option<Vec<String>>, serde_json::Value) = sqlx::query_as("SELECT routing_state,status,target_instance_id,reply_context,replay_source_message_ids,attachment_refs FROM intake_outbox WHERE id=$1")
        .bind(held).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2.as_str()),
        ("admitted", "pending", "mac-b"),
        "the stored plan must resume without another user input"
    );
    assert_eq!(row.3.as_deref(), Some("held reply context"));
    assert_eq!(row.4, Some(sources.clone()));
    assert_eq!(row.5, serde_json::json!([bundle]));
    assert_eq!(notices(&f).await, 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_replans_head_when_observation_changes_before_plan_pg() {
    const CHANNEL: &str = "73369405";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    offline(&f, "mac-a").await;
    *super::BEFORE_PLAN.lock().unwrap() =
        Some("UPDATE worker_nodes SET last_heartbeat_at=NOW() WHERE instance_id='mac-a'".into());
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let delivery = forwarded(&f, &ctx).await;
    let peer: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM intake_session_owners WHERE owner_instance_id='mac-b'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(peer, 0, "a rejected plan must not promote its stale target");
    assert_eq!(notices(&f).await, 0);
    let row: (String, String) = sqlx::query_as(
        "SELECT target_instance_id,COALESCE(routing_plan->>'target','') FROM intake_outbox WHERE id=$1",
    )
    .bind(delivery)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        ("mac-a".into(), "mac-a".into()),
        "the head is re-observed and planned for the recovered home"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_promotion_rechecks_stored_home_proof_pg() {
    const CHANNEL: &str = "73369406";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    offline(&f, "mac-a").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let first = forwarded(&f, &ctx).await;
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(first)
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW() WHERE instance_id='mac-a'")
        .execute(&f.pool)
        .await
        .unwrap();
    ctx.user_msg_id = "10000";
    ctx.user_text = "planned for home";
    let held = forwarded(&f, &ctx).await;
    sqlx::query("UPDATE agents SET default_execution_node_id='mac-b'")
        .execute(&f.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE generation=0")
        .execute(&f.pool).await.unwrap();
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    let owner: (String, i64) = sqlx::query_as("SELECT owner_instance_id,generation FROM intake_session_owners WHERE provider='claude' ORDER BY generation DESC LIMIT 1")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        owner,
        ("mac-b".into(), 0),
        "a changed agent home must not promote the plan's old target"
    );
    let row: (String, String) =
        sqlx::query_as("SELECT routing_state,user_text FROM intake_outbox WHERE id=$1")
            .bind(held)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(row, ("held".into(), "planned for home".into()));
    assert_eq!(notices(&f).await, 1, "only the first switch is announced");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_rejects_local_upload_before_custody_pg() {
    const CHANNEL: &str = "73369407";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    offline(&f, "mac-a").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    ctx.has_nonportable_uploads = true;
    let decision = try_route_intake(&f.pool, &ctx).await;
    assert!(
        matches!(
            &decision,
            IntakeRouterDecision::Blocked {
                reason: IntakeBlockedReason::NonPortableAttachmentRoutedTarget { target_instance_id }
            } if target_instance_id == "mac-b"
        ),
        "{decision:?}"
    );
    assert_eq!(routing_effects(&f.pool).await, (0, 0, 0, 0));
    f.finish().await;
}

async fn row(f: &Fixture, id: i64) -> (String, String, Option<i64>) {
    sqlx::query_as(
        "SELECT routing_state,target_instance_id,owner_generation FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&f.pool)
    .await
    .unwrap()
}
async fn done(f: &Fixture, id: i64) {
    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(id)
        .execute(&f.pool)
        .await
        .unwrap();
}
async fn open_route(f: &Fixture, channel: &str, message: &str) -> i64 {
    crate::db::intake_outbox::insert_pending(
        &f.pool,
        &owner_record::output::tests::payload(channel, message, "mac-b"),
        1,
        None,
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_queued_followers_resume_in_order_without_new_input_pg() {
    const CHANNEL: &str = "73369408";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    offline(&f, "mac-a").await;
    let open = open_route(&f, CHANNEL, "7336940801").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let first = forwarded(&f, &ctx).await;
    let bundle = f.bundle_ref(CHANNEL, "10001").await;
    let refs = [bundle.clone()];
    let sources = vec!["10000".to_string(), "10001".to_string()];
    ctx.user_msg_id = "10001";
    ctx.user_text = "queued follower";
    ctx.reply_context = Some("follower reply");
    ctx.replay_source_message_ids = Some(&sources);
    ctx.attachment_refs = &refs;
    let second = forwarded(&f, &ctx).await;
    let planned: Vec<(i64, Option<String>)> = sqlx::query_as("SELECT id,routing_plan->>'target' FROM intake_outbox WHERE routing_state='held' ORDER BY id")
        .fetch_all(&f.pool).await.unwrap();
    assert_eq!(
        planned,
        vec![
            (first, Some("mac-b".into())),
            (second, Some("mac-b".into()))
        ],
        "every held input carries its own plan"
    );
    done(&f, open).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    assert_eq!(row(&f, first).await.0, "admitted");
    assert_eq!(
        row(&f, second).await.0,
        "held",
        "the follower waits its turn"
    );
    done(&f, first).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    let follower: (String, String, Option<String>, Option<Vec<String>>, serde_json::Value) = sqlx::query_as("SELECT routing_state,target_instance_id,reply_context,replay_source_message_ids,attachment_refs FROM intake_outbox WHERE id=$1")
        .bind(second).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        (follower.0.as_str(), follower.1.as_str()),
        ("admitted", "mac-b"),
        "a queued follower resumes without another user input"
    );
    assert_eq!(follower.2.as_deref(), Some("follower reply"));
    assert_eq!(follower.3, Some(sources.clone()));
    assert_eq!(follower.4, serde_json::json!([bundle]));
    assert_eq!(notices(&f).await, 1);
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_cleanup_heads_never_promote_plan_less_custody_pg() {
    const CHANNEL: &str = "73369409";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    offline(&f, "mac-a").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let first = forwarded(&f, &ctx).await;
    done(&f, first).await;
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW() WHERE instance_id='mac-a'")
        .execute(&f.pool)
        .await
        .unwrap();
    ctx.user_msg_id = "10000";
    let second = forwarded(&f, &ctx).await;
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE generation=0")
        .execute(&f.pool).await.unwrap();
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    assert_eq!(
        row(&f, second).await,
        ("admitted".into(), "mac-a".into(), Some(1))
    );
    let payload = owner_record::output::tests::payload(CHANNEL, "7336940902", "mac-a");
    let bare = owner_record::custody::stage(
        &f.pool,
        owner_record::custody::StageRequest {
            payload: &payload,
            home_revision: 0,
            wait_generation: Some(1),
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    done(&f, second).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    assert_eq!(
        (row(&f, bare).await.0, planned_target(&f, bare).await),
        ("admitted".into(), Some("mac-a".into())),
        "custody without a saved plan is planned before cleanup history promotes it"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_replans_held_after_raw_home_revision_change_pg() {
    const CHANNEL: &str = "73369410";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    let id = OwnerIdentity::new("claude", CHANNEL);
    offline(&f, "mac-a").await;
    let open = open_route(&f, CHANNEL, "7336941001").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let held = forwarded(&f, &ctx).await;
    f.home(&id, Some("mac-b")).await;
    done(&f, open).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    let (state, plan_home): (String, Option<String>) =
        sqlx::query_as("SELECT routing_state,routing_plan->>'home' FROM intake_outbox WHERE id=$1")
            .bind(held)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), plan_home.as_deref()),
        ("admitted", Some("mac-b")),
        "a /node change re-plans held custody instead of leaving it waiting"
    );
    assert_eq!(
        notices(&f).await,
        0,
        "admission on the new home is not a fallback"
    );

    // Once fallback no longer places the channel, held input is released with a notice.
    done(&f, held).await;
    ctx.user_msg_id = "10000";
    let open = open_route(&f, CHANNEL, "7336941002").await;
    let queued = forwarded(&f, &ctx).await;
    reinstall(active("other-channel"));
    f.home(&id, Some("mac-a")).await;
    done(&f, open).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    let released: (String, String) = sqlx::query_as("SELECT d.routing_state,r.replay_disposition FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.id=$1")
        .bind(queued).fetch_one(&f.pool).await.unwrap();
    assert_eq!(released, ("superseded".into(), "withheld".into()));
    let notice: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox WHERE reason_code='routing.held_released'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(notice, 1, "a released input tells the channel once");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_same_target_keeps_pg_owner_over_stale_session_pg() {
    const CHANNEL: &str = "73369411";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    let id = OwnerIdentity::new("claude", CHANNEL);
    let mut tx = f.pool.begin().await.unwrap();
    owner_record::custody::lock(&mut tx, &id).await.unwrap();
    owner_record::acquire_owner_in_tx(&mut tx, &id, "mac-b", |_| true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO sessions(session_key,provider,channel_id,instance_id,status) VALUES('claude/token/mac-a:deferred-owner','claude',$1,'mac-a','idle')")
        .bind(CHANNEL).execute(&f.pool).await.unwrap();
    offline(&f, "mac-a").await;
    let first = open_route(&f, CHANNEL, "7336941101").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let decision = try_route_intake(&f.pool, &ctx).await;
    let next = match decision {
        IntakeRouterDecision::Forwarded {
            target_instance_id,
            outbox_id,
            ..
        } if target_instance_id == "mac-b" => outbox_id,
        other => panic!("the PG owner must keep the same-target input: {other:?}"),
    };
    assert_eq!(row(&f, next).await.0, "held");
    done(&f, first).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    assert_eq!(
        row(&f, next).await,
        ("admitted".into(), "mac-b".into(), Some(0))
    );
    let owners: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_session_owners")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(owners, 1, "same-owner admission keeps its generation");
    assert_eq!(notices(&f).await, 0, "same-owner input is not a transition");
    f.finish().await;
}

async fn planned_target(f: &Fixture, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT routing_plan->>'target' FROM intake_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(&f.pool)
        .await
        .unwrap()
}
async fn expire_cleanup(f: &Fixture, generation: i64) {
    sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()-INTERVAL '1 second' WHERE generation=$1")
        .bind(generation).execute(&f.pool).await.unwrap();
}
async fn acquire(f: &Fixture, id: &OwnerIdentity, instance: &str) {
    let mut tx = f.pool.begin().await.unwrap();
    owner_record::custody::lock(&mut tx, id).await.unwrap();
    owner_record::acquire_owner_in_tx(&mut tx, id, instance, |_| true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_switch_resumes_retired_pending_and_announces_once_pg() {
    for (channel, placed) in [("73369412", true), ("73369413", false)] {
        let _config = install(active(channel));
        let f = Fixture::new(channel).await;
        let id = OwnerIdentity::new("claude", channel);
        let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, channel);
        ctx.leader_instance_id = "mac-a";
        // R0 is unstarted on the old owner, either from existing placement or from fallback.
        let r0 = if placed {
            acquire(&f, &id, "mac-a").await;
            crate::db::intake_outbox::insert_pending(
                &f.pool,
                &owner_record::output::tests::payload(channel, "7336941201", "mac-a"),
                1,
                None,
            )
            .await
            .unwrap()
        } else {
            forwarded(&f, &ctx).await
        };
        let before: (String, Option<Vec<String>>) = sqlx::query_as(
            "SELECT user_text,replay_source_message_ids FROM intake_outbox WHERE id=$1",
        )
        .bind(r0)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        let base = notices(&f).await;
        offline(&f, "mac-a").await;
        ctx.leader_instance_id = "mac-b";
        ctx.user_msg_id = "10001";
        ctx.user_text = "switch input";
        let r1 = forwarded(&f, &ctx).await;
        expire_cleanup(&f, 0).await;
        cleanup::advance_pending(&f.pool, "claude").await.unwrap();
        assert_eq!(
            row(&f, r0).await,
            ("admitted".into(), "mac-b".into(), Some(1)),
            "{channel}: the retired input resumes first without another user input"
        );
        assert_eq!(row(&f, r1).await.0, "held");
        let after: (String, Option<Vec<String>>) = sqlx::query_as(
            "SELECT user_text,replay_source_message_ids FROM intake_outbox WHERE id=$1",
        )
        .bind(r0)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(
            after, before,
            "{channel}: body and sources survive retirement"
        );
        done(&f, r0).await;
        cleanup::advance_pending(&f.pool, "claude").await.unwrap();
        assert_eq!(
            row(&f, r1).await,
            ("admitted".into(), "mac-b".into(), Some(1))
        );
        assert_eq!(
            notices(&f).await - base,
            1,
            "{channel}: the move to mac-b is announced exactly once"
        );
        f.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_replan_rolls_back_refresh_when_observation_moves_pg() {
    const CHANNEL: &str = "73369414";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    let id = OwnerIdentity::new("claude", CHANNEL);
    offline(&f, "mac-a").await;
    let open = open_route(&f, CHANNEL, "7336941401").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let held = forwarded(&f, &ctx).await;
    f.home(&id, Some("mac-a")).await;
    *owner_record::plan::AFTER_REFRESH.lock().unwrap() =
        Some("UPDATE worker_nodes SET last_heartbeat_at=NOW() WHERE instance_id='mac-a'".into());
    done(&f, open).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    assert_eq!(row(&f, held).await.0, "held");
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    assert_eq!(
        (row(&f, held).await, planned_target(&f, held).await),
        (
            ("admitted".into(), "mac-a".into(), Some(0)),
            Some("mac-a".into())
        ),
        "a rejected re-plan stays stale, so the next tick plans the recovered home"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_cleanup_advance_plans_head_before_promotion_pg() {
    const CHANNEL: &str = "73369415";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    let id = OwnerIdentity::new("claude", CHANNEL);
    offline(&f, "mac-a").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-b";
    let first = forwarded(&f, &ctx).await;
    done(&f, first).await;
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW() WHERE instance_id='mac-a'")
        .execute(&f.pool)
        .await
        .unwrap();
    ctx.user_msg_id = "10000";
    let switch = forwarded(&f, &ctx).await;
    let receipt: i64 =
        sqlx::query_scalar("SELECT routing_receipt_id FROM intake_outbox WHERE id=$1")
            .bind(switch)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    owner_record::custody::lock(&mut tx, &id).await.unwrap();
    crate::db::replay_disposition::write::withhold_owner_loss_on_tx(&mut tx, receipt)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let payload = owner_record::output::tests::payload(CHANNEL, "7336941502", "mac-a");
    let bare = owner_record::custody::stage(
        &f.pool,
        owner_record::custody::StageRequest {
            payload: &payload,
            home_revision: 0,
            wait_generation: Some(0),
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    expire_cleanup(&f, 0).await;
    cleanup::advance_pending(&f.pool, "claude").await.unwrap();
    assert_eq!(
        (row(&f, bare).await, planned_target(&f, bare).await),
        (
            ("admitted".into(), "mac-a".into(), Some(1)),
            Some("mac-a".into())
        ),
        "the cleanup advance plans a plan-less head before it switches the owner"
    );
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_fallback_same_target_local_upload_runs_on_its_owner_pg() {
    const CHANNEL: &str = "73369416";
    let _config = install(active(CHANNEL));
    let f = Fixture::new(CHANNEL).await;
    let id = OwnerIdentity::new("claude", CHANNEL);
    acquire(&f, &id, "mac-a").await;
    let mut ctx = ctx_for_channel(IntakeRoutingMode::Enforce, CHANNEL);
    ctx.leader_instance_id = "mac-a";
    ctx.has_nonportable_uploads = true;
    let decision = try_route_intake(&f.pool, &ctx).await;
    assert!(
        matches!(
            &decision,
            IntakeRouterDecision::RanLocal {
                reason: RanLocalReason::NodeOverrideIsLeader
            }
        ),
        "a local file on its own owner runs locally: {decision:?}"
    );
    assert_eq!(
        routing_effects(&f.pool).await,
        (0, 1, 0, 0),
        "no custody, the same single owner, no notice"
    );
    f.finish().await;
}
