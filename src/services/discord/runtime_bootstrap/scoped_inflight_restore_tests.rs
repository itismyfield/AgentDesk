use super::*;
use crate::db::o_channel_homes::{ChannelHome, HomeState};
use crate::services::agent_protocol::NativeTerminalKind;
use crate::services::cluster::{channel_home, channel_home_port::scoped_restore::*};

use crate::services::discord::recovery_engine::{install_inflight_scoped, resume_inflight_scoped};

pub(crate) fn home(channel: u64, state: HomeState) -> ChannelHome {
    ChannelHome {
        channel_id: channel.to_string(),
        provider: "claude".into(),
        state,
        holder: (state != HomeState::Released).then(|| "mini".into()),
        target: (state != HomeState::Worker).then(|| {
            if state == HomeState::Released {
                "mini"
            } else {
                "gateway"
            }
            .into()
        }),
        epoch: 1,
        renewed_at: Some(chrono::Utc::now()),
        updated_at: chrono::Utc::now(),
        detail: None,
    }
}
fn row(channel: u64, kind: Option<NativeTerminalKind>) -> inflight::InflightTurnState {
    let provider = ProviderKind::Claude;
    let name = provider.build_tmux_session_name(&format!("scoped-b2-{channel}"));
    let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let mut state = inflight::InflightTurnState::new(
        provider,
        channel,
        None,
        7,
        channel + 1,
        channel + 2,
        "original request".into(),
        None,
        Some(name),
        None,
        None,
        0,
    );
    state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    state.turn_nonce = Some(format!("nonce-{channel}"));
    state.born_generation = 0;
    state.tui_terminal_kind = kind;
    state.full_response = "captured completion".into();
    state
}
async fn fixture() -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
    Arc<SharedData>,
) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared = crate::services::discord::host_teardown_gate::test_support::shared_on(&pool).await;
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    (db, pool, shared)
}
async fn seed_home(pool: &sqlx::PgPool, home: &ChannelHome) {
    sqlx::query(
        "INSERT INTO o_channel_homes (channel_id,provider,state,holder,target,epoch,renewed_at)
        VALUES ($1,$2,$3,$4,$5,$6,NOW())",
    )
    .bind(&home.channel_id)
    .bind(&home.provider)
    .bind(home.state.as_str())
    .bind(&home.holder)
    .bind(&home.target)
    .bind(home.epoch)
    .execute(pool)
    .await
    .unwrap();
}
fn durable(state: &inflight::InflightTurnState) -> std::path::PathBuf {
    inflight::save_inflight_state_create_new(state).unwrap();
    inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        state.channel_id,
    )
}
fn complete(attempt: &mut super::ScopedRestoreAttempt) -> RestoreWitness {
    attempt.acknowledge(InstallStage::Inflight).unwrap();
    assert_eq!(attempt.context.generation, 1);
    attempt.acknowledge(InstallStage::Marker).unwrap();
    attempt.acknowledge(InstallStage::Placeholder).unwrap();
    attempt.finish().unwrap()
}

#[tokio::test]
async fn s3act_b2_install_is_effect_free_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = fixture().await;
    for (n, home_state, kind) in [
        (0, HomeState::Worker, Some(NativeTerminalKind::Aborted)),
        (1, HomeState::Released, Some(NativeTerminalKind::Completed)),
        (2, HomeState::Worker, Some(NativeTerminalKind::Aborted)),
        (3, HomeState::Worker, None),
        (4, HomeState::Worker, None),
        (5, HomeState::Released, None),
    ] {
        let channel = 5340201 + n;
        let home = home(channel, home_state);
        seed_home(&pool, &home).await;
        let gate = channel_home::register_for_test(channel, None);
        assert_eq!(gate.ownership(), channel_home::HomeOwnership::Lost);
        let mut original = row(channel, kind);
        if n == 2 {
            original.replay_hold_reasons = vec!["canonical debt retained".into()];
            original.restart_generation = Some(1);
        }
        if n >= 4 {
            original.tmux_session_name = None;
        }
        if n == 5 {
            original.restart_generation = Some(1);
        }
        let path = durable(&original);
        let before = std::fs::read(&path).unwrap();
        let discord =
            crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
        let http = discord.http.clone();
        let target = shared.clone();
        let lane = ScopedRestoreLane::new(channel);
        lane.run(
            RestoreScope::new(&home, "claude", "mini").unwrap(),
            1,
            move |mut attempt| async move {
                attempt.acknowledge(InstallStage::Queue)?;
                let installed = install_inflight_scoped(
                    &http,
                    &target,
                    &ProviderKind::Claude,
                    &attempt.context,
                )
                .await?;
                attempt.observe(installed.observation.clone());
                let witness = complete(&mut attempt);
                assert!(
                    resume_inflight_scoped(
                        &http,
                        &target,
                        &ProviderKind::Claude,
                        &attempt.context,
                        &witness,
                        &installed
                    )
                    .await
                    .is_err(),
                    "Lost/Released cannot resume effects"
                );
                Ok(())
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            discord.calls().is_empty(),
            "installation must make zero REST calls"
        );
        assert_eq!(
            std::fs::read(&path).ok(),
            Some(before),
            "installation must preserve canonical obligation"
        );
        let expected = match n {
            0 => InflightObservation::Admitted {
                kind: NativeTerminalKind::Aborted,
            },
            1 => InflightObservation::Admitted {
                kind: NativeTerminalKind::Completed,
            },
            2 => InflightObservation::ReplayHeld {
                receipt: None,
                reasons: vec!["canonical debt retained".into()],
                delivery: None,
            },
            3 => InflightObservation::HerdrHeld,
            4 => InflightObservation::Active,
            _ => {
                InflightObservation::Deferred("released target preparation awaits adoption".into())
            }
        };
        assert_eq!(lane.witness().inflight, Some(expected));
        assert_eq!(lane.witness().status, RestoreStatus::Restored);
        let snapshot = shared
            .mailbox(ChannelId::new(channel))
            .try_snapshot()
            .await
            .unwrap();
        assert_eq!(
            snapshot.cancel_token.is_some(),
            n == 4,
            "only an ordinary active row is minted"
        );
        channel_home::unregister_if_same(&gate);
    }
    assert_eq!(
        shared.tmux_watchers.len(),
        0,
        "installation spawns no provider watchers"
    );
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "installation starts no provider sessions"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn s3act_b2_unverified_receipts_block_without_durable_error_holds_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, _) = fixture().await;
    let closed = sqlx::PgPool::connect(&db.database_url).await.unwrap();
    closed.close().await;
    let unavailable =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(closed));
    for (n, projected) in [(0, true), (1, false)] {
        let channel = 5340221 + n;
        let mut original = row(channel, Some(NativeTerminalKind::Completed));
        original.replay_receipt_id = projected.then_some(999999);
        original.restart_generation = Some(1);
        let path = durable(&original);
        let before = std::fs::read(&path).unwrap();
        let lane = ScopedRestoreLane::new(channel);
        let target = unavailable.clone();
        let discord =
            crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
        let http = discord.http.clone();
        let result = lane
            .run(
                RestoreScope::new(&home(channel, HomeState::Worker), "claude", "mini").unwrap(),
                1,
                move |mut attempt| async move {
                    attempt.acknowledge(InstallStage::Queue)?;
                    match install_inflight_scoped(
                        &http,
                        &target,
                        &ProviderKind::Claude,
                        &attempt.context,
                    )
                    .await
                    {
                        Err(reason) => {
                            attempt.block(&reason);
                            Err(reason)
                        }
                        Ok(_) => panic!("unverified receipt must block installation"),
                    }
                },
            )
            .await
            .unwrap();
        assert!(result.is_err());
        assert!(matches!(lane.witness().status, RestoreStatus::Blocked(_)));
        assert_eq!(
            std::fs::read(path).ok(),
            Some(before),
            "lookup errors never write or invalidate the row"
        );
        assert!(discord.calls().is_empty());
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn s3act_b2_admitted_kinds_resume_only_after_installation_and_output_admission_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = fixture().await;
    for (n, kind, state) in [
        (0, NativeTerminalKind::Aborted, HomeState::Worker),
        (1, NativeTerminalKind::Completed, HomeState::Worker),
        (2, NativeTerminalKind::Aborted, HomeState::Reclaiming),
    ] {
        let channel = 5340241 + n;
        let home = home(channel, state);
        seed_home(&pool, &home).await;
        let gate = channel_home::register_for_test(channel, Some(state));
        if state == HomeState::Reclaiming {
            assert!(gate.admit_command("claude").is_none());
        }
        let original = row(channel, Some(kind));
        let path = durable(&original);
        let discord = Arc::new(
            crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await,
        );
        let during_install = discord.clone();
        let ambient_output = gate.admit_recovery("claude");
        let http = discord.http.clone();
        let target = shared.clone();
        let lane = ScopedRestoreLane::new(channel);
        lane.run(
            RestoreScope::new(&home, "claude", "mini").unwrap(),
            1,
            move |mut attempt| async move {
                attempt.acknowledge(InstallStage::Queue)?;
                let installed = channel_home::command_scope(
                    ambient_output,
                    install_inflight_scoped(
                        &http,
                        &target,
                        &ProviderKind::Claude,
                        &attempt.context,
                    ),
                )
                .await?;
                assert!(
                    during_install.calls().is_empty(),
                    "installation must make zero REST calls"
                );
                attempt.observe(installed.observation.clone());
                let incomplete = RestoreWitness {
                    scope: Some(attempt.context.scope.clone()),
                    generation: 1,
                    status: RestoreStatus::Restoring,
                    inflight: Some(installed.observation.clone()),
                };
                assert!(
                    resume_inflight_scoped(
                        &http,
                        &target,
                        &ProviderKind::Claude,
                        &attempt.context,
                        &incomplete,
                        &installed
                    )
                    .await
                    .is_err()
                );
                let witness = complete(&mut attempt);
                let outcome = resume_inflight_scoped(
                    &http,
                    &target,
                    &ProviderKind::Claude,
                    &attempt.context,
                    &witness,
                    &installed,
                )
                .await?;
                attempt.observe(outcome);
                Ok(())
            },
        )
        .await
        .unwrap()
        .unwrap();
        let expected = if kind == NativeTerminalKind::Aborted {
            crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE
        } else {
            "captured completion"
        };
        assert!(
            discord.contents().iter().any(|s| s.contains(expected)),
            "delivery must use the admitted kind"
        );
        if kind == NativeTerminalKind::Aborted {
            assert!(
                !discord
                    .contents()
                    .iter()
                    .any(|s| s.contains("captured completion"))
            );
        }
        assert!(
            !path.exists(),
            "only confirmed settlement clears its canonical row"
        );
        assert_eq!(
            lane.witness().inflight,
            Some(InflightObservation::Settled { kind }),
            "the admitted kind must survive settlement"
        );
        assert_eq!(gate.commands_in_flight(), 0);
        channel_home::unregister_if_same(&gate);
    }
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn s3act_b2_retained_terminal_keeps_reason_and_obligation_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = fixture().await;
    let channel = 5340261;
    let home = home(channel, HomeState::Worker);
    seed_home(&pool, &home).await;
    let gate = channel_home::register_for_test(channel, Some(HomeState::Worker));
    let mut original = row(channel, Some(NativeTerminalKind::Completed));
    original.full_response.clear();
    let path = durable(&original);
    let before = std::fs::read(&path).unwrap();
    let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
    let http = discord.http.clone();
    let target = shared.clone();
    let lane = ScopedRestoreLane::new(channel);
    lane.run(
        RestoreScope::new(&home, "claude", "mini").unwrap(),
        1,
        move |mut attempt| async move {
            attempt.acknowledge(InstallStage::Queue)?;
            let installed =
                install_inflight_scoped(&http, &target, &ProviderKind::Claude, &attempt.context)
                    .await?;
            attempt.observe(installed.observation.clone());
            let witness = complete(&mut attempt);
            let outcome = resume_inflight_scoped(
                &http,
                &target,
                &ProviderKind::Claude,
                &attempt.context,
                &witness,
                &installed,
            )
            .await?;
            assert_eq!(
                outcome,
                InflightObservation::Retained {
                    kind: NativeTerminalKind::Completed,
                    reason: "a completion with no stored body".into()
                }
            );
            attempt.observe(outcome);
            assert!(
                resume_inflight_scoped(
                    &http,
                    &target,
                    &ProviderKind::Claude,
                    &attempt.context,
                    &witness,
                    &installed
                )
                .await
                .is_err(),
                "same installation cannot execute effects twice"
            );
            Ok(())
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(std::fs::read(path).ok(), Some(before));
    assert!(discord.calls().is_empty());
    assert_eq!(
        lane.witness().status,
        RestoreStatus::Restored,
        "independent installation proof survives retention"
    );
    channel_home::unregister_if_same(&gate);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn s3act_b2_old_generation_cannot_install_a_mailbox_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, shared) = fixture().await;
    let channel = 5340281;
    let mut original = row(channel, None);
    original.tmux_session_name = None;
    durable(&original);
    let context = RestoreContext {
        scope: RestoreScope::new(&home(channel, HomeState::Worker), "claude", "mini").unwrap(),
        generation: 1,
        current: Arc::new(std::sync::atomic::AtomicU64::new(2)),
    };
    let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
    let result =
        install_inflight_scoped(&discord.http, &shared, &ProviderKind::Claude, &context).await;
    assert!(
        shared
            .mailbox(ChannelId::new(channel))
            .try_snapshot()
            .await
            .unwrap()
            .cancel_token
            .is_none(),
        "old runtime generation must have zero mailbox installation effects"
    );
    assert!(result.is_err());
    assert!(discord.calls().is_empty());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn s3act_b2_fresh_receipt_precedes_admitted_settlement_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = fixture().await;
    let channel = 5340291;
    let home = home(channel, HomeState::Worker);
    seed_home(&pool, &home).await;
    let gate = channel_home::register_for_test(channel, Some(HomeState::Worker));
    let original = row(channel, Some(NativeTerminalKind::Aborted));
    let path = durable(&original);
    let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
    let http = discord.http.clone();
    let target = shared.clone();
    let db_pool = pool.clone();
    let lane = ScopedRestoreLane::new(channel);
    lane.run(RestoreScope::new(&home, "claude", "mini").unwrap(), 1, move |mut attempt| async move {
        attempt.acknowledge(InstallStage::Queue)?;
        let installed = install_inflight_scoped(&http, &target, &ProviderKind::Claude, &attempt.context).await?;
        attempt.observe(installed.observation.clone()); let witness = complete(&mut attempt);
        sqlx::query("INSERT INTO intake_outbox (target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,
            request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_only,replay_disposition,
            replay_source_message_ids) VALUES ('worker','leader',$1,$2,'7','original request','standard','agent',
            'claude','unknown',TRUE,'withheld',$3)")
            .bind(channel.to_string()).bind((channel + 1).to_string()).bind(vec![(channel + 1).to_string()])
            .execute(&db_pool).await.unwrap();
        let outcome = resume_inflight_scoped(&http, &target, &ProviderKind::Claude, &attempt.context,
            &witness, &installed).await?;
        assert!(matches!(outcome, InflightObservation::ReplayHeld { receipt: Some(_), delivery: Some(Ok(())), .. }),
            "fresh replay authority precedes admitted terminal cleanup");
        attempt.observe(outcome); Ok(())
    }).await.unwrap().unwrap();
    assert!(
        path.exists(),
        "replay preserves the original request episode"
    );
    assert!(
        discord
            .contents()
            .iter()
            .any(|s| s.contains("captured completion")),
        "held captured debt delivery must be observed"
    );
    assert!(!discord.contents().iter().any(|s| s.contains(
        crate::services::discord::recovery_engine::herdr_admitted_restart::ADMITTED_ABORT_NOTICE
    )));
    channel_home::unregister_if_same(&gate);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn s3act_b2_resume_rechecks_home_epoch_and_input_fence_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool, shared) = fixture().await;
    let channel = 5340295;
    let home = home(channel, HomeState::Worker);
    seed_home(&pool, &home).await;
    let gate = channel_home::register_for_test(channel, Some(HomeState::Worker));
    let original = row(channel, Some(NativeTerminalKind::Completed));
    let path = durable(&original);
    let before = std::fs::read(&path).unwrap();
    let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
    let http = discord.http.clone();
    let target = shared.clone();
    let db_pool = pool.clone();
    let lane = ScopedRestoreLane::new(channel);
    lane.run(
        RestoreScope::new(&home, "claude", "mini").unwrap(),
        1,
        move |mut attempt| async move {
            attempt.acknowledge(InstallStage::Queue)?;
            let installed =
                install_inflight_scoped(&http, &target, &ProviderKind::Claude, &attempt.context)
                    .await?;
            attempt.observe(installed.observation.clone());
            let witness = complete(&mut attempt);
            sqlx::query("UPDATE o_channel_homes SET epoch = 2 WHERE channel_id = $1")
                .bind(channel.to_string())
                .execute(&db_pool)
                .await
                .unwrap();
            assert!(
                resume_inflight_scoped(
                    &http,
                    &target,
                    &ProviderKind::Claude,
                    &attempt.context,
                    &witness,
                    &installed
                )
                .await
                .is_err(),
                "fresh home identity must be rechecked before effects"
            );
            sqlx::query("UPDATE o_channel_homes SET epoch = 1 WHERE channel_id = $1")
                .bind(channel.to_string())
                .execute(&db_pool)
                .await
                .unwrap();
            let input = crate::services::discord::input_runtime::fence::Gate::protect(
                ProviderKind::Claude,
                channel,
            )
            .unwrap();
            let _health =
                crate::services::discord::input_runtime::fence::test_health::Clear::new(&input);
            assert!(
                resume_inflight_scoped(
                    &http,
                    &target,
                    &ProviderKind::Claude,
                    &attempt.context,
                    &witness,
                    &installed
                )
                .await
                .is_err(),
                "input fence remains an independent admission"
            );
            Ok(())
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(discord.calls().is_empty());
    assert_eq!(std::fs::read(path).ok(), Some(before));
    channel_home::unregister_if_same(&gate);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn s3act_b2_install_defers_active_source_queue_feedback_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (db, pool, shared) = fixture().await;
    let channel = 5340297;
    let mut original = row(channel, None);
    original.tmux_session_name = None;
    durable(&original);
    let id = MessageId::new(original.user_msg_id);
    let item = crate::services::turn_orchestrator::Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: id,
        queued_generation: 1,
        source_message_ids: vec![id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "original request".into(),
        mode: crate::services::turn_orchestrator::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    };
    shared
        .mailbox(ChannelId::new(channel))
        .merge_restored_queue_items(
            vec![item],
            queue_persistence_context(&shared, &ProviderKind::Claude, ChannelId::new(channel)),
        )
        .await;
    shared
        .queued
        .queued_placeholders
        .insert((ChannelId::new(channel), id), MessageId::new(channel + 3));
    let discord = crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
    let http = discord.http.clone();
    let target = shared.clone();
    let lane = ScopedRestoreLane::new(channel);
    lane.run(
        RestoreScope::new(&home(channel, HomeState::Worker), "claude", "mini").unwrap(),
        1,
        move |mut attempt| async move {
            attempt.acknowledge(InstallStage::Queue)?;
            let installed =
                install_inflight_scoped(&http, &target, &ProviderKind::Claude, &attempt.context)
                    .await?;
            attempt.observe(installed.observation);
            complete(&mut attempt);
            Ok(())
        },
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        shared
            .queued
            .queued_placeholders
            .contains_key(&(ChannelId::new(channel), id)),
        "installation must defer queue-exit placeholder effects"
    );
    let actor = shared
        .mailbox(ChannelId::new(channel))
        .try_snapshot()
        .await
        .unwrap();
    assert!(actor.cancel_token.is_some());
    assert!(actor.intervention_queue.is_empty());
    assert!(discord.calls().is_empty());
    pool.close().await;
    db.drop().await;
}
