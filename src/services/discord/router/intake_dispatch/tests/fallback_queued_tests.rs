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
    routing.input_fallback_pilot_channel_ids = vec![CHANNEL.into()];
    routing.input_fallback_pair_instance_ids = vec!["mac-a".into(), "mac-b".into()];
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
