use super::*;
use crate::services::discord::routine_presentation::RoutinePresentation;
use crate::test_support::wait::{WAIT_CAP, await_within};
use sha2::{Digest, Sha256};

async fn restored(committed: bool, held: bool, evidence: &str, quiet: bool, captured: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let mut shared = shared_on(&pool).await;
    let drain = evidence.starts_with("drain");
    let old_generation = if drain {
        // Each isolated root needs its own allocator epoch, not the prior root cache.
        crate::services::discord::runtime_store::set_process_generation_for_tests(None);
        crate::services::discord::runtime_store::allocate_process_generation()
    } else {
        0
    };
    let provider = ProviderKind::Claude;
    let channel = 1_479_811_700_000_100_000;
    let name = provider.build_tmux_session_name("routine-restart");
    seed(
        &pool,
        &channel_key(&shared, &name),
        &name,
        channel,
        Stored::Legacy,
    )
    .await;
    let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&name), HostLiveness::DeadOrAbsent);
    let _present = InjectedPresenceGuard::set(HostSessionRef::tmux(&name), HostPresence::Missing);
    let output = tempfile::NamedTempFile::new().unwrap();
    let mut original = row(channel, name);
    original.born_generation = old_generation;
    original.full_response = if evidence == "empty" {
        ""
    } else {
        "진행1\n진행2"
    }
    .into();
    original.streaming_rollover_frozen_msg_ids.clear();
    original.current_msg_len = 0;
    original.response_sent_offset = if committed {
        original.full_response.len()
    } else {
        0
    };
    original.terminal_delivery_committed = committed;
    original.turn_start_offset = Some(0);
    let bytes = format!(
        "{}\n{}",
        serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":original.full_response}]}}),
        if captured {
            format!(
                "{}\n",
                serde_json::json!({"type":"result","subtype":"success","result":"NO_REPLY"})
            )
        } else {
            String::new()
        }
    );
    std::fs::write(output.path(), &bytes).unwrap();
    original.output_path = Some(output.path().display().to_string());
    original.last_offset = bytes.len() as u64;
    if evidence != "legacy" {
        original.routine_presentation =
            RoutinePresentation::at_start(&provider, Some("routine"), true);
        if evidence != "missing" {
            original
                .routine_presentation
                .observe_terminal(if evidence == "full" {
                    "완료"
                } else {
                    "NO_REPLY"
                });
        }
        if evidence == "ambiguous" {
            original.routine_presentation.observe_terminal("NO_REPLY");
        }
    }
    let receipt_id = if held {
        let id = receipt(&pool, &original, "withheld").await;
        original.replay_receipt_id = Some(id);
        Some(id)
    } else {
        None
    };
    if evidence == "drain_missing_receipt" {
        original.replay_receipt_id = Some(i64::MAX);
    }
    if evidence == "drain_unprojected_hold" {
        original.replay_receipt_id = None;
    }
    let raw_hash = format!("{:x}", Sha256::digest(bytes.as_bytes()));
    inflight::save_inflight_state_create_new(&original).unwrap();
    if drain {
        let report = inflight::mark_restart_mode_blocking(
            provider.clone(),
            crate::services::discord::InflightRestartMode::DrainRestart,
        )
        .await;
        assert_eq!(report.marked, 1);
        assert!(report.failed.is_empty() && !report.incomplete);
        Arc::get_mut(&mut shared)
            .unwrap()
            .restart
            .current_generation = old_generation + 1;
    }
    let serialized = inflight::load_inflight_state(&provider, channel).unwrap();
    if drain {
        assert_eq!(serialized.restart_generation, Some(old_generation));
    }
    assert_eq!(
        serialized.routine_presentation,
        original.routine_presentation
    );
    let discord = o_cut_recorder::start(channel).await;
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &provider,
        channel,
    );
    let before = std::fs::read(&path).unwrap();
    if evidence == "drain_closed_pool" {
        pool.close().await;
    }
    if evidence.starts_with("drain_race_") {
        Box::pin(async {
            let mut transaction = pool.begin().await.unwrap();
            sqlx::query("LOCK TABLE intake_outbox IN ACCESS EXCLUSIVE MODE")
                .execute(&mut *transaction)
                .await
                .unwrap();
            let task_shared = shared.clone();
            let task_row = serialized.clone();
            let task = tokio::spawn(async move {
                super::super::settle_routine_no_visible(
                    &task_shared,
                    &ProviderKind::Claude,
                    &task_row,
                )
                .await
            });
            await_within(
                async {
                    loop {
                        let blocked: bool = sqlx::query_scalar(
                            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity
                         WHERE datname=current_database() AND wait_event_type='Lock'
                         AND pid<>pg_backend_pid() AND query LIKE '%intake_outbox%')",
                        )
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                        if blocked {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                },
                WAIT_CAP,
            )
            .await;
            let actor = if evidence == "drain_race_actor" {
                let actor = Arc::new(CancelToken::new());
                assert!(
                    shared
                        .mailbox(ChannelId::new(channel))
                        .try_start_turn(
                            actor.clone(),
                            serenity::UserId::new(7),
                            serenity::MessageId::new(original.user_msg_id),
                        )
                        .await
                );
                Some(actor)
            } else {
                let mut locked = inflight::lock_inflight_episode(
                    &provider,
                    channel,
                    &inflight::InflightEpisodePin::from_state(&serialized),
                )
                .unwrap();
                let advanced = locked.state().clone();
                assert!(matches!(
                    locked.persist_verified_under_guard(&advanced, "receipt_generation_race"),
                    inflight::GuardedSaveOutcome::Saved
                ));
                None
            };
            let expected = std::fs::read(&path).unwrap();
            transaction.commit().await.unwrap();
            assert!(await_within(task, WAIT_CAP).await.unwrap());
            assert_eq!(
                std::fs::read(&path).unwrap(),
                expected,
                "receipt await cannot authorize a stale adoption"
            );
            let kept = inflight::load_inflight_state(&provider, channel).unwrap();
            assert_eq!(kept.restart_mode, serialized.restart_mode);
            assert!(!kept.terminal_delivery_committed);
            if let Some(actor) = actor {
                let snapshot = shared.mailbox(ChannelId::new(channel)).snapshot().await;
                assert!(Arc::ptr_eq(snapshot.cancel_token.as_ref().unwrap(), &actor));
            }
            assert!(
                discord
                    .calls()
                    .iter()
                    .all(|c| !c.route.starts_with("POST ") && !c.route.starts_with("PATCH "))
            );
            assert_eq!(
                format!(
                    "{:x}",
                    Sha256::digest(std::fs::read(output.path()).unwrap())
                ),
                raw_hash
            );
            pool.close().await;
            db.drop().await;
        })
        .await;
        return;
    }
    let live_actor = if evidence == "drain_live" {
        let actor = Arc::new(CancelToken::new());
        assert!(
            shared
                .mailbox(ChannelId::new(channel))
                .try_start_turn(
                    actor.clone(),
                    serenity::UserId::new(7),
                    serenity::MessageId::new(original.user_msg_id),
                )
                .await
        );
        Some(actor)
    } else {
        None
    };
    let lease = if evidence == "lease_busy" {
        match crate::services::discord::turn_bridge::NoVisibleResultLease::acquire(
            &shared,
            ChannelId::new(channel),
            &serialized,
            Some(serialized.last_offset),
        ) {
            Some(lease) => Some(lease),
            _ => panic!("the competing owner must hold the real lease"),
        }
    } else {
        None
    };
    // The boot entry runs receipt settlement and the actual restore_inflight_states scan.
    if evidence == "idle" {
        let gateway = crate::services::discord::gateway::DiscordGateway::new(
            discord.http.clone(),
            shared.clone(),
            provider.clone(),
            None,
        );
        assert!(super::super::super::idle_captured_response::recover_idle_partial_response_from_ready_source(
            &discord.http, &shared, &serialized, output.path(), &gateway,
        ).await);
    } else {
        restore_inflight_turns(&discord.http, &shared, &provider).await;
    }
    if matches!(evidence, "drain_missing_receipt" | "drain_closed_pool") {
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "unverified receipt must preserve the marker and save generation"
        );
        let kept = inflight::load_inflight_state(&provider, channel).unwrap();
        assert_eq!(kept.restart_mode, serialized.restart_mode);
        assert_eq!(kept.restart_generation, serialized.restart_generation);
        assert_eq!(kept.save_generation, serialized.save_generation);
        assert!(!kept.terminal_delivery_committed);
        assert!(
            discord
                .calls()
                .iter()
                .all(|c| !c.route.starts_with("POST ") && !c.route.starts_with("PATCH "))
        );
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(output.path()).unwrap())
            ),
            raw_hash
        );
        pool.close().await;
        db.drop().await;
        return;
    }
    if let Some(actor) = live_actor {
        let kept = inflight::load_inflight_state(&provider, channel).unwrap();
        assert_eq!(
            serde_json::to_value(&kept).unwrap(),
            serde_json::to_value(&serialized).unwrap()
        );
        let snapshot = shared.mailbox(ChannelId::new(channel)).snapshot().await;
        assert!(Arc::ptr_eq(snapshot.cancel_token.as_ref().unwrap(), &actor));
        assert!(discord.contents().is_empty());
        assert_eq!(
            format!(
                "{:x}",
                Sha256::digest(std::fs::read(output.path()).unwrap())
            ),
            raw_hash
        );
        pool.close().await;
        db.drop().await;
        return;
    }
    if let Some(lease) = lease {
        let kept = inflight::load_inflight_state(&provider, channel).unwrap();
        assert!(
            !kept.terminal_delivery_completed(),
            "a busy lease cannot acknowledge a quiet result"
        );
        assert_eq!(kept.full_response, original.full_response);
        assert!(discord.contents().is_empty());
        drop(lease);
        restore_inflight_turns(&discord.http, &shared, &provider).await;
    }
    let writes: Vec<_> = discord
        .calls()
        .into_iter()
        .filter(|call| call.route.starts_with("POST ") || call.route.starts_with("PATCH "))
        .collect();
    if quiet {
        assert!(
            writes.is_empty(),
            "C1 recovery leaked POST/PATCH: {writes:?}; committed={committed} held={held}"
        );
        let remaining = inflight::load_inflight_state(&provider, channel);
        if held {
            let remaining = remaining.expect("a hold retains the request episode");
            if drain {
                assert!(
                    remaining.restart_mode.is_none(),
                    "the completed drain marker must be consumed"
                );
                assert_eq!(remaining.turn_nonce, original.turn_nonce);
                assert_eq!(remaining.born_generation, original.born_generation);
            }
            assert!(
                remaining.terminal_delivery_completed(),
                "quiet debt must be settled"
            );
            assert_eq!(remaining.full_response, original.full_response);
            assert_eq!(remaining.last_offset, original.last_offset);
            assert_eq!(remaining.response_sent_offset, original.full_response.len());
        } else {
            assert!(remaining.is_none(), "the settled ordinary row must retire");
            let outcome: String =
                sqlx::query_scalar("SELECT terminal_outcome FROM turns WHERE turn_id=$1")
                    .bind(format!("discord:{channel}:{}", original.user_msg_id))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(
                outcome,
                if original.full_response.is_empty() {
                    "empty"
                } else {
                    "completed"
                },
                "no-visible success is not an interrupted run"
            );
        }
        restore_inflight_turns(&discord.http, &shared, &provider).await;
        assert_eq!(
            discord
                .calls()
                .iter()
                .filter(|call| call.route.starts_with("POST ") || call.route.starts_with("PATCH "))
                .count(),
            0,
            "a second boot cannot repost quiet debt"
        );
    } else {
        assert_eq!(
            writes
                .iter()
                .filter(|call| call
                    .content
                    .as_deref()
                    .is_some_and(|body| body.contains(&original.full_response)))
                .count(),
            1,
            "Legacy/Full/Missing/Ambiguous keep their body: {writes:?}"
        );
    }
    if let Some(id) = receipt_id {
        let kept: (String, serde_json::Value) = sqlx::query_as(
            "SELECT replay_disposition,replay_preserved FROM intake_outbox WHERE id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(kept.0, "withheld");
        assert_eq!(kept.1["body"], original.full_response);
    }
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(std::fs::read(output.path()).unwrap())
        ),
        raw_hash
    );
    println!(
        "RAW_RECOVERY_SHA256 {raw_hash} committed={committed} held={held} evidence={evidence} captured={captured}"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn i8117_recovery_quiet_before_commit_pg() {
    for captured in [false, true] {
        restored(false, false, "no_reply", true, captured).await;
    }
}
#[tokio::test]
async fn i8117_recovery_quiet_after_commit_pg() {
    for captured in [false, true] {
        restored(true, false, "no_reply", true, captured).await;
    }
}
#[tokio::test]
async fn i8117_recovery_quiet_before_commit_with_hold_pg() {
    for captured in [false, true] {
        restored(false, true, "no_reply", true, captured).await;
    }
}
#[tokio::test]
async fn i8117_recovery_quiet_after_commit_with_hold_pg() {
    for captured in [false, true] {
        restored(true, true, "no_reply", true, captured).await;
    }
}
#[tokio::test]
async fn i8117_recovery_full_controls_pg() {
    for held in [false, true] {
        for evidence in ["legacy", "full", "missing", "ambiguous"] {
            for captured in [false, true] {
                restored(false, held, evidence, false, captured).await;
            }
        }
    }
}

#[tokio::test]
async fn i8117_idle_recovery_quiet_uses_the_same_settlement_pg() {
    for captured in [false, true] {
        restored(false, false, "idle", true, captured).await;
    }
}

#[tokio::test]
async fn i8117_recovery_quiet_waits_for_the_existing_delivery_lease_pg() {
    for captured in [false, true] {
        restored(false, false, "lease_busy", true, captured).await;
    }
}

#[tokio::test]
async fn i8117_recovery_quiet_without_progress_is_terminal_pg() {
    for committed in [false, true] {
        for held in [false, true] {
            for captured in [false, true] {
                restored(committed, held, "empty", true, captured).await;
            }
        }
    }
}

#[tokio::test]
async fn i8117_drain_restart_quiet_settles_pg() {
    for committed in [false, true] {
        for held in [false, true] {
            for captured in [false, true] {
                restored(committed, held, "drain", true, captured).await;
            }
        }
    }
}

#[tokio::test]
async fn i8117_drain_restart_live_actor_preserves_marker_pg() {
    restored(false, false, "drain_live", true, true).await;
}

#[tokio::test]
async fn i8117_drain_missing_receipt_preserves_marker_pg() {
    restored(false, false, "drain_missing_receipt", true, true).await;
}

#[tokio::test]
async fn i8117_drain_closed_pool_preserves_marker_pg() {
    restored(false, true, "drain_closed_pool", true, true).await;
}

#[tokio::test]
async fn i8117_receipt_lookup_races_refuse_adoption_pg() {
    for evidence in ["drain_race_actor", "drain_race_generation"] {
        restored(false, true, evidence, true, true).await;
    }
}

#[tokio::test]
async fn i8117_drain_unprojected_hold_settles_pg() {
    restored(false, true, "drain_unprojected_hold", true, true).await;
}
