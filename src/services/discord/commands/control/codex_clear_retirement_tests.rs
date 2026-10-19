//! Retirement changes only cold-launch admission, and only after a confirmed canary end.
#![cfg(unix)]

use super::codex_clear_relaunch_tests::CanaryFixture;
use super::{ClearReport, SoftClearNotifyMode, clear_channel_session_state};
use crate::services::claude_tui::hook_server::adoption_retry::DurableKind;
use crate::services::claude_tui::hook_server::observation_ingress::{
    IngressOutcome, observe_binding_hook,
};
use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, CapturedContext, HookBindingEnvelope, ObservedHookProcess, PreparedIncarnation,
    SpawnNonceMarker,
};
use crate::services::tui_prompt_dedupe::{self as dedupe, clear_relaunch};
use poise::serenity_prelude::{ChannelId, Http};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const LATE_ID: &str = "019e660d-4859-7522-9cee-8ba7c4e7c745";

fn watermark() -> PathBuf {
    crate::config::runtime_root()
        .unwrap()
        .join("runtime/codex_clear_retired")
        .join(format!("{CANARY_CHANNEL}.json"))
}

fn vacate(fixture: &CanaryFixture) {
    fixture.fake.mark_missing();
    let _ = crate::services::session_backend::remove_process_session(CANARY_TMUX);
    crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
}

fn assert_launch_held() {
    assert!(!clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(clear_relaunch::launch_blocked(CANARY_TMUX));
    assert!(dedupe::codex_verified_requires_proof(CANARY_TMUX));
    assert!(dedupe::codex_verified_input_blocked(CANARY_TMUX));
    let cancel = Arc::new(CancelToken::new());
    cancel.cancelled.store(true, Ordering::SeqCst);
    assert!(
        crate::services::codex_tui::verified_hold::preflight(CANARY_TMUX, Some(&cancel)).is_some()
    );
}

async fn clear(fixture: &CanaryFixture, provider: &ProviderKind, channel: u64) -> ClearReport {
    clear_channel_session_state(
        &Arc::new(Http::new("")),
        &fixture.shared,
        provider,
        ChannelId::new(channel),
        "/clear",
        SoftClearNotifyMode::Suppress,
    )
    .await
    .unwrap()
}

fn late_pending_hook(fixture: &CanaryFixture) {
    fs::write(
        crate::services::tmux_common::session_temp_path(CANARY_TMUX, "spawn_nonce"),
        &fixture.context.execution_nonce,
    )
    .unwrap();
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(fixture.context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let published = chrono::Utc::now() + chrono::Duration::seconds(2);
    headers.insert(
        crate::services::claude_tui::hook_server::relay_receipts::RELAY_PUBLISHED_AT_HEADER,
        published.to_rfc3339().parse().unwrap(),
    );
    assert_eq!(
        observe_binding_hook(
            "codex",
            "session-start",
            Some(LATE_ID),
            Some(LATE_ID),
            &json!({"session_id":LATE_ID,"source":"startup"}),
            &headers,
        ),
        IngressOutcome::Durable(DurableKind::Pending),
        "an unresolved late hook must append a real newer ownership record"
    );
    crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
}

#[tokio::test(flavor = "current_thread")]
async fn relaunch_predicate_closes_on_any_live_or_newer_evidence_pg() {
    let fixture = CanaryFixture::new().await;
    let captured = clear_relaunch::capture(CANARY_TMUX).unwrap().unwrap();
    vacate(&fixture);
    clear_relaunch::commit(&captured, "explicit_clear").unwrap();
    assert!(clear_relaunch::relaunch_open(CANARY_TMUX));
    let original = fs::read(watermark()).unwrap();

    fixture.fake.mark_present();
    assert_launch_held();
    fixture.fake.mark_missing();
    fixture.fake.fail_probes();
    assert_launch_held();
    fixture.fake.restore_probes();

    crate::services::session_backend::insert_process_session(
        CANARY_TMUX,
        crate::services::session_backend::SessionHandle::TestProcess {
            pid: 6_845_052,
            alive: Arc::new(AtomicBool::new(true)),
        },
    );
    assert_launch_held();
    let _ = crate::services::session_backend::remove_process_session(CANARY_TMUX);

    let nonce = crate::services::tmux_common::session_temp_path(CANARY_TMUX, "spawn_nonce");
    fs::write(&nonce, &fixture.context.execution_nonce).unwrap();
    assert_launch_held();
    fs::remove_file(&nonce).unwrap();
    fs::create_dir(&nonce).unwrap();
    assert_launch_held();
    fs::remove_dir(&nonce).unwrap();

    let marker = crate::services::tmux_common::session_temp_path(
        CANARY_TMUX,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    );
    fs::write(&marker, br#"{"codex_ownership":{"proof_seq":1}}"#).unwrap();
    assert_launch_held();
    fs::remove_file(marker).unwrap();

    for (field, value) in [
        ("nonce", json!("different-nonce")),
        ("owner_runtime_root", json!("/another-runtime")),
        ("schema", json!(2)),
        ("retired_through_seq", json!(captured.seq.saturating_sub(1))),
    ] {
        let mut changed: Value = serde_json::from_slice(&original).unwrap();
        changed[field] = value;
        fs::write(watermark(), serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_launch_held();
    }
    for malformed in [b"invalid json".to_vec(), vec![b'x'; 1025]] {
        fs::write(watermark(), malformed).unwrap();
        assert_launch_held();
    }
    fs::write(watermark(), original).unwrap();
    assert!(clear_relaunch::relaunch_open(CANARY_TMUX));

    late_pending_hook(&fixture);
    let newer = clear_relaunch::capture(CANARY_TMUX).unwrap().unwrap();
    assert_eq!(newer.nonce, captured.nonce, "isolate the sequence check");
    assert!(newer.seq > captured.seq);
    assert_launch_held();
}

#[tokio::test(flavor = "current_thread")]
async fn clear_commits_watermark_only_after_confirmed_end_pg() {
    let fixture = CanaryFixture::new().await;
    let before = fs::read(fixture.history()).unwrap();
    fixture.fake.fail_kills();
    let refused = clear(&fixture, &ProviderKind::Codex, CANARY_CHANNEL).await;
    assert!(refused.end_unconfirmed);
    assert!(!watermark().exists());

    fixture.fake.lose_kill_ack_after_exit();
    let lost_ack = clear(&fixture, &ProviderKind::Codex, CANARY_CHANNEL).await;
    assert!(lost_ack.end_unconfirmed);
    assert_eq!(
        crate::services::platform::tmux::session_presence(CANARY_TMUX),
        crate::services::platform::tmux::SessionPresence::Missing,
        "vacancy alone must not replace the failed kill acknowledgement"
    );
    assert!(
        !watermark().exists(),
        "unconfirmed end must not commit retirement"
    );
    assert_eq!(fs::read(fixture.history()).unwrap(), before);
}

#[tokio::test(flavor = "current_thread")]
async fn clear_capture_does_not_retire_a_later_hook_record_pg() {
    let fixture = CanaryFixture::new().await;
    let captured = clear_relaunch::capture(CANARY_TMUX).unwrap().unwrap();
    late_pending_hook(&fixture);
    vacate(&fixture);
    assert!(clear_relaunch::commit(&captured, "explicit_clear").is_err());
    assert!(!watermark().exists());
    assert_launch_held();
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_codex_and_claude_clear_do_not_write_canary_retirement_pg() {
    let fixture = CanaryFixture::new().await;
    for (index, provider) in [ProviderKind::Codex, ProviderKind::Claude]
        .into_iter()
        .enumerate()
    {
        let channel = ChannelId::new(6_845_060 + index as u64);
        super::clear_persist_failure_tests::seed_session(&fixture.shared, channel).await;
        let name = fixture.shared.core.lock().await.sessions[&channel]
            .channel_name
            .clone()
            .unwrap();
        let tmux = provider.build_tmux_session_name(&name);
        let fake = FakeTmux::install(&tmux);
        fake.become_missing_after_kill();
        assert!(
            !clear(&fixture, &provider, channel.get())
                .await
                .end_unconfirmed
        );
        assert!(
            fake.take_calls()
                .iter()
                .any(|call| call.starts_with("kill-session"))
        );
        assert!(!watermark().exists());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn clear_of_dead_canary_with_stale_marker_reopens_launch_pg() {
    let fixture = CanaryFixture::new().await;
    let captured = clear_relaunch::capture(CANARY_TMUX).unwrap().unwrap();
    fixture.fake.mark_missing();
    let _ = crate::services::session_backend::remove_process_session(CANARY_TMUX);
    assert!(matches!(
        crate::services::tui_prompt_dedupe::binding_context::observe_spawn_nonce_marker(
            CANARY_TMUX
        ),
        SpawnNonceMarker::Known(_)
    ));
    assert!(
        !clear(&fixture, &ProviderKind::Codex, CANARY_CHANNEL)
            .await
            .end_unconfirmed
    );
    assert_eq!(
        crate::services::tui_prompt_dedupe::binding_context::observe_spawn_nonce_marker(
            CANARY_TMUX
        ),
        SpawnNonceMarker::Absent,
    );
    let retired: Value = serde_json::from_slice(&fs::read(watermark()).unwrap()).unwrap();
    assert_eq!(retired["retired_through_seq"], captured.seq);
    assert_eq!(retired["nonce"], captured.nonce);
    assert_eq!(retired["origin"], "explicit_clear");
    assert!(clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(dedupe::codex_verified_requires_proof(CANARY_TMUX));
    assert!(dedupe::codex_verified_input_blocked(CANARY_TMUX));
}

#[tokio::test(flavor = "current_thread")]
async fn clear_during_inflight_canary_launch_still_ends_pane_pg() {
    let fixture = CanaryFixture::new().await;
    let old_history = fs::read(fixture.history()).unwrap();
    crate::services::tmux_common::cleanup_session_temp_files(CANARY_TMUX);
    let mut next = fixture.context.clone();
    next.created_at = chrono::Utc::now();
    next.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    let prepared = PreparedIncarnation::create(next).unwrap();
    assert!(
        !clear(&fixture, &ProviderKind::Codex, CANARY_CHANNEL)
            .await
            .end_unconfirmed
    );
    assert!(
        fixture
            .fake
            .take_calls()
            .iter()
            .any(|call| call.starts_with("kill-session"))
    );
    assert!(crate::services::session_backend::process_session_pid(CANARY_TMUX).is_none());
    assert!(
        prepared.path.is_file(),
        "immutable launch evidence is retained"
    );
    assert_eq!(fs::read(fixture.history()).unwrap(), old_history);
    assert!(clear_relaunch::relaunch_open(CANARY_TMUX));
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_codex_turn_mode_unknown_keeps_existing_and_new_input_queued_pg() {
    use crate::services::discord::turn_presence::activity::{self, Activity};
    use crate::services::tui_o::turn_mode::TestConfirmation;
    use crate::services::turn_orchestrator::QueueIdentity;

    let fixture = CanaryFixture::new().await;
    for (index, existing) in [true, false].into_iter().enumerate() {
        let channel = ChannelId::new(6_845_070 + index as u64);
        super::clear_persist_failure_tests::seed_session(&fixture.shared, channel).await;
        let name = format!("e51-legacy-unknown-{index}");
        {
            let mut core = fixture.shared.core.lock().await;
            let session = core.sessions.get_mut(&channel).unwrap();
            session.channel_name = Some(name.clone());
            if !existing {
                session.session_id = None;
            }
        }
        let tmux = ProviderKind::Codex.build_tmux_session_name(&name);
        let fake = FakeTmux::install(&tmux);
        fake.become_missing_after_kill();
        fake.fail_probes();
        let _turn_mode = TestConfirmation::new(channel.get());
        assert!(!dedupe::codex_verified_requires_proof(&tmux));
        assert!(!clear_relaunch::relaunch_open(&tmux));
        super::clear_persist_failure_tests::seed_backlog(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
        )
        .await;
        let before = fixture.shared.mailbox(channel).snapshot().await;
        let identity = QueueIdentity::of(&before.intervention_queue[0]);
        let observed = activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel).await;
        assert_eq!(observed.activity, Activity::Unknown, "{observed:?}");
        let taken = crate::services::discord::mailbox_take_next_automatic_intervention(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
        )
        .await;
        assert!(taken.intervention.is_none(), "{observed:?}");
        assert!(taken.dispatch_lease.is_none());
        let after = fixture.shared.mailbox(channel).snapshot().await;
        assert_eq!(after.intervention_queue.len(), 1);
        assert_eq!(QueueIdentity::of(&after.intervention_queue[0]), identity);
        assert!(after.pending_user_dispatch.is_none());
        assert!(!fake.take_calls().iter().any(|call| {
            call.starts_with("new-session")
                || call.starts_with("send-keys")
                || call.starts_with("load-buffer")
        }));
    }

    // An unretired canary, another channel while the canary is retired, and a retired but
    // Herdr-hosted canary all stay held: every part of the launch exception is required.
    vacate(&fixture);
    fixture
        .fake
        .add_unwatched_inventory("AgentDesk-codex-external-e51-unwatched");
    let canary = ChannelId::new(CANARY_CHANNEL);
    fixture.shared.core.lock().await.sessions.remove(&canary);
    for (channel, herdr) in [
        (canary, false),
        (ChannelId::new(6_845_072), false),
        (canary, true),
    ] {
        let _turn_mode = (channel != canary).then(|| TestConfirmation::new(channel.get()));
        // Live and on-disk config carry the same Herdr section, so activity reads the hosted scope.
        let _hosts = herdr.then(|| {
            let forced = crate::config::session_hosts::force_for_test(
                Some("test-local"),
                &[(CANARY_CHANNEL, "test-local")],
            );
            let mut config = (*crate::config_live_reload::current().unwrap()).clone();
            config.session_hosts =
                crate::config::session_hosts::with_boot(|boot| boot.unwrap().config().clone());
            config.cluster.instance_id = Some("test-local".into());
            let disk = crate::runtime_layout::config_file_path(fixture.root.path());
            fs::write(disk, serde_yaml::to_string(&config).unwrap()).unwrap();
            crate::config_live_reload::install(config);
            forced
        });
        if channel != canary {
            let captured = clear_relaunch::capture(CANARY_TMUX).unwrap().unwrap();
            clear_relaunch::commit(&captured, "explicit_clear").unwrap();
            assert!(clear_relaunch::relaunch_open(CANARY_TMUX));
        } else {
            assert_eq!(clear_relaunch::relaunch_open(CANARY_TMUX), herdr);
        }
        if herdr {
            assert!(crate::config::session_hosts::herdr_endpoint(CANARY_CHANNEL).is_some());
        } else {
            super::clear_persist_failure_tests::seed_backlog(
                &fixture.shared,
                &ProviderKind::Codex,
                channel,
            )
            .await;
        }
        let before = fixture.shared.mailbox(channel).snapshot().await;
        let identity = QueueIdentity::of(&before.intervention_queue[0]);
        let observed = activity::activity_now(&fixture.shared, &ProviderKind::Codex, channel).await;
        assert_eq!(
            (observed.activity, observed.reason),
            (Activity::Unknown, "session_identity_unresolved")
        );
        let taken = crate::services::discord::mailbox_take_next_automatic_intervention(
            &fixture.shared,
            &ProviderKind::Codex,
            channel,
        )
        .await;
        assert!(
            taken.intervention.is_none(),
            "channel={channel} retirement_open={}",
            clear_relaunch::relaunch_open(CANARY_TMUX)
        );
        let after = fixture.shared.mailbox(channel).snapshot().await;
        assert_eq!(after.intervention_queue.len(), 1);
        assert_eq!(QueueIdentity::of(&after.intervention_queue[0]), identity);
        assert!(after.pending_user_dispatch.is_none());
    }
}

/// A Discord API stand-in that names the canary channel, so intake and the clear stay offline.
async fn canary_api(
    fixture: &mut CanaryFixture,
) -> crate::services::discord::admin_host_guard::tests::Recorder {
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start_with(Arc::new(
        |method, path| {
            (method == axum::http::Method::GET
                && path.ends_with(&format!("/channels/{CANARY_CHANNEL}")))
            .then(|| {
                json!({
                    "id": CANARY_CHANNEL.to_string(), "type": 0,
                    "name": "adk-codex-tui-e2e", "guild_id": "42000",
                    "position": 0, "permission_overwrites": [], "nsfw": false,
                    "parent_id": null
                })
            })
        },
    ))
    .await;
    crate::services::discord::internal_api::init(api.port, None);
    Arc::get_mut(&mut fixture.shared).unwrap().api_port = api.port;
    api
}

fn isolated(test: &str) -> bool {
    crate::services::tui_o::cutover::test_override::in_empty_list_process(test)
        && crate::services::discord::admin_host_guard::tests::api_child(
            test.split_once("::").unwrap().1,
        )
}

#[tokio::test(flavor = "current_thread")]
async fn unretired_pin_in_shadow_mode_parks_restored_input_until_clear_pg() {
    let test = concat!(
        module_path!(),
        "::unretired_pin_in_shadow_mode_parks_restored_input_until_clear_pg"
    );
    if isolated(test) {
        parked_until_clear(crate::services::codex::CodexSourceMode::Shadow).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unretired_pin_with_an_old_boundary_parks_restored_input_until_clear_pg() {
    let test = concat!(
        module_path!(),
        "::unretired_pin_with_an_old_boundary_parks_restored_input_until_clear_pg"
    );
    if isolated(test) {
        parked_until_clear(crate::services::codex::CodexSourceMode::Verified).await;
    }
}

/// A dead pinned canary with no core session and no clear since: the restored input is parked,
/// never taken into a launch that would wait on the pin, and the next clear discards and reopens.
async fn parked_until_clear(mode: crate::services::codex::CodexSourceMode) {
    use crate::services::turn_orchestrator::QueueIdentity;
    let mut fixture = CanaryFixture::new_in(mode).await;
    let api = canary_api(&mut fixture).await;
    let _hook = crate::services::claude_tui::hook_server::publish_hook_endpoint(format!(
        "http://127.0.0.1:{}",
        api.port
    ));
    let _cli = fixture.cold_cli();
    vacate(&fixture);
    let channel = ChannelId::new(CANARY_CHANNEL);
    fixture.shared.core.lock().await.sessions.remove(&channel);
    // An earlier clear's boundary is no explicit clear of this incarnation.
    let ended =
        crate::services::tui_prompt_dedupe::binding_events::binding_events_since(CANARY_CHANNEL, 0)
            .unwrap()
            .last()
            .unwrap()
            .committed_at;
    sqlx::query(
        "INSERT INTO channel_session_clear_boundaries (channel_id, cleared_at, clear_generation)
         VALUES ($1, $2, 1)",
    )
    .bind(CANARY_CHANNEL.to_string())
    .bind(ended + chrono::Duration::seconds(1))
    .execute(fixture.shared.pg_pool.as_ref().unwrap())
    .await
    .unwrap();
    super::clear_persist_failure_tests::seed_backlog(
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
    )
    .await;
    let before = fixture.shared.mailbox(channel).snapshot().await;
    let identity = QueueIdentity::of(&before.intervention_queue[0]);
    api.take();
    let deps = crate::services::discord::router::IntakeDeps {
        http: &api.http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared: &fixture.shared,
        token: "e51-isolated-test-bot",
    };
    let kick = crate::services::discord::queue_dispatch::kickoff::kickoff_idle_queues_with_deps;
    let started = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        kick(&deps, &ProviderKind::Codex),
    )
    .await
    .expect("a parked kickoff returns at once");
    assert_eq!(started, 0, "the pinned canary must not launch");
    let after = fixture.shared.mailbox(channel).snapshot().await;
    assert_eq!(after.intervention_queue.len(), 1);
    assert_eq!(QueueIdentity::of(&after.intervention_queue[0]), identity);
    assert!(after.pending_user_dispatch.is_none() && after.cancel_token.is_none());
    let parked = fixture.shared.queue_park_ledger.project(
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        &after,
    );
    assert_eq!(parked.owner, Some("codex_verified"), "{parked:?}");
    assert!(
        !fixture
            .fake
            .take_calls()
            .iter()
            .any(|call| call.starts_with("new-session ")),
    );
    assert_launch_held();

    let report = clear_channel_session_state(
        &api.http,
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        "!clear",
        SoftClearNotifyMode::Suppress,
    )
    .await
    .unwrap();
    assert_eq!(
        report.reply().lines().nth(1),
        Some("대기 메시지 1건을 비웠어요.")
    );
    assert!(
        !report
            .failed
            .iter()
            .any(|what| what.starts_with("codex_retirement")),
        "{report:?}"
    );
    let retired: Value = serde_json::from_slice(&fs::read(watermark()).unwrap()).unwrap();
    assert_eq!(retired["origin"], "explicit_clear");
    assert!(clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(!clear_relaunch::launch_blocked(CANARY_TMUX));
    let readiness = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
    assert!(!readiness(&fixture.shared, &ProviderKind::Codex, channel).await);
}

/// A canary pane that outlived its core session is not vacant: the clear warns, writes no
/// retirement and leaves launch closed, in a non-verified process mode too.
#[tokio::test(flavor = "current_thread")]
async fn clear_of_a_live_sessionless_canary_warns_without_retiring_pg() {
    let test = concat!(
        module_path!(),
        "::clear_of_a_live_sessionless_canary_warns_without_retiring_pg"
    );
    if !isolated(test) {
        return;
    }
    let mut fixture = CanaryFixture::new_in(crate::services::codex::CodexSourceMode::Shadow).await;
    let api = canary_api(&mut fixture).await;
    let channel = ChannelId::new(CANARY_CHANNEL);
    fixture.shared.core.lock().await.sessions.remove(&channel);
    let report = clear_channel_session_state(
        &api.http,
        &fixture.shared,
        &ProviderKind::Codex,
        channel,
        "!clear",
        SoftClearNotifyMode::Suppress,
    )
    .await
    .unwrap();
    assert!(
        report.failed.contains(&"codex_retirement_commit"),
        "{report:?}"
    );
    assert!(report.warning().is_some());
    assert!(!watermark().exists());
    assert!(!clear_relaunch::relaunch_open(CANARY_TMUX));
    assert!(
        !fixture
            .fake
            .take_calls()
            .iter()
            .any(|call| call.starts_with("kill-session")),
        "the clear gains no kill over a pane its core session does not name"
    );
}
