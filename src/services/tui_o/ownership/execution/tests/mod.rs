use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::intake_router_hook::owner_record::{custody, output};
use crate::services::tui_o::{cutover, ownership::GatewayOwnership};

pub(crate) fn mutant(mode: &str) -> bool {
    std::env::var("ADK_PC_B_MUTANT").as_deref() == Ok(mode)
}

pub(crate) async fn admission(pool: &PgPool, provider: &str, channel: u64, target: &str) -> i64 {
    sqlx::query("INSERT INTO agents(id,name,provider) VALUES('custody-agent','owner fixture',$1) ON CONFLICT(id) DO NOTHING")
        .bind(provider).execute(pool).await.unwrap();
    let mut payload = output::tests::payload(&channel.to_string(), "90100", "gateway");
    payload.provider = provider.into();
    let id = custody::stage(
        pool,
        custody::StageRequest {
            payload: &payload,
            home_revision: 0,
            wait_generation: None,
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    assert!(
        custody::promote(pool, id, target, None, custody::SwitchReadiness::Wait)
            .await
            .unwrap()
    );
    id
}

#[tokio::test]
async fn owner_execution_symmetric_pg_permit_ignores_gateway_home_and_preserves_pending() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_execution_symmetric_pg_permit_ignores_gateway_home_and_preserves_pending"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, (provider, kind)) in [
        ("claude", RuntimeHandoffKind::ClaudeTui),
        ("codex", RuntimeHandoffKind::CodexTui),
    ]
    .into_iter()
    .enumerate()
    {
        for (direction, (local, preferred)) in [("mac-a", "mac-b"), ("mac-b", "mac-a")]
            .into_iter()
            .enumerate()
        {
            let channel = 73368010 + (index * 10 + direction) as u64;
            admission(&pool, provider, channel, local).await;
            let _channels = cutover::test_override::force_foreign(&[(channel, kind)], preferred);
            let snapshot = cutover::boot_snapshot().unwrap();
            let permit = read(&pool, provider, channel, local, || Ok(false))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(permit.generation, 0);
            assert_eq!(permit.session_ref, SessionRef::Fresh);
            assert_eq!(
                permit.gate().current(),
                GatewayOwnership::ExecutionOwned {
                    epoch: 1,
                    generation: 0
                }
            );
            assert!(!matches!(
                permit.gate().current(),
                GatewayOwnership::Owned { .. }
            ));
            snapshot.install_execution(channel, Some(Ok(permit)));
            let boot = cutover::boot_ownership();
            assert!(
                boot.iter()
                    .any(|(id, _, candidate)| *id == channel && candidate.is_some()),
                "PG execution owner must host on either node independently of gateway preference"
            );
            let _ = cutover::intake_route::route_for_placement(provider, channel);
            assert_ne!(
                snapshot.output_candidate(channel).unwrap().peek(),
                crate::services::tui_o::channel_policy::Adoption::Released,
                "managed pending must never fall back to Legacy"
            );
        }
    }
    pool.close().await;
}

#[tokio::test]
async fn owner_execution_unknown_and_foreign_close_restore_without_legacy_fallback_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_execution_unknown_and_foreign_close_restore_without_legacy_fallback_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73368090;
    admission(&pool, "claude", channel, "mac-a").await;
    let _channels =
        cutover::test_override::force_channels(&[(channel, RuntimeHandoffKind::ClaudeTui)]);
    let snapshot = cutover::boot_snapshot().unwrap();
    let permit = read(&pool, "claude", channel, "mac-a", || Ok(false))
        .await
        .unwrap()
        .unwrap();
    let old = permit.gate();
    snapshot.install_execution(channel, Some(Ok(permit)));
    let foreign = read(&pool, "claude", channel, "mac-b", || {
        panic!("foreign owner must not inspect local session")
    })
    .await
    .unwrap_err();
    snapshot.install_execution(channel, Some(Err(foreign)));
    assert!(old.admit(|_| ()).is_none());
    assert!(
        cutover::boot_ownership()
            .iter()
            .all(|(_, _, candidate)| candidate.is_none()),
        "foreign PG owner must not restore a local actor"
    );
    assert!(matches!(
        cutover::intake_route::route_for_placement("claude", channel),
        cutover::intake_route::IntakeRoute::Hold(_)
    ));
    pool.close().await;
    let unknown = read(&pool, "claude", channel, "mac-a", || Ok(false))
        .await
        .unwrap_err();
    snapshot.install_execution(channel, Some(Err(unknown)));
    assert!(
        cutover::boot_ownership()
            .iter()
            .all(|(_, _, candidate)| candidate.is_none()),
        "PG error must not restore old gateway ownership"
    );
}

#[tokio::test]
async fn owner_execution_legacy_live_session_is_not_adopted_by_current_generation_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    admission(&pool, "claude", 73368091, "mac-a").await;
    let held = read(&pool, "claude", 73368091, "mac-a", || Ok(true))
        .await
        .unwrap_err();
    assert!(held.contains("adoption"));
    assert!(
        read(&pool, "claude", 73368092, "mac-a", || panic!(
            "unmanaged is untouched"
        ))
        .await
        .unwrap()
        .is_none()
    );
    pool.close().await;
}

#[tokio::test]
async fn owner_execution_bound_native_requires_original_local_incarnation_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_execution_bound_native_requires_original_local_incarnation_pg"
    )) {
        return;
    }
    use crate::db::intake_outbox as outbox;
    use crate::services::cluster::intake_router_hook::owner_record::input_start;
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73368093;
    let delivery = admission(&pool, "claude", channel, "mac-a").await;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let owner = "owner-execution-native-fixture";
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let context = BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.clone(),
        tmux_session: owner.into(),
        channel_id: Some(channel),
        owner_runtime_root: root.clone(),
        host: Some("mac-a".into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
    };
    let _prepared = PreparedIncarnation::create(context).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(owner, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(&marker, &nonce).unwrap();
    let binding = serde_json::json!({"host":"tmux","endpoint":"mac-a","owner":owner,"root":root,"execution_nonce":nonce});
    let claimed =
        outbox::claim_pending_for_target_except(&pool, "mac-a", "claude", "owner-token", &[])
            .await
            .unwrap()
            .unwrap();
    assert_eq!(claimed.id, delivery);
    assert!(
        outbox::mark_accepted(&pool, delivery, "owner-token")
            .await
            .unwrap()
    );
    assert!(
        outbox::mark_spawned(&pool, delivery, "owner-token")
            .await
            .unwrap()
    );
    let start = input_start::prepare(
        &pool,
        input_start::StartRequest {
            delivery_id: delivery,
            executor: "mac-a",
            claim_owner: "owner-token",
            incarnation: "owner-process",
            frame: "frame",
            native_binding: &binding,
        },
    )
    .await
    .unwrap();
    start.submit("frame", &binding, |_| async {}).await.unwrap();
    let permit = read(&pool, "claude", channel, "mac-a", || {
        panic!("original binding already determines local session")
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(permit.session_ref, SessionRef::Bound(binding));
    std::fs::write(&marker, uuid::Uuid::new_v4().simple().to_string()).unwrap();
    assert!(
        read(&pool, "claude", channel, "mac-a", || Ok(false))
            .await
            .unwrap_err()
            .contains("incarnation"),
        "restarting a native session must not adopt the old original binding"
    );
    pool.close().await;
}

#[tokio::test]
async fn owner_execution_gate_close_never_holds_the_snapshot_read_lock_pg() {
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_execution_gate_close_never_holds_the_snapshot_read_lock_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73368094;
    admission(&pool, "claude", channel, "mac-a").await;
    let _channels =
        cutover::test_override::force_candidates(&[(channel, RuntimeHandoffKind::ClaudeTui)]);
    let snapshot = cutover::boot_snapshot().unwrap();
    let permit = read(&pool, "claude", channel, "mac-a", || Ok(false))
        .await
        .unwrap()
        .unwrap();
    let gate = permit.gate();
    snapshot.install_execution(channel, Some(Ok(permit)));
    let again = read(&pool, "claude", channel, "mac-a", || Ok(false))
        .await
        .unwrap()
        .unwrap();
    snapshot.install_execution(channel, Some(Ok(again)));
    assert!(
        Arc::ptr_eq(
            &gate,
            &snapshot.execution(channel).unwrap().permit.unwrap().gate()
        ),
        "same owner generation and local session must keep the gate"
    );
    let (entered, admitted) = std::sync::mpsc::channel();
    let (release, waiting) = std::sync::mpsc::channel();
    let admitting = std::thread::spawn(move || {
        gate.admit(|_| {
            entered.send(()).unwrap();
            waiting.recv().unwrap();
        })
    });
    admitted.recv().unwrap();
    let changing = snapshot.clone();
    let installer = std::thread::spawn(move || {
        changing.install_execution(channel, Some(Err("revoked".into())))
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    let (read_back, read_done) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let _ = snapshot.execution(channel);
        read_back.send(()).unwrap();
    });
    let read_result = read_done.recv_timeout(std::time::Duration::from_secs(1));
    release.send(()).unwrap();
    admitting.join().unwrap();
    installer.join().unwrap();
    reader.join().unwrap();
    assert!(
        read_result.is_ok(),
        "snapshot readers must progress while closing a busy execution gate"
    );
    pool.close().await;
}

/// The same execution and its first binding keep the late candidate with its fixed boundary; a
/// different execution closes the old gate and seeds a candidate whose boundary is not fixed.
#[tokio::test]
async fn owner_execution_swap_seeds_a_late_candidate_with_an_unfixed_boundary_pg() {
    use crate::services::tui_o::channel_policy::{Adoption, BootChannels, LateBoundary};
    if !cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::owner_execution_swap_seeds_a_late_candidate_with_an_unfixed_boundary_pg"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let channel = 73368095;
    admission(&pool, "claude", channel, "mac-a").await;
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {}, "agents": [{"id": "w", "name": "W",
            "channels": {"claude": {"id": channel.to_string(), "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [channel], "late_adopt": true}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config).unwrap();
    let states = std::collections::BTreeMap::from([(channel, Adoption::Pending)]);
    let seeded = boot.seeded(&config, |_, _| Ok(states)).unwrap();
    let _channels = cutover::test_override::force_boot(seeded);
    let snapshot = cutover::boot_snapshot().unwrap();
    let read_now = || read(&pool, "claude", channel, "mac-a", || Ok(false));
    let permit = read_now().await.unwrap().unwrap();
    let (old_gate, generation) = (permit.gate(), permit.generation);
    let like = |generation: i64, session_ref: SessionRef| OwnerPermit {
        pool: pool.clone(),
        identity: permit.identity.clone(),
        executor: permit.executor.clone(),
        generation,
        session_ref,
        gate: Arc::new(super::super::OwnershipGate::for_execution(generation)),
    };
    let bound = like(
        generation,
        SessionRef::Bound(serde_json::json!({"host": "tmux"})),
    );
    let swapped = like(generation + 1, SessionRef::Fresh);
    snapshot.install_execution(channel, Some(Ok(Arc::clone(&permit))));
    let first = snapshot.output_candidate(channel).unwrap();
    assert!(first.late_adopt_origin());
    let fixed = LateBoundary {
        baseline: 3,
        takeover_after: 3,
        sends: (1, 1),
    };
    assert_eq!(first.fix_late_boundary(|| fixed), fixed);

    snapshot.install_execution(channel, Some(Ok(read_now().await.unwrap().unwrap())));
    snapshot.install_execution(channel, Some(Ok(Arc::new(bound))));
    let kept = snapshot.output_candidate(channel).unwrap();
    assert_eq!(
        kept.late_boundary(),
        Some(fixed),
        "same execution, then its first binding"
    );
    let gate = snapshot.execution(channel).unwrap().permit.unwrap().gate();
    assert!(Arc::ptr_eq(&old_gate, &gate));

    snapshot.install_execution(channel, Some(Ok(Arc::new(swapped))));
    assert_eq!(
        old_gate.current(),
        GatewayOwnership::Lost,
        "the old gate closes"
    );
    let next = snapshot.output_candidate(channel).unwrap();
    assert!(next.late_adopt_origin());
    assert_eq!(
        next.late_boundary(),
        None,
        "the next host's first read fixes its own"
    );
    let refixed = LateBoundary {
        baseline: 5,
        takeover_after: 5,
        sends: (2, 2),
    };
    assert_eq!(next.fix_late_boundary(|| refixed), refixed);
    assert_eq!(first.late_boundary(), Some(fixed));
    pool.close().await;
}
