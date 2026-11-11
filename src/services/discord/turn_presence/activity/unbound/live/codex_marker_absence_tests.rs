use super::*;
use crate::services::codex::legacy_hint::evidence::{AbsenceStamp, HintPath};
use crate::services::discord::commands::control::codex_clear_relaunch_tests::settled_owner_observation;
use crate::services::discord::turn_presence::activity::reading_now;
use crate::services::turn_orchestrator::QueueIdentity;

#[tokio::test(flavor = "current_thread")]
async fn marker_only_legacy_reads_private_snapshot_instead_of_public_reason_pg() {
    let id = 69_187_601;
    let rig = super::super::review_tests::Rig::new(id, true, false).await;
    let mut config = (*crate::config_live_reload::current().unwrap()).clone();
    let agent = &mut config.agents[0];
    agent.provider = "codex".into();
    let channel = agent.channels.get("claude").unwrap().clone();
    agent.channels = crate::config::AgentChannels::new().with("codex", channel);
    crate::config_live_reload::install(config.clone());
    std::fs::write(
        crate::runtime_layout::config_file_path(rig.root.path()),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
    let name = ProviderKind::Codex.build_tmux_session_name(&format!("s1e-{id}"));
    let key = adk_session::build_namespaced_session_key(
        &rig.shared.token_hash,
        &ProviderKind::Codex,
        &name,
    );
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3, NULL)")
        .bind(&key).bind(&rig.shared.token_hash).bind(id.to_string())
        .execute(rig.shared.pg_pool.as_ref().unwrap()).await.unwrap();
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(id);
    let mut reading = settled_owner_observation(
        "marker-only private snapshot",
        || reading_now(&rig.shared, &ProviderKind::Codex, rig.channel),
        |reading| matches!(reading.observed.reason, "superseded" | "catching_up"),
    )
    .await;
    assert_eq!(
        reading.observed,
        observed(Activity::Unknown, "session_identity_unresolved")
    );
    let snapshot = reading.unbound.as_ref().unwrap().snapshot.as_ref().unwrap();
    let HostedLookup::Found(row) = &snapshot.exact else {
        panic!("missing fixture Legacy row")
    };
    assert_eq!(row.record, HostedRecord::Legacy);
    let paths = [
        marker_path(&name, tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT),
        std::path::PathBuf::from(tmux_common::legacy_tmp_session_path(
            &name,
            tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
        )),
    ];
    std::fs::create_dir_all(paths[0].parent().unwrap()).unwrap();
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: rig.shared.token_hash.clone(),
        channel_id: id.to_string(),
        logical_key: name,
        owner_node: "test-node".into(),
        runtime_root: rig.root.path().display().to_string(),
    };
    let evidence = AbsenceStamp {
        owner: owner.clone(),
        expected: crate::services::turn_orchestrator::MarkerRowExpectation::Legacy {
            owner: (&owner).into(),
            row_id: row.session_id(),
            exact_session_key: key,
        },
        front: QueueIdentity {
            primary: id,
            queued_generation: 1,
            sources: vec![],
        },
        epoch: "epoch-1".into(),
        paths: [
            HintPath::observe(&paths[0]).unwrap(),
            HintPath::observe(&paths[1]).unwrap(),
        ],
        nonce: Ok(None),
        source: Ok(None),
    };
    assert_eq!(
        reading.marker_only_legacy(&evidence, &evidence),
        Some(LegacyMarkerOnly)
    );
    let mut wrong = evidence.clone();
    let crate::services::turn_orchestrator::MarkerRowExpectation::Legacy { row_id, .. } =
        &mut wrong.expected
    else {
        unreachable!()
    };
    *row_id += 1;
    assert!(reading.marker_only_legacy(&wrong, &wrong).is_none());
    let snapshot = reading.unbound.as_mut().unwrap().snapshot.as_mut().unwrap();
    snapshot.exact = HostedLookup::Missing;
    assert!(matches!(
        snapshot.scope(&rig.shared).result,
        Scope::Hold("exact_canonical_mismatch")
    ));
    assert_eq!(
        reading.observed,
        observed(Activity::Unknown, "session_identity_unresolved")
    );
    assert!(reading.marker_only_legacy(&evidence, &evidence).is_none());
}
