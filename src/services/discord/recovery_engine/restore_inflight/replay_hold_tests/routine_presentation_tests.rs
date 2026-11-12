use super::*;
use crate::services::discord::routine_presentation::RoutinePresentation;
use sha2::{Digest, Sha256};

async fn restored(committed: bool, held: bool, evidence: &str, quiet: bool, captured: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let (db, pool) = fixture().await;
    let shared = shared_on(&pool).await;
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
    let raw_hash = format!("{:x}", Sha256::digest(bytes.as_bytes()));
    inflight::save_inflight_state_create_new(&original).unwrap();
    let serialized = inflight::load_inflight_state(&provider, channel).unwrap();
    assert_eq!(
        serialized.routine_presentation,
        original.routine_presentation
    );
    let discord = o_cut_recorder::start(channel).await;
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
