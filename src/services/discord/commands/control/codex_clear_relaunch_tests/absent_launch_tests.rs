//! Restored input crosses the real readiness, intake, cold provider and hook boundaries.

use super::*;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, QueueIdentity, QueuePersistenceContext,
};
use std::sync::mpsc;

pub(in crate::services::discord) mod auxiliary_tests;
pub(in crate::services::discord) mod custody_tests;
mod late_tests;
mod negative_tests;
mod source_boundary_tests;

#[derive(Clone, Copy, Debug)]
enum HistoricalMarker {
    Absent,
    ForeignIdentity,
    OwnedMemory,
    OwnedBoot,
}

struct HoldHookReset;

impl Drop for HoldHookReset {
    fn drop(&mut self) {
        *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

fn isolated(test: &str) -> bool {
    crate::services::tui_o::cutover::test_override::in_empty_list_process(test)
        && crate::services::discord::admin_host_guard::tests::api_child(
            test.split_once("::").unwrap().1,
        )
}

#[tokio::test(flavor = "current_thread")]
async fn absent_pin_restored_head_reaches_fresh_provider_pg() {
    let test = concat!(
        module_path!(),
        "::absent_pin_restored_head_reaches_fresh_provider_pg"
    );
    if isolated(test) {
        restored_head_reaches_provider(HistoricalMarker::Absent, true, true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn disconnected_foreign_identity_queue_kickoff_starts_turn_pg() {
    if isolated(concat!(
        module_path!(),
        "::disconnected_foreign_identity_queue_kickoff_starts_turn_pg"
    )) {
        restored_head_reaches_provider(HistoricalMarker::ForeignIdentity, true, true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn owned_pin_memory_restored_head_reaches_fresh_provider_pg() {
    let test = concat!(
        module_path!(),
        "::owned_pin_memory_restored_head_reaches_fresh_provider_pg"
    );
    if isolated(test) {
        restored_head_reaches_provider(HistoricalMarker::OwnedMemory, true, true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn owned_pin_boot_restored_head_reaches_fresh_provider_pg() {
    let test = concat!(
        module_path!(),
        "::owned_pin_boot_restored_head_reaches_fresh_provider_pg"
    );
    if isolated(test) {
        restored_head_reaches_provider(HistoricalMarker::OwnedBoot, true, true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn absent_pin_warm_off_with_stale_session_reaches_fresh_provider_pg() {
    if isolated(concat!(
        module_path!(),
        "::absent_pin_warm_off_with_stale_session_reaches_fresh_provider_pg"
    )) {
        restored_head_reaches_provider(HistoricalMarker::Absent, false, true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn absent_pin_warm_off_without_session_reaches_fresh_provider_pg() {
    if isolated(concat!(
        module_path!(),
        "::absent_pin_warm_off_without_session_reaches_fresh_provider_pg"
    )) {
        restored_head_reaches_provider(HistoricalMarker::Absent, false, false).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn absent_pin_warm_on_without_session_reaches_fresh_provider_pg() {
    if isolated(concat!(
        module_path!(),
        "::absent_pin_warm_on_without_session_reaches_fresh_provider_pg"
    )) {
        restored_head_reaches_provider(HistoricalMarker::Absent, true, false).await;
    }
}

async fn restore_disk_head(fixture: &mut CanaryFixture) -> QueueIdentity {
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    clear_persist_failure_tests::seed_backlog(&fixture.shared, &ProviderKind::Codex, channel).await;
    let (disk, _) =
        crate::services::turn_orchestrator::test_support::load_channel_pending_queue_checked(
            &ProviderKind::Codex,
            &fixture.shared.token_hash,
            channel,
        )
        .expect("the seed must be a readable durable queue");
    assert_eq!(disk.len(), 1);
    let identity = QueueIdentity::of(&disk[0]);
    Arc::get_mut(&mut fixture.shared)
        .expect("fixture is not shared before kickoff")
        .mailboxes = ChannelMailboxRegistry::default();
    let mailbox = fixture.shared.mailbox(channel);
    assert!(mailbox.snapshot().await.intervention_queue.is_empty());
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Codex, &fixture.shared.token_hash, None);
    let restored = mailbox.hydrate_pending_queue_from_disk(persistence).await;
    assert!(restored.persistence_error.is_none(), "{restored:?}");
    assert_eq!(restored.absorbed, 1);
    let snapshot = mailbox.snapshot().await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(QueueIdentity::of(&snapshot.intervention_queue[0]), identity);
    identity
}

async fn restored_head_reaches_provider(marker: HistoricalMarker, warm: bool, stale: bool) {
    let mut fixture =
        CanaryFixture::build_with_host(0, CodexSourceMode::Verified, Some("b1-absent-test-host"))
            .await;
    let _warm = fixture.warm_followup(warm);
    assert_eq!(
        crate::services::codex_tui::warm_followup::codex_tui_warm_followup_enabled(),
        warm
    );
    if !stale {
        fixture
            .shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&serenity::ChannelId::new(CANARY_CHANNEL))
            .unwrap()
            .session_id = None;
    }
    assert_eq!(fixture.context.host.as_deref(), Some("b1-absent-test-host"));
    assert_eq!(
        dedupe::binding_context::stable_host_identity(),
        fixture.context.host
    );
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        |method, path| {
            (method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{CANARY_CHANNEL}")))
            .then(|| {
                json!({
                    "id": CANARY_CHANNEL.to_string(), "type": 0,
                    "name": "adk-codex-tui-e2e", "guild_id": "42000", "position": 0,
                    "permission_overwrites": [], "nsfw": false, "parent_id": null
                })
            })
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    let (request_tx, request_rx) = mpsc::channel();
    let _request_setup = crate::services::discord::codex_absence::test_setup::install(
        CANARY_CHANNEL,
        Box::new(move |_| {
            let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
                &ProviderKind::Codex,
                CANARY_CHANNEL,
            )
            .unwrap()
            .expect("actual provider entry must own its admitted inflight row");
            request_tx.send(row.session_id).unwrap();
            Box::new(())
        }),
    );
    let old_history = fs::read(fixture.history()).unwrap();
    let old_rollout = fixture
        .context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{NATIVE_ID}.jsonl"));
    let old_body = fs::read(&old_rollout).unwrap();
    let old_archive = PathBuf::from(&fixture.context.owner_runtime_root)
        .join("runtime/binding_evidence/codex")
        .join(format!("{}.json", fixture.context.execution_nonce));
    let old_archive_bytes = fs::read(&old_archive).unwrap();
    fixture.fake.mark_missing();
    fixture.alive.store(false, Ordering::SeqCst);
    crate::services::session_backend::remove_process_session(CANARY_TMUX);
    if matches!(
        marker,
        HistoricalMarker::Absent | HistoricalMarker::ForeignIdentity
    ) {
        crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
    } else {
        assert_eq!(
            dedupe::binding_context::observe_spawn_nonce_marker(CANARY_TMUX),
            SpawnNonceMarker::Known(fixture.context.execution_nonce.clone())
        );
        let rollout_marker = crate::services::tmux_common::session_temp_path(
            CANARY_TMUX,
            crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
        );
        let owned: serde_json::Value =
            serde_json::from_slice(&fs::read(rollout_marker).unwrap()).unwrap();
        assert_eq!(
            owned["codex_ownership"]["execution_nonce"],
            fixture.context.execution_nonce
        );
    }
    if matches!(marker, HistoricalMarker::OwnedBoot) {
        assert!(dedupe::evict_dead_tmux_mirror(CANARY_TMUX));
        assert!(dedupe::runtime_binding_for_tmux_session(CANARY_TMUX).is_none());
    }
    let channel = serenity::ChannelId::new(CANARY_CHANNEL);
    let identity = restore_disk_head(&mut fixture).await;
    let initial = fixture.shared.mailbox(channel).snapshot().await;
    let prompt = initial.intervention_queue[0].text.clone();
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only_result(
            &ProviderKind::Codex,
            CANARY_CHANNEL,
        )
        .unwrap()
        .is_none()
    );
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    let activity = settled_owner_observation(
        "B1_AFTER_ABSENCE superseded_retries",
        || activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel),
        |sample| sample.reason == "superseded",
    )
    .await;
    assert_eq!(
        (activity.activity, activity.reason),
        (Activity::Idle, "no_session")
    );
    if matches!(marker, HistoricalMarker::ForeignIdentity) {
        let pool = fixture.shared.pg_pool.as_ref().unwrap();
        sqlx::query("UPDATE sessions SET identity_kind = NULL, discord_token_hash = NULL")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, channel_id,
                                   identity_kind, discord_token_hash, last_heartbeat)
             VALUES ($1, 'codex', 'disconnected', $2, 'discord_channel', $3,
                     NOW() - INTERVAL '60 days')",
        )
        .bind(format!(
            "codex/{}/old-mac-mini:{CANARY_TMUX}",
            fixture.shared.token_hash
        ))
        .bind(CANARY_CHANNEL.to_string())
        .bind(&fixture.shared.token_hash)
        .execute(pool)
        .await
        .unwrap();
    }
    if !matches!(marker, HistoricalMarker::ForeignIdentity) {
        let blocked = crate::services::discord::router::hosted_tui_promote_readiness_blocked(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
        )
        .await;
        assert!(
            !blocked,
            "{marker:?}: confirmed absence must pass real readiness without clear; head={identity:?}"
        );
    }
    let (hold_tx, hold_rx) = mpsc::channel();
    *crate::services::codex_tui::verified_hold::HOLD_ENTERED_FOR_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(hold_tx);
    let _hold_reset = HoldHookReset;
    fixture.fake.take_calls();
    let deps = crate::services::discord::router::IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: TEST_BOT,
    };
    let outcome = crate::services::discord::queue_dispatch::kickoff::kickoff_idle_queue_channel(
        &deps,
        &ProviderKind::Codex,
        channel,
    )
    .await;
    assert!(
        outcome.started,
        "{marker:?}: restored head never entered real intake"
    );
    let guard = fixture.turn_failure_guard().await;
    let held = async {
        loop {
            match hold_rx.try_recv() {
                Ok(tmux) => break tmux,
                Err(mpsc::TryRecvError::Empty) => {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(mpsc::TryRecvError::Disconnected) => std::future::pending().await,
            }
        }
    };
    let new = tokio::select! {
        new = fixture.wait_new_context() => {
            assert_ne!(new.execution_nonce, fixture.context.execution_nonce);
            assert!(!dedupe::codex_verified_o_source_allowed(CANARY_CHANNEL, &fixture.old_source));
            Some(fixture.publish_context(new, &api, false, &mut Vec::new()).await)
        }
        held_tmux = held => {
            assert_eq!(held_tmux, CANARY_TMUX);
            eprintln!("B1_PROVIDER_HOLD_ENTERED marker={marker:?} head={identity:?}");
            guard.token.cancelled.store(true, Ordering::SeqCst);
            None
        }
        _ = async {
            while fixture.shared.mailbox(channel).snapshot().await.cancel_token.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        } => None,
    };
    tokio::time::timeout(Duration::from_secs(15), async {
        while fixture
            .shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the controlled provider must release its mailbox before assertions");
    let calls = fixture.fake.take_calls();
    let launches = calls
        .iter()
        .filter(|call| call.starts_with("new-session "))
        .count();
    assert_eq!(
        launches, 1,
        "{marker:?}: real provider must cold-launch the restored head once; actual calls={calls:?}"
    );
    let new = new.expect("a provider hold is not a fresh launch");
    assert_eq!(
        request_rx
            .try_recv()
            .expect("actual provider entry observed"),
        stale.then(|| NATIVE_ID.to_owned()),
        "the warm/stale matrix must reach the driver with its requested selector"
    );
    assert_eq!(new.launch_mode, "fresh");
    assert_eq!(new.source_policy.as_deref(), Some("verified"));
    assert!(new.expected_native_session_id.is_none());
    let argv: Vec<String> =
        serde_json::from_slice(&fs::read(fixture.root.path().join("fresh-argv.json")).unwrap())
            .unwrap();
    assert!(!argv.iter().any(|arg| arg == "resume" || arg == NATIVE_ID));
    assert!(
        argv.iter().any(|arg| arg.contains(&prompt)),
        "the fresh CLI must receive the restored text: {argv:?}"
    );
    assert!(
        !calls.iter().any(|call| call.starts_with("kill-session")),
        "{calls:?}"
    );
    assert_eq!(fs::read(old_rollout).unwrap(), old_body);
    assert_eq!(fs::read(old_archive).unwrap(), old_archive_bytes);
    assert!(
        fs::read(fixture.history())
            .unwrap()
            .starts_with(&old_history)
    );
    assert_eq!(
        dedupe::binding_context::execution_context("codex", &fixture.context.execution_nonce)
            .unwrap(),
        fixture.context
    );
    assert!(!dedupe::clear_relaunch::relaunch_open(CANARY_TMUX));
    let after = fixture.shared.mailbox(channel).snapshot().await;
    assert!(after.intervention_queue.is_empty() && after.pending_user_dispatch.is_none());
    let (disk, _) =
        crate::services::turn_orchestrator::test_support::load_channel_pending_queue_checked(
            &ProviderKind::Codex,
            &fixture.shared.token_hash,
            channel,
        )
        .unwrap();
    assert!(disk.is_empty());
    let sent = tokio::time::timeout(Duration::from_secs(15), async {
        let mut sent = Vec::new();
        loop {
            sent.extend(api.take());
            let completed = sent.iter().any(|call| {
                let mut parts = call.splitn(3, ' ');
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();
                let body = parts.next().unwrap_or_default();
                matches!(method, "POST" | "PATCH")
                    && path.contains(&format!("/channels/{CANARY_CHANNEL}/messages"))
                    && serde_json::from_str::<serde_json::Value>(body)
                        .is_ok_and(|value| value["content"] == "clear relaunch completed")
            });
            let summary = sent.iter().any(|call| {
                let mut parts = call.splitn(3, ' ');
                let method = parts.next().unwrap_or_default();
                let path = parts.next().unwrap_or_default();
                let body = parts.next().unwrap_or_default();
                method == "POST"
                    && path.contains(&format!("/channels/{CANARY_CHANNEL}/messages"))
                    && serde_json::from_str::<serde_json::Value>(body).is_ok_and(|value| {
                        value["content"]
                            .as_str()
                            .is_some_and(|content| content.starts_with("-# ⏱️ "))
                    })
            });
            if completed && summary {
                break sent;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the completed bridge must publish its final response");
    assert_one_completed_response(&sent);
    guard.finish();
}
