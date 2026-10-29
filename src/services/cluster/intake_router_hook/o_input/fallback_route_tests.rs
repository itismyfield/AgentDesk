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
    let mut config = crate::config::Config::default();
    config.cluster.intake_routing.enabled = true;
    config.cluster.intake_routing.mode = crate::config::ClusterIntakeRoutingMode::Enforce;
    edit(&mut config.cluster.intake_routing);
    crate::config_live_reload::install(config);
    Config {
        _guards: guards,
        _live: live,
        _root: root,
    }
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
