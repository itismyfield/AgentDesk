use super::*;
use crate::services::cluster::attachment_transfer::uploads::Upload;
use crate::services::cluster::intake_router_hook::owner_record::snapshot::tests::Fixture;

const CHANNEL: &str = "73369501";

struct Live {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}
/// Input fallback active for one pilot channel, with the pair the snapshot fixture registers.
fn fallback_live() -> Live {
    fallback_live_for(CHANNEL, ["mac-a".into(), "mac-b".into()])
}

fn fallback_live_for(channel: &str, pair: [String; 2]) -> Live {
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
    let routing = &mut config.cluster.intake_routing;
    routing.enabled = true;
    routing.mode = crate::config::ClusterIntakeRoutingMode::Enforce;
    routing.input_fallback_enabled = true;
    routing.input_fallback_pilot_channel_ids = vec![channel.into()];
    routing.input_fallback_pair_instance_ids = pair.into();
    crate::config_live_reload::install(config);
    Live {
        _guards: guards,
        _live: live,
        _root: root,
    }
}
async fn admit(
    deps: &IntakeDeps<'_>,
    channel: ChannelId,
    intervention: &Intervention,
) -> QueuedAdmissionDisposition {
    admit_queued_intake(
        deps,
        ProviderKind::Claude,
        channel,
        intervention,
        intervention.author_id,
        "fallback-owner".to_string(),
        false,
        false,
        "fallback_attachment_test",
        None,
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn queued_local_upload_on_fallback_switch_is_rejected_without_custody_pg() {
    let _live = fallback_live();
    let f = Fixture::new(CHANNEL).await;
    sqlx::query("UPDATE worker_nodes SET last_heartbeat_at=NOW()-INTERVAL '10 minutes' WHERE instance_id='mac-a'")
        .execute(&f.pool).await.unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(f.pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let channel = ChannelId::new(CHANNEL.parse().unwrap());
    let local = queued_intervention(
        73_369_511,
        vec![
            "/private/tmp/fallback-local-attachment.txt"
                .to_string()
                .into(),
        ],
    );
    assert!(matches!(
        admit(&deps, channel, &local).await,
        QueuedAdmissionDisposition::RejectedNonPortableAttachment
    ));
    let effects: (i64, i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM intake_outbox),(SELECT COUNT(*) FROM intake_session_owners),(SELECT COUNT(*) FROM message_outbox WHERE source='routing_transition')")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        effects,
        (0, 0, 0),
        "a local path cannot enter PG custody, an owner switch or a notice"
    );
    assert!(
        shared
            .mailbox(channel)
            .snapshot()
            .await
            .intervention_queue
            .is_empty()
    );

    let reference = f.bundle_ref(CHANNEL, "73369512").await;
    let portable = queued_intervention(73_369_512, vec![Upload::Bundle(reference.clone())]);
    let QueuedAdmissionDisposition::Admitted(admitted) = admit(&deps, channel, &portable).await
    else {
        panic!("a portable bundle takes the fallback switch");
    };
    super::super::finish_admitted_queued_intake(&deps, admitted)
        .await
        .unwrap();
    let routed: (String, String, serde_json::Value) = sqlx::query_as("SELECT routing_state,target_instance_id,attachment_refs FROM intake_outbox WHERE routing_state IS NOT NULL AND user_msg_id='73369512'")
        .fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        routed,
        (
            "admitted".into(),
            "mac-b".into(),
            serde_json::json!([reference])
        )
    );
    f.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_local_upload_waits_for_held_only_input_through_promotion_pg() {
    use crate::services::cluster::intake_router_hook::owner_record::{
        OwnerIdentity, cleanup, custody, output, plan,
    };

    const LOCAL: &str = "73369503";
    let gateway =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let live = fallback_live_for(LOCAL, [gateway.clone(), "mac-b".into()]);
    let f = Fixture::new(LOCAL).await;
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,capabilities) SELECT $1,status,NOW(),capabilities FROM worker_nodes WHERE instance_id='mac-a'")
        .bind(&gateway).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE agents SET default_execution_node_id=$1")
        .bind(&gateway)
        .execute(&f.pool)
        .await
        .unwrap();
    let id = OwnerIdentity::new("claude", LOCAL);
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, &id).await.unwrap();
    crate::services::cluster::intake_router_hook::owner_record::acquire_owner_in_tx(
        &mut tx,
        &id,
        &gateway,
        |_| true,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let earlier = custody::stage(
        &f.pool,
        custody::StageRequest {
            payload: &output::tests::payload(LOCAL, "73369530", &gateway),
            home_revision: 0,
            wait_generation: Some(0),
            grace_seconds: 0,
        },
    )
    .await
    .unwrap();
    let mut context = f.context();
    context.pair = [gateway.clone(), "mac-b".into()];
    context.gateway = Some(gateway.clone());
    let observation = f.observe(&id, &context).await;
    let mut tx = f.pool.begin().await.unwrap();
    plan::save_on_tx(&mut tx, &id, earlier, &observation)
        .await
        .unwrap()
        .expect("R0 has a plan for periodic promotion");
    tx.commit().await.unwrap();
    let held: (String, String) =
        sqlx::query_as("SELECT status,routing_state FROM intake_outbox WHERE id=$1")
            .bind(earlier)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(held, ("unknown".into(), "held".into()));
    let open = format!(
        "SELECT COUNT(*) FROM intake_outbox WHERE status IN ({})",
        crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL
    );
    let open_count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(open))
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(open_count, 0, "R0 is held with no open route to mask it");

    let file = live._root.path().join("held-only-attachment.txt");
    let bytes = b"local bytes wait for held input";
    std::fs::write(&file, bytes).unwrap();
    let mut local = queued_intervention(
        73_369_531,
        vec![Upload::Local(file.to_string_lossy().into_owned())],
    );
    local.source_message_ids = vec![MessageId::new(73_369_529), MessageId::new(73_369_531)];
    local.reply_context = Some("held-only reply".into());
    local.has_reply_boundary = true;
    let assert_original = |item: &Intervention| {
        assert_eq!(
            (
                item.message_id,
                &item.text,
                &item.source_message_ids,
                &item.pending_uploads,
                &item.reply_context,
                item.has_reply_boundary,
            ),
            (
                local.message_id,
                &local.text,
                &local.source_message_ids,
                &local.pending_uploads,
                &local.reply_context,
                local.has_reply_boundary,
            ),
            "R1 keeps its original identity, body, uploads and reply context"
        );
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
    };
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(f.pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let channel = ChannelId::new(LOCAL.parse().unwrap());
    let mailbox = shared.mailbox(channel);
    let persistence = crate::services::discord::queue_persistence_context(
        &shared,
        &ProviderKind::Claude,
        channel,
    );
    mailbox
        .replace_queue(vec![local.clone()], persistence.clone())
        .await;
    for phase in ["held only", "promoted but unfinished"] {
        let dequeued = mailbox.take_next_soft(persistence.clone()).await;
        let item = dequeued
            .intervention
            .expect("R1 is retried from the mailbox");
        assert_original(&item);
        let admission = admit_queued_intake(
            &deps,
            ProviderKind::Claude,
            channel,
            &item,
            item.author_id,
            "fallback-owner".into(),
            false,
            false,
            "fallback_attachment_test",
            dequeued.dispatch_lease,
        )
        .await;
        assert!(
            matches!(admission, QueuedAdmissionDisposition::Deferred),
            "R1 must defer without local admission or permanent rejection while R0 is {phase}"
        );
        let queue = mailbox.snapshot().await.intervention_queue;
        assert_eq!(queue.len(), 1, "R1 is restored exactly once while {phase}");
        assert_original(&queue[0]);
        if phase == "held only" {
            cleanup::advance_pending(&f.pool, "claude").await.unwrap();
            let promoted: (String, String, String) = sqlx::query_as(
                "SELECT status,routing_state,target_instance_id FROM intake_outbox WHERE id=$1",
            )
            .bind(earlier)
            .fetch_one(&f.pool)
            .await
            .unwrap();
            assert_eq!(
                promoted,
                ("pending".into(), "admitted".into(), gateway.clone())
            );
        }
    }

    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(earlier)
        .execute(&f.pool)
        .await
        .unwrap();
    let dequeued = mailbox.take_next_soft(persistence.clone()).await;
    let item = dequeued.intervention.expect("R1 survived both deferrals");
    assert_original(&item);
    let QueuedAdmissionDisposition::Admitted(admitted) = admit_queued_intake(
        &deps,
        ProviderKind::Claude,
        channel,
        &item,
        item.author_id,
        "fallback-owner".into(),
        false,
        false,
        "fallback_attachment_test",
        dequeued.dispatch_lease,
    )
    .await
    else {
        panic!("R1 is admitted after R0 finishes");
    };
    assert_eq!(
        admitted.local_uploads().expect("R1 is admitted locally"),
        &local.pending_uploads
    );
    assert!(mailbox.snapshot().await.intervention_queue.is_empty());
    assert!(
        mailbox
            .take_next_soft(persistence)
            .await
            .intervention
            .is_none()
    );
    let r1_routes: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE user_msg_id='73369531'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(r1_routes, 0, "R1 never enters remote custody");
    f.finish().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_local_upload_waits_for_earlier_input_then_runs_on_its_owner_pg() {
    const LOCAL: &str = "73369502";
    let gateway =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    let _live = crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    );
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.yaml");
    std::fs::write(&path, "server: {}\n").unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _guards = [
        set("AGENTDESK_ROOT_DIR", root.path()),
        set("AGENTDESK_CONFIG", &path),
    ];
    let mut config = crate::config::Config::default();
    let routing = &mut config.cluster.intake_routing;
    routing.enabled = true;
    routing.mode = crate::config::ClusterIntakeRoutingMode::Enforce;
    routing.input_fallback_enabled = true;
    routing.input_fallback_pilot_channel_ids = vec![LOCAL.into()];
    routing.input_fallback_pair_instance_ids = vec![gateway.clone(), "mac-b".into()];
    crate::config_live_reload::install(config);
    let f = Fixture::new(LOCAL).await;
    // This process is the channel's home and PG owner.
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,last_heartbeat_at,capabilities) SELECT $1,status,NOW(),capabilities FROM worker_nodes WHERE instance_id='mac-a'")
        .bind(&gateway).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE agents SET default_execution_node_id=$1")
        .bind(&gateway)
        .execute(&f.pool)
        .await
        .unwrap();
    let id = crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new(
        "claude", LOCAL,
    );
    let mut tx = f.pool.begin().await.unwrap();
    crate::services::cluster::intake_router_hook::owner_record::custody::lock(&mut tx, &id)
        .await
        .unwrap();
    crate::services::cluster::intake_router_hook::owner_record::acquire_owner_in_tx(
        &mut tx,
        &id,
        &gateway,
        |_| true,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let earlier = crate::db::intake_outbox::insert_pending(
        &f.pool,
        &crate::services::cluster::intake_router_hook::owner_record::output::tests::payload(
            LOCAL, "73369520", &gateway,
        ),
        1,
        None,
    )
    .await
    .unwrap();

    let file = root.path().join("local-attachment.txt");
    std::fs::write(&file, b"local bytes stay on this Mac").unwrap();
    let mut local = queued_intervention(
        73_369_521,
        vec![Upload::Local(file.to_string_lossy().into_owned())],
    );
    local.source_message_ids = vec![MessageId::new(73_369_519), MessageId::new(73_369_521)];
    local.reply_context = Some("local reply".into());
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(f.pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let channel = ChannelId::new(LOCAL.parse().unwrap());
    let persistence = crate::services::discord::queue_persistence_context(
        &shared,
        &ProviderKind::Claude,
        channel,
    );
    shared
        .mailbox(channel)
        .replace_queue(vec![local.clone()], persistence.clone())
        .await;
    let dequeued = shared
        .mailbox(channel)
        .take_next_soft(persistence.clone())
        .await;
    let (item, lease) = (dequeued.intervention.unwrap(), dequeued.dispatch_lease);
    let first = admit_queued_intake(
        &deps,
        ProviderKind::Claude,
        channel,
        &item,
        item.author_id,
        "fallback-owner".to_string(),
        false,
        false,
        "fallback_attachment_test",
        lease,
    )
    .await;
    assert!(
        matches!(first, QueuedAdmissionDisposition::Deferred),
        "the earlier pending input runs first"
    );
    let queue = shared.mailbox(channel).snapshot().await.intervention_queue;
    assert_eq!(queue.len(), 1);
    assert_eq!(
        (
            &queue[0].pending_uploads,
            &queue[0].source_message_ids,
            queue[0].reply_context.as_deref()
        ),
        (
            &local.pending_uploads,
            &local.source_message_ids,
            Some("local reply")
        ),
        "the deferred original is restored whole"
    );

    sqlx::query("UPDATE intake_outbox SET status='done' WHERE id=$1")
        .bind(earlier)
        .execute(&f.pool)
        .await
        .unwrap();
    let dequeued = shared.mailbox(channel).take_next_soft(persistence).await;
    let (item, lease) = (dequeued.intervention.unwrap(), dequeued.dispatch_lease);
    let QueuedAdmissionDisposition::Admitted(admitted) = admit_queued_intake(
        &deps,
        ProviderKind::Claude,
        channel,
        &item,
        item.author_id,
        "fallback-owner".to_string(),
        false,
        false,
        "fallback_attachment_test",
        lease,
    )
    .await
    else {
        panic!("after the earlier input the local upload runs here");
    };
    let uploads = admitted.local_uploads().expect("runs on its own Mac");
    let [Upload::Local(delivered)] = uploads.as_slice() else {
        panic!("{uploads:?}");
    };
    assert_eq!(
        std::fs::read(delivered).unwrap(),
        b"local bytes stay on this Mac"
    );
    let owners: Vec<(String, i64)> =
        sqlx::query_as("SELECT owner_instance_id,generation FROM intake_session_owners")
            .fetch_all(&f.pool)
            .await
            .unwrap();
    assert_eq!(
        owners,
        vec![(gateway.clone(), 0)],
        "same owner and generation"
    );
    let notices: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source='routing_transition'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(notices, 0);
    f.finish().await;
}
