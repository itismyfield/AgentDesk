use super::super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedRecord, load_hosted_execution_pg,
};
use crate::services::agent_protocol::{RuntimeHandoffKind, StreamMessage};
use crate::services::codex::actual_submit_tests as submit;
use crate::services::discord;
use crate::services::discord::admin_host_guard::tests::Recorder;
use crate::services::discord::health::{self, HealthRegistry};
use crate::services::discord::inflight::{self, InflightTurnIdentity, InflightTurnState};
use crate::services::discord::shared_state::test_rest;
use crate::services::discord::task_supervisor::{watcher_completion, watcher_spawn_observation};
use crate::services::discord::turn_bridge::{
    BridgeCompletionSignal, TurnBridgeContext, resume_pin_tests, spawn_turn_bridge,
};
use crate::services::discord::turn_presence::activity::{self, Activity};
use crate::services::provider::CancelToken;
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as p5};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const CHANNEL: u64 = 62_841_777;
const TMUX: &str = "AgentDesk-codex-replacement-regression";
const NATIVE: &str = "019e660d-4859-7522-9cee-8ba7c4e7c743";

fn managed_row(path: &Path, user: u64) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Codex,
        CHANNEL,
        Some("replacement-regression".into()),
        71,
        user,
        0,
        "resume prompt".into(),
        Some(NATIVE.into()),
        Some(TMUX.into()),
        Some(path.display().to_string()),
        None,
        0,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    row.turn_nonce = Some(format!("replacement-turn-{user}"));
    row
}

async fn until(mut ready: impl FnMut() -> bool, why: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{why}"));
}

fn install_cli(root: &Path) -> (PathBuf, Vec<crate::config::TestEnvVarGuard>) {
    let cli = root.join("codex-fixture");
    // The old exact-resume source already exists. Append a new completed turn only when the
    // actual launch script invokes the fixture CLI. No native/provider process is started.
    fs::write(&cli, r#"#!/bin/sh
if [ "${1-}" = --version ]; then echo 'codex-cli 0.157.1'; exit 0; fi
if [ "${1-} ${2-}" = 'resume --help' ]; then echo '--dangerously-bypass-hook-trust'; exit 0; fi
python3 - <<'PY'
import json, os, pathlib
context_path = pathlib.Path(os.environ['AGENTDESK_BINDING_CONTEXT'])
context = json.loads(context_path.read_text())
root = pathlib.Path(os.environ['CODEX_HOME'])
identity = '019e660d-4859-7522-9cee-8ba7c4e7c743'
path = root / 'sessions' / ('rollout-' + identity + '.jsonl')
rows = [
 {'type':'event_msg','payload':{'type':'task_started','turn_id':'replacement-turn'}},
 {'type':'response_item','payload':{'type':'message','id':'replacement-message','role':'assistant','content':[{'type':'output_text','text':'replacement completed'}]}},
 {'type':'event_msg','payload':{'type':'task_complete','turn_id':'replacement-turn','last_agent_message':'replacement completed'}},
 {'type':'event_msg','payload':{'type':'composer_ready','turn_id':'replacement-turn'}},
]
with path.open('a') as output:
 for row in rows: output.write(json.dumps(row) + '\n')
(root / 'fixture-created').write_text(context['execution_nonce'])
PY
"#).unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    (
        cli.clone(),
        vec![set("AGENTDESK_CODEX_PATH", &cli), set("CODEX_HOME", root)],
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReplacementCase {
    Success,
    CreateFailure,
    WatcherExitThenCreateFailure,
    CancelDuringGap,
    CrashAfterSpawn,
}

#[tokio::test(flavor = "current_thread")]
async fn codex_warm_replacement_preserves_paused_watcher_and_start_activity() {
    run_replacement_case(
        ReplacementCase::Success,
        concat!(
            module_path!(),
            "::codex_warm_replacement_preserves_paused_watcher_and_start_activity"
        ),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn codex_replacement_create_failure_releases_watcher_and_clears_row() {
    run_replacement_case(
        ReplacementCase::CreateFailure,
        concat!(
            module_path!(),
            "::codex_replacement_create_failure_releases_watcher_and_clears_row"
        ),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn codex_watcher_exit_preserves_row_until_create_failure_bridge_settles() {
    run_replacement_case(
        ReplacementCase::WatcherExitThenCreateFailure,
        concat!(
            module_path!(),
            "::codex_watcher_exit_preserves_row_until_create_failure_bridge_settles"
        ),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn codex_cancel_during_replacement_does_not_protect_dead_pane() {
    run_replacement_case(
        ReplacementCase::CancelDuringGap,
        concat!(
            module_path!(),
            "::codex_cancel_during_replacement_does_not_protect_dead_pane"
        ),
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn codex_true_crash_after_replacement_clears_row() {
    run_replacement_case(
        ReplacementCase::CrashAfterSpawn,
        concat!(
            module_path!(),
            "::codex_true_crash_after_replacement_clears_row"
        ),
    )
    .await;
}

async fn run_replacement_case(case: ReplacementCase, name: &str) {
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _live = crate::config_live_reload::test_live_guard(env_lock);
    let (root, _root_env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    let mut config: crate::config::Config = serde_yaml::from_str(&format!(
        "server: {{}}\nagents:\n  - id: replacement-fixture\n    name: replacement-fixture\n    provider: codex\n    channels:\n      codex: {{id: '{CHANNEL}', name: replacement-regression, runtime: tui}}\n"
    ))
    .unwrap();
    config.runtime.followup_prompt_ready_timeout_secs = Some(1);
    crate::config_live_reload::install(config);
    let set = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock;
    let _mode = set("AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE", "legacy".as_ref());
    let _warm = set("AGENTDESK_CODEX_TUI_WARM_FOLLOWUP", "1".as_ref());
    let _binding_root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_TEST_CODEX_BINDING_ROOT",
        root.path(),
    );
    dedupe::reset_state_for_tests();
    p5::set_test_root(Some(root.path()));

    let fake = FakeTmux::install(TMUX);
    fake.become_missing_after_kill();
    fake.enable_fixture_launch();
    // This is this test's PATH-first temporary fake, never an operational tmux location.
    let fake_dir = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .next()
        .unwrap();
    fs::write(fake_dir.join("ready"), "").unwrap();
    let (_cli, _cli_env) = install_cli(root.path());
    let sessions = root.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let native = sessions.join(format!("rollout-{NATIVE}.jsonl"));
    let stamp = chrono::Utc::now().to_rfc3339();
    let rows = [
        serde_json::json!({"type":"session_meta","timestamp":stamp,"payload":{
            "id":NATIVE,"timestamp":stamp,"cwd":root.path(),"source":"cli","originator":"codex_cli_rs"}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"prior"}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"prior"}}),
        serde_json::json!({"type":"event_msg","payload":{"type":"composer_ready","turn_id":"prior"}}),
    ];
    fs::write(
        &native,
        rows.iter().map(|r| format!("{r}\n")).collect::<String>(),
    )
    .unwrap();
    crate::services::discord::stamp_spawn_markers(TMUX, None).unwrap();
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        TMUX,
        RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    crate::services::tmux_common::write_tmux_session_auth_profile(TMUX, "default").unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        TMUX,
        &native,
        Some(NATIVE),
    )
    .unwrap();
    crate::services::codex_tui::warm_followup::write_codex_tui_launch_options_evidence(
        TMUX,
        &submit::options(NATIVE, root.path()),
    )
    .unwrap();
    dedupe::register_tmux_channel(TMUX, CHANNEL);
    dedupe::register_tmux_runtime_binding(
        TMUX,
        dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: native.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(NATIVE.into()),
            last_offset: fs::metadata(&native).unwrap().len(),
            relay_last_offset: None,
        },
    );

    let api = Recorder::start().await;
    let _database = TestPostgresDb::create().await;
    let pool = _database.connect_and_migrate().await;
    let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Codex;
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    {
        let mut settings = shared.settings.write().await;
        settings.provider = ProviderKind::Codex;
        settings.allowed_channel_ids = vec![CHANNEL];
    }
    crate::services::discord::host_defer_gate::tests::map_channel(
        &shared,
        ChannelId::new(CHANNEL),
        "replacement-regression",
    )
    .await;
    assert_eq!(
        ProviderKind::Codex.build_tmux_session_name("replacement-regression"),
        TMUX,
    );
    // A warm pane already has its durable local session row, separate from its turn row.
    let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &ProviderKind::Codex,
        TMUX,
    );
    let channel_text = CHANNEL.to_string();
    let session_identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: &shared.token_hash,
        channel_id: &channel_text,
    };
    let pool = shared.pg_pool.as_ref().unwrap();
    upsert_hook_session_with_identity_pg(
        pool,
        HookSessionUpsert {
            session_key: &session_key,
            instance_id: None,
            agent_id: None,
            provider: "codex",
            status: "idle",
            session_info: None,
            model: None,
            tokens: None,
            cwd: None,
            active_dispatch_id: None,
            thread_channel_id: None,
            channel_id: Some(&channel_text),
            claude_session_id: None,
            raw_provider_session_id: Some(NATIVE),
            turn_start_nonce: None,
            dispatched_origin: false,
        },
        Some(session_identity),
    )
    .await
    .unwrap();
    let exact = load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&session_key)).await;
    let canonical = load_hosted_execution_pg(
        pool,
        HostedLookupKey::Canonical {
            provider: "codex",
            identity: session_identity,
        },
    )
    .await;
    assert!(matches!(
        &exact,
        HostedLookup::Found(found) if found.record == HostedRecord::Legacy
    ));
    assert_eq!(
        exact, canonical,
        "both locators must name one legacy session row"
    );
    let registry = HealthRegistry::new();
    registry.register("codex".into(), shared.clone()).await;
    let confirmation = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let now = chrono::Utc::now();
    let event = p5::BindingEvent {
        seq: 1,
        channel_id: CHANNEL,
        provider: "codex".into(),
        tmux_session: TMUX.into(),
        execution_nonce: Some("legacy-execution".into()),
        old: None,
        new: p5::BindingTarget::Source(
            crate::services::tui_o::shadow::binding_reader::source_id_for(NATIVE, &native).unwrap(),
        ),
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: Some("SessionStart".into()),
            received_at: now,
        },
        committed_at: now,
    };
    let events = root.path().join(p5::BINDING_EVENTS_DIR);
    fs::create_dir_all(&events).unwrap();
    fs::write(
        events.join(format!("{CHANNEL}.log")),
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();
    activity::tests::companion_attach_log_watcher(&shared, CHANNEL, TMUX, &native);
    inflight::save_inflight_state(&managed_row(&native, 101)).unwrap();
    let channel = ChannelId::new(CHANNEL);
    let handle = shared.tmux_watchers.get(&channel).unwrap();
    handle.paused.store(true, Ordering::Release);
    let flags = (
        handle.cancel.clone(),
        handle.paused.clone(),
        handle.resume_offset.clone(),
        handle.pause_epoch.clone(),
        handle.turn_delivered.clone(),
        handle.last_heartbeat_ts_ms.clone(),
    );
    drop(handle);
    let observation = inflight::o_seed_observation::Guard::new(&ProviderKind::Codex, CHANNEL);
    let mut watcher = tokio::spawn(tmux_output_watcher_with_restore(
        channel,
        api.http.clone(),
        shared.clone(),
        native.display().to_string(),
        TMUX.into(),
        0,
        flags.0.clone(),
        flags.1.clone(),
        flags.2.clone(),
        flags.3.clone(),
        flags.4.clone(),
        flags.5.clone(),
        None,
    ));
    until(
        || observation.snapshot().event_count("host_probe") > 0,
        "paused watcher did not enter real loop",
    )
    .await;
    let initial = activity::tests::companion_settled_activity(&shared, CHANNEL).await;
    eprintln!("REPLACEMENT_INITIAL {initial:?}");
    assert_eq!(initial.activity, Activity::Idle);
    // Reproduce adoption of the newer managed row by the incumbent's actual prologue.
    let token = Arc::new(CancelToken::new());
    let mut current = managed_row(&native, 102);
    current.turn_nonce = token.turn_nonce().map(str::to_owned);
    inflight::save_inflight_state(&current).unwrap();
    if case == ReplacementCase::WatcherExitThenCreateFailure {
        assert!(
            discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                serenity::UserId::new(current.request_owner_user_id),
                MessageId::new(current.user_msg_id),
            )
            .await
        );
        discord::increment_global_active(&shared, "replacement_failure_admission");
        discord::increment_global_active(&shared, "other_channel_sentinel");
    }
    let (gap_tx, mut gap_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let driver_token = token.clone();
    let cwd = root.path().to_owned();
    let (messages_tx, messages_rx) = mpsc::channel();
    let driver = tokio::task::spawn_blocking(move || {
        submit::before_create_effect(Box::new(move || {
            gap_tx.send(()).unwrap();
            release_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("replacement gap was not released");
        }));
        submit::cold_driver(&cwd, NATIVE, (TMUX, CHANNEL), driver_token, messages_tx)
    });
    tokio::time::timeout(Duration::from_secs(10), &mut gap_rx)
        .await
        .expect("actual warm fallback never reached create boundary")
        .unwrap();
    assert!(
        !crate::services::platform::tmux::has_session(TMUX),
        "real cleanup must remove the fake old session before create"
    );
    let at_gap = observation.snapshot().event_count("host_probe");
    until(
        || watcher.is_finished() || observation.snapshot().event_count("host_probe") >= at_gap + 2,
        "watcher did not observe the create gap",
    )
    .await;
    let gap_row =
        inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL).map(|r| r.user_msg_id);
    let gap_same_watcher = shared
        .tmux_watchers
        .get(&channel)
        .is_some_and(|h| Arc::ptr_eq(&h.cancel, &flags.0));
    let gap_exit_count = observation.snapshot().event_count("post_stream_exit");
    let gap_activity = activity::tests::companion_settled_activity(&shared, CHANNEL).await;
    let gap_hold =
        health::external_turn_hold_for_start(Some(&registry), &ProviderKind::Codex, CHANNEL).await;
    eprintln!(
        "REPLACEMENT_GAP row={gap_row:?} same_watcher={gap_same_watcher} exits={gap_exit_count} activity={gap_activity:?} hold={gap_hold:?} reason={:?}",
        gap_hold.map(|h| h.reason())
    );

    let mut watcher_joined = false;
    if case == ReplacementCase::WatcherExitThenCreateFailure {
        cancel_incumbent_in_replacement_for_test(
            &shared,
            &current,
            &token,
            &flags.0,
            &mut watcher,
            &observation,
        )
        .await;
        watcher_joined = true;
    }
    // Observe cancellation while the provider is still parked with its guard alive.
    let mut cancelled_cleanup = None;
    if case == ReplacementCase::CancelDuringGap {
        token.publish_cancel("replacement fixture user stop");
        until(
            || watcher.is_finished(),
            "cancelled replacement suppressed real pane death",
        )
        .await;
        cancelled_cleanup = Some((
            inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL).is_none(),
            shared.tmux_watchers.get(&channel).is_none(),
            observation.snapshot().event_count("post_stream_exit"),
        ));
    }
    if matches!(
        case,
        ReplacementCase::CreateFailure
            | ReplacementCase::WatcherExitThenCreateFailure
            | ReplacementCase::CancelDuringGap
    ) {
        // The actual new-session command now fails. In the cancel case this only ends the
        // parked fixture provider, after the real watcher cleanup was already observed.
        fs::remove_file(fake_dir.join("launch_enabled")).unwrap();
    } else {
        fake.enable_fixture_launch();
    }
    release_tx.send(()).unwrap();
    let mut ready = false;
    let mut ready_handoff = None;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            for message in messages_rx.try_iter() {
                if let StreamMessage::RuntimeReady { handoff } = message {
                    ready = true;
                    ready_handoff = Some(handoff);
                }
            }
            if ready || driver.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual replacement neither returned nor published RuntimeReady");
    let created = root.path().join("fixture-created").exists();
    if ready {
        // Idle relay recovery may restore ownership without creating a watcher claim.
        let had_handle = shared.tmux_watchers.get(&channel).is_some();
        let restored = shared
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(TMUX, channel);
        assert_eq!(
            shared.tmux_watchers.owner_channel_for_tmux_session(TMUX),
            Some(channel),
        );
        if !had_handle {
            assert!(shared.tmux_watchers.get(&channel).is_none());
            assert!(shared.tmux_watchers.channel_binding(&channel).is_none());
        }
        eprintln!("REPLACEMENT_OWNER restored={restored} had_handle={had_handle}");
    }
    if case == ReplacementCase::CrashAfterSpawn {
        // finish_spawn and post-tail RuntimeReady already happened; this is a later real death.
        assert!(
            created && ready,
            "crash control must pass actual replacement completion first"
        );
        fake.mark_missing();
    }
    // Observe the actual RuntimeReady dispatch's successor submission, not an inferred slot.
    let submitted = watcher_spawn_observation::Scope::new(&shared);
    let mut adopted_successor = None;
    if case == ReplacementCase::Success && gap_row.is_some() {
        let mut row = inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL)
            .expect("replacement row survives until actual RuntimeReady");
        let pin = crate::services::discord::turn_bridge::adopt_codex_replacement_for_test(
            &shared,
            &mut row,
            ready_handoff
                .take()
                .expect("actual driver RuntimeReady frame"),
            &flags.0,
            api.http.clone(),
        )
        .await;
        assert_eq!(
            submitted.submissions(),
            1,
            "actual RuntimeReady submits one successor watcher"
        );
        let completion = watcher_completion::observe(&pin.cancel)
            .expect("the actual successor spawn registered completion");
        tokio::time::timeout(Duration::from_secs(10), &mut watcher)
            .await
            .expect("actual old watcher did not exit after the claim")
            .unwrap();
        watcher_joined = true;
        assert_eq!(observation.snapshot().event_count("post_stream_exit"), 1);
        assert_same_managed_episode(&row, &token);
        assert!(
            pin.adopt_if_current(&shared.tmux_watchers, |_| ())
                .is_some()
        );
        let registered = shared
            .tmux_watchers
            .get(&channel)
            .expect("old exit kept actual successor");
        assert!(Arc::ptr_eq(&registered.cancel, &pin.cancel));
        assert!(!registered.cancel.load(Ordering::Acquire));
        drop(registered);
        until(
            || observation.snapshot().event_count("watcher_initialized") == 2,
            "the actual successor did not enter its watcher loop",
        )
        .await;
        adopted_successor = Some((pin, completion));
    }
    let terminal_cleanup = if !matches!(
        case,
        ReplacementCase::Success | ReplacementCase::WatcherExitThenCreateFailure
    ) {
        until(
            || watcher.is_finished(),
            "real failure left the paused watcher attached",
        )
        .await;
        Some((
            inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL).is_none(),
            shared.tmux_watchers.get(&channel).is_none(),
            observation.snapshot().event_count("post_stream_exit"),
        ))
    } else {
        None
    };
    let after_activity = activity::tests::companion_settled_activity(&shared, CHANNEL).await;
    let conflict = crate::server::routes::codex_replacement_start_conflict_for_test(
        Some(&registry),
        &ProviderKind::Codex,
        CHANNEL,
    )
    .await;
    eprintln!("REPLACEMENT_START_RESPONSE activity={after_activity:?} conflict={conflict:?}");
    // End only the fake launch; keep the tested registry snapshot captured above.
    if case != ReplacementCase::WatcherExitThenCreateFailure {
        token.publish_cancel("replacement fixture complete");
    }
    let driver_result = tokio::time::timeout(Duration::from_secs(15), driver)
        .await
        .expect("driver did not stop")
        .unwrap();
    if case == ReplacementCase::WatcherExitThenCreateFailure {
        settle_codex_create_failure_via_bridge_for_test(
            &shared,
            &current,
            &token,
            driver_result
                .as_ref()
                .expect_err("actual create must fail")
                .clone(),
            api.http.clone(),
        )
        .await;
    }
    if !watcher_joined {
        flags.0.store(true, Ordering::Release);
        tokio::time::timeout(Duration::from_secs(10), &mut watcher)
            .await
            .expect("watcher did not stop")
            .unwrap();
    }
    if let Some((pin, completion)) = adopted_successor {
        assert!(
            pin.adopt_if_current(&shared.tmux_watchers, |_| ())
                .is_some(),
            "actual successor must still own its live claim before fixture cleanup"
        );
        assert_same_managed_episode(&current, &token);
        let finished = submitted.cancel_and_join_all().await;
        assert_eq!(finished.len(), 1);
        assert!(
            finished
                .iter()
                .all(|(_, outcome)| *outcome == watcher_completion::Outcome::Returned),
            "{finished:?}"
        );
        assert_eq!(
            completion.wait().await,
            watcher_completion::Outcome::Returned
        );
        assert_eq!(observation.snapshot().event_count("post_stream_exit"), 2);
        assert!(!shared.tmux_watchers.contains_key(&channel));
        assert_same_managed_episode(&current, &token);
    }
    let calls = fake.take_calls();
    eprintln!(
        "REPLACEMENT_CREATED created={created} ready={ready} driver={driver_result:?} calls={calls:?}"
    );
    p5::forget_channel_for_tests(CHANNEL);
    p5::set_test_root(None);
    dedupe::reset_state_for_tests();
    drop(confirmation);

    assert_eq!(
        gap_row,
        Some(102),
        "paused incumbent deleted the replacement turn's managed row"
    );
    assert!(
        gap_same_watcher,
        "paused incumbent released its registry slot during same-name replacement"
    );
    assert_eq!(
        gap_exit_count, 0,
        "a same-name replacement is not terminal pane death"
    );
    assert_eq!(
        gap_activity.activity,
        Activity::Idle,
        "replacement falsely turned a settled source Unknown"
    );
    assert_eq!(
        gap_hold, None,
        "turn/start returned turn_activity_unknown after the replacement gap"
    );
    match case {
        ReplacementCase::Success => {
            assert!(
                conflict.is_none(),
                "actual turn/start returned {conflict:?}"
            );
            assert!(
                created && ready,
                "must traverse actual create/finish_spawn/tail RuntimeReady"
            );
            assert!(
                driver_result.is_ok(),
                "actual driver failed: {driver_result:?}"
            );
        }
        ReplacementCase::CreateFailure => {
            let error = driver_result.expect_err("actual create must fail");
            assert!(
                error.contains("tmux error:"),
                "failed before the create effect: {error}"
            );
            assert!(
                !created && !ready,
                "failed create cannot publish successor readiness"
            );
            assert!(
                calls.iter().any(|call| call.starts_with("new-session ")),
                "no actual create attempt: {calls:?}"
            );
            assert_eq!(
                terminal_cleanup,
                Some((true, true, 1)),
                "create failure must take actual watcher cleanup"
            );
        }
        ReplacementCase::WatcherExitThenCreateFailure => {
            let error = driver_result.expect_err("actual create must fail after watcher exit");
            assert!(
                error.contains("tmux error:"),
                "failed before the create effect: {error}"
            );
            assert!(
                !created && !ready,
                "failed create cannot publish successor readiness"
            );
            assert!(
                calls.iter().any(|call| call.starts_with("new-session ")),
                "no actual create attempt: {calls:?}"
            );
            assert!(
                watcher_joined,
                "old watcher was joined while Replacement was still active"
            );
            assert!(inflight::load_inflight_state(&ProviderKind::Codex, CHANNEL).is_none());
        }
        ReplacementCase::CancelDuringGap => {
            assert_eq!(
                cancelled_cleanup,
                Some((true, true, 1)),
                "cancel must take cleanup before the provider guard drops"
            );
            assert!(
                !created && !ready,
                "the stopped fixture must not publish readiness"
            );
        }
        ReplacementCase::CrashAfterSpawn => {
            assert!(
                created && ready,
                "crash must follow a successful replacement"
            );
            assert_eq!(
                terminal_cleanup,
                Some((true, true, 1)),
                "completed replacement cannot protect a later real crash"
            );
            assert!(
                driver_result.is_ok(),
                "completed provider failed: {driver_result:?}"
            );
        }
    }
}

#[tokio::test]
async fn codex_host_gate_rechecks_a_replacement_completed_inside_the_probe() {
    use crate::services::discord::codex_absence::replacement::Replacement;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let token = Arc::new(CancelToken::new());
    crate::services::discord::save_actual_episode_for_tests(CHANNEL, TMUX, 30, &token);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let host = HostSnapshot::new(WatchHost::Legacy);
    let observed =
        super::host_gate::host_alive(&shared, TMUX, ChannelId::new(CHANNEL), &host, async {
            let replacement = Replacement::begin(Some(CHANNEL), TMUX, Some(&token))
                .unwrap()
                .unwrap();
            tokio::task::yield_now().await;
            replacement.complete();
            false
        })
        .await;
    assert!(
        observed,
        "a negative probe spanning kill/create must be retried"
    );
    assert!(
        !super::host_gate::host_alive(&shared, TMUX, ChannelId::new(CHANNEL), &host, async {
            false
        },)
        .await,
        "a subsequent negative probe is a real death"
    );
}

fn assert_same_managed_episode(current: &InflightTurnState, token: &Arc<CancelToken>) {
    assert_eq!(current.provider_kind(), Some(ProviderKind::Codex));
    assert_eq!(current.turn_source, inflight::TurnSource::Managed);
    assert!(current.user_msg_id > 0);
    assert_eq!(current.turn_nonce.as_deref(), token.turn_nonce());
    let durable = inflight::load_inflight_state(&ProviderKind::Codex, current.channel_id)
        .expect("this managed row must survive until the bridge settles the real failure");
    assert!(InflightTurnIdentity::from_state(current).matches_state(&durable));
    assert_eq!(durable.turn_nonce, current.turn_nonce);
}

async fn assert_mailbox_still_owned(
    shared: &Arc<SharedData>,
    current: &InflightTurnState,
    token: &Arc<CancelToken>,
) {
    let snapshot = discord::mailbox_snapshot(shared, ChannelId::new(current.channel_id)).await;
    assert!(
        snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(held, token))
    );
    assert_eq!(
        snapshot.active_user_message_id,
        Some(MessageId::new(current.user_msg_id))
    );
}

/// The actual provider is parked at before_create_effect with its replacement alive.
/// Stop only the incumbent; the provider token remains uncancelled.

async fn cancel_incumbent_in_replacement_for_test(
    shared: &Arc<SharedData>,
    current: &InflightTurnState,
    token: &Arc<CancelToken>,
    watcher_cancel: &Arc<AtomicBool>,
    watcher: &mut tokio::task::JoinHandle<()>,
    observation: &inflight::o_seed_observation::Guard,
) {
    assert!(!token.cancelled.load(Ordering::Acquire));
    assert_same_managed_episode(current, token);
    assert_mailbox_still_owned(shared, current, token).await;
    let exit_before = observation.snapshot().event_count("post_stream_exit");
    assert_eq!(
        exit_before, 0,
        "the real incumbent must still be running at the create gap"
    );
    watcher_cancel.store(true, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(10), watcher)
        .await
        .expect("watcher-only cancellation did not finish")
        .expect("actual incumbent task panicked");
    assert_eq!(
        observation.snapshot().event_count("post_stream_exit"),
        exit_before + 1
    );
    assert!(
        !shared
            .tmux_watchers
            .contains_key(&ChannelId::new(current.channel_id)),
        "post_stream_exit must release the cancelled incumbent's own registry slot"
    );
    assert!(
        !token.cancelled.load(Ordering::Acquire),
        "watcher cancellation cannot cancel the provider"
    );
    assert_same_managed_episode(current, token);
    assert_mailbox_still_owned(shared, current, token).await;
}

fn bridge_context(
    row: InflightTurnState,
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
) -> TurnBridgeContext {
    TurnBridgeContext {
        provider: ProviderKind::Codex,
        // Use the fixture's direct REST transport; its PG outbox has no delivery worker.
        gateway: Arc::new(discord::gateway::DiscordGateway::new(
            http,
            shared,
            ProviderKind::Codex,
            None,
        )),
        channel_id: ChannelId::new(row.channel_id),
        user_msg_id: Some(MessageId::new(row.user_msg_id)),
        user_text_owned: row.user_text.clone(),
        request_owner_name: String::new(),
        role_binding: None,
        adk_session_key: None,
        adk_session_name: None,
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: Default::default(),
        context_window_tokens: 0,
        context_compact_percent: 0,
        current_msg_id: (row.current_msg_id > 0).then(|| MessageId::new(row.current_msg_id)),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: None,
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: None,
        is_external_input_tui_direct: false,
        inflight_state: row,
    }
}

/// Removes unconsumed probe slots on assertion failure. A consumed capture's release
/// sender belongs to the function below and is dropped on unwind, so no task waits forever.
struct BridgeProbeGuard(ChannelId);
impl Drop for BridgeProbeGuard {
    fn drop(&mut self) {
        let mut captured = resume_pin_tests::BRIDGE_CAPTURE_PROBE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if captured.as_ref().is_some_and(|probe| probe.0 == self.0) {
            captured.take();
        }
        let mut completed = resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if completed.as_ref().is_some_and(|probe| probe.0 == self.0) {
            completed.take();
        }
    }
}

/// Settle the actual create error after joining the old watcher.
/// Only the real bridge can now remove the retained managed row.

async fn settle_codex_create_failure_via_bridge_for_test(
    shared: &Arc<SharedData>,
    current: &InflightTurnState,
    token: &Arc<CancelToken>,
    error: String,
    http: Arc<serenity::Http>,
) {
    let channel = ChannelId::new(current.channel_id);
    assert!(
        error.contains("tmux error:"),
        "expected actual create failure: {error}"
    );
    assert!(!token.cancelled.load(Ordering::Acquire));
    assert!(!shared.tmux_watchers.contains_key(&channel));
    assert_same_managed_episode(current, token);
    assert_mailbox_still_owned(shared, current, token).await;
    let active_before = shared.restart.global_active.load(Ordering::Relaxed);
    assert!(
        active_before >= 2,
        "admit this turn plus a sentinel before invoking cold_driver"
    );
    let queued_before = discord::mailbox_snapshot(shared, channel)
        .await
        .intervention_queue
        .len();

    let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let (joined_tx, joined_rx) = tokio::sync::oneshot::channel();
    let _probes = BridgeProbeGuard(channel);
    {
        let mut probe = resume_pin_tests::BRIDGE_CAPTURE_PROBE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            probe.is_none(),
            "another test owns the existing bridge capture probe"
        );
        *probe = Some((channel, captured_tx, resume_rx));
    }
    {
        let mut probe = resume_pin_tests::BRIDGE_COMPLETION_PROBE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        assert!(
            probe.is_none(),
            "another test owns the existing bridge task probe"
        );
        *probe = Some((channel, joined_tx));
    }
    let _rest = test_rest::install(http.clone());
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    let mut bridge = bridge_context(current.clone(), shared.clone(), http);
    bridge.completion_tx = Some(completed_tx);
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_turn_bridge(shared.clone(), token.clone(), rx, bridge);
    // This is intake's actual Err -> Error conversion, not an invented Done frame.
    tx.send(
        discord::router::message_handler::producer_error_frame(current.channel_id, error)
            .expect("an ordinary Codex create failure must reach the bridge"),
    )
    .unwrap();
    drop(tx);
    tokio::time::timeout(Duration::from_secs(10), captured_rx)
        .await
        .expect("real bridge did not capture its clear fence")
        .unwrap();
    assert_same_managed_episode(current, token);
    assert_mailbox_still_owned(shared, current, token).await;
    assert!(!token.cancelled.load(Ordering::Acquire));
    resume_tx.send(()).unwrap();
    let signal = tokio::time::timeout(Duration::from_secs(20), completed_rx)
        .await
        .expect("bridge did not report completion")
        .unwrap();
    assert_ne!(
        signal,
        BridgeCompletionSignal::EntryAborted,
        "must enter real bridge authority"
    );
    tokio::time::timeout(Duration::from_secs(10), joined_rx)
        .await
        .expect("the actual bridge task did not join")
        .unwrap();
    // The completion signal alone does not join terminal settlement. Wait for its
    // actual mailbox release and active-count decrement after the bridge task joins.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mailbox = discord::mailbox_snapshot(shared, channel).await;
            if mailbox.cancel_token.is_none()
                && mailbox.active_user_message_id.is_none()
                && shared.restart.global_active.load(Ordering::Relaxed) == active_before - 1
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("real bridge finalizer did not release exactly its mailbox episode");
    assert!(
        inflight::load_inflight_state(&ProviderKind::Codex, current.channel_id).is_none(),
        "the bridge's captured-episode CAS must remove the row retained by old watcher exit"
    );
    assert!(!shared.tmux_watchers.contains_key(&channel));
    assert!(
        token.cancelled.load(Ordering::Acquire),
        "finalizer settles this provider token after failure"
    );
    assert_eq!(
        discord::mailbox_snapshot(shared, channel)
            .await
            .intervention_queue
            .len(),
        queued_before
    );
}
