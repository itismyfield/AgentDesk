use super::*;
use crate::services::discord::admin_host_guard::tests::Recorder;
use crate::services::discord::inflight::{self, InflightTurnIdentity};
use crate::services::discord::queue_dispatch::kickoff::install_owner_race_pause;
use crate::services::discord::shared_state::test_rest;
use crate::services::discord::task_supervisor::{
    self, watcher_completion, watcher_spawn_observation,
};
use crate::services::discord::turn_presence::activity;
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The actual RuntimeReady dispatch, using the fixture REST client and gateway seam. It must
/// replace this exact paused incumbent and publish its successor's actual adopted claim pin.
pub(in crate::services::discord) async fn adopt_codex_replacement_for_test(
    shared: &Arc<SharedData>,
    state: &mut InflightTurnState,
    handoff: RuntimeHandoff,
    incumbent_cancel: &Arc<AtomicBool>,
    http: Arc<serenity::Http>,
) -> WatcherClaimIncarnation {
    let channel = ChannelId::new(state.channel_id);
    let RuntimeHandoff::CodexTui {
        ref tmux_session_name,
        ref rollout_path,
        ..
    } = handoff
    else {
        panic!("fixture must use the actual Codex RuntimeReady frame");
    };
    let (tmux, output) = (tmux_session_name.clone(), rollout_path.clone());
    let old = shared
        .tmux_watchers
        .get(&channel)
        .expect("preserved paused incumbent");
    assert!(Arc::ptr_eq(&old.cancel, incumbent_cancel));
    assert!(old.paused.load(Ordering::Acquire));
    assert!(!old.cancel.load(Ordering::Acquire));
    let old_delivery = old.turn_delivered.clone();
    drop(old);
    let before = load_inflight_state(&ProviderKind::Codex, state.channel_id)
        .expect("same managed row exists before RuntimeReady");
    assert_eq!(before.user_msg_id, state.user_msg_id);
    assert_eq!(before.turn_nonce, state.turn_nonce);

    let _rest = test_rest::install(http);
    let _gateway = super::super::test_gateway::connect();
    let mut dirty = false;
    let observed = dispatch_process_handoff(
        shared,
        &ProviderKind::Codex,
        state,
        RuntimeHandoffLoopMessage::RuntimeReady { handoff },
        &mut dirty,
        false,
    )
    .await;
    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::Saved));
    assert!(observed.retry_message.is_none());
    assert_eq!(observed.claim_outcome, WatcherHandoffClaimOutcome::Spawned);
    assert!(observed.terminal_control_ready_observed);
    assert!(observed.tmux_handed_off && observed.watcher_relay_available);
    assert_eq!(observed.watcher_owner_channel_id, channel);
    assert_eq!(observed.watcher_slots, 1);
    let pin = observed
        .watcher_delivery_pin
        .expect("RuntimeReady adopted its successor incarnation");
    assert!(!Arc::ptr_eq(&pin.cancel, incumbent_cancel));
    assert!(!Arc::ptr_eq(&pin.turn_delivered, &old_delivery));
    assert!(
        incumbent_cancel.load(Ordering::Acquire),
        "actual claim cancelled the old watcher"
    );
    assert!(!pin.cancel.load(Ordering::Acquire));
    assert!(
        !pin.paused.load(Ordering::Acquire),
        "actual adoption unpaused the successor"
    );
    let successor = shared
        .tmux_watchers
        .get(&channel)
        .expect("adopted successor registered");
    assert_eq!(successor.tmux_session_name, tmux);
    assert_eq!(successor.output_path, output);
    assert!(Arc::ptr_eq(&successor.cancel, &pin.cancel));
    assert!(Arc::ptr_eq(&successor.turn_delivered, &pin.turn_delivered));
    drop(successor);
    let after = load_inflight_state(&ProviderKind::Codex, state.channel_id)
        .expect("runtime stamp preserved the managed turn");
    assert_eq!(after.user_msg_id, before.user_msg_id);
    assert_eq!(after.turn_nonce, before.turn_nonce);
    assert_eq!(after.runtime_kind, Some(RuntimeHandoffKind::CodexTui));
    pin
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

#[tokio::test(flavor = "current_thread")]
async fn codex_runtime_ready_replaces_paused_incumbent_and_old_exit_keeps_successor() {
    let name = concat!(
        module_path!(),
        "::codex_runtime_ready_replaces_paused_incumbent_and_old_exit_keeps_successor"
    );
    if !crate::services::tui_o::cutover::test_override::in_empty_list_process(name) {
        return;
    }
    let env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _live = crate::config_live_reload::test_live_guard(env_lock);
    let (root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
    crate::config_live_reload::install(crate::config::Config::default());
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(62_841_778);
    let tmux = "AgentDesk-codex-replacement-bridge";
    let fake = FakeTmux::install(tmux);
    let transcript = root.path().join("native.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    crate::services::discord::stamp_spawn_markers(tmux, None).unwrap();
    let offset = std::fs::metadata(&transcript).unwrap().len();
    let api = Recorder::start().await;
    let _rest = test_rest::install(api.http.clone());
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
    Arc::get_mut(&mut shared).unwrap().api_port = api.port;
    shared.settings.write().await.provider = provider.clone();
    let submitted = watcher_spawn_observation::Scope::new(&shared);
    let observations = inflight::o_seed_observation::Guard::new(&provider, channel.get());
    let mut state = runtime_seed(provider.clone(), channel.get());
    state.tmux_session_name = Some(tmux.into());
    state.output_path = Some(transcript.display().to_string());
    state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    state.current_msg_id = 0;
    state.last_offset = offset;
    state.turn_start_offset = Some(0);
    state.turn_nonce = Some("replacement-bridge-managed-turn".into());
    save_inflight_state(&state).unwrap();
    state = load_inflight_state(&provider, channel.get()).unwrap();
    activity::tests::companion_attach_log_watcher(&shared, channel.get(), tmux, &transcript);
    let old = shared.tmux_watchers.get(&channel).unwrap();
    old.paused.store(true, Ordering::Release);
    let flags = (
        old.cancel.clone(),
        old.paused.clone(),
        old.resume_offset.clone(),
        old.pause_epoch.clone(),
        old.turn_delivered.clone(),
        old.last_heartbeat_ts_ms.clone(),
    );
    drop(old);
    let old_task = task_supervisor::spawn_observed_tmux_watcher(
        "codex_replacement_fixture_incumbent",
        shared.clone(),
        tmux.into(),
        flags.0.clone(),
        crate::services::discord::tmux::tmux_output_watcher_with_restore(
            channel,
            api.http.clone(),
            shared.clone(),
            transcript.display().to_string(),
            tmux.into(),
            offset,
            flags.0.clone(),
            flags.1.clone(),
            flags.2.clone(),
            flags.3.clone(),
            flags.4.clone(),
            flags.5.clone(),
            None,
        ),
    );
    let old_completion = watcher_completion::observe(&flags.0).expect("old spawn is registered");
    until(
        || observations.snapshot().event_count("host_probe") > 0,
        "old paused watcher never polled",
    )
    .await;
    let (exit_entered, release_exit) = install_owner_race_pause(channel, "watcher_before_release");
    let pin = adopt_codex_replacement_for_test(
        &shared,
        &mut state,
        RuntimeHandoff::CodexTui {
            rollout_path: transcript.display().to_string(),
            thread_id: Some("replacement-native-id".into()),
            tmux_session_name: tmux.into(),
            last_offset: offset,
        },
        &flags.0,
        api.http.clone(),
    )
    .await;
    assert_eq!(
        submitted.submissions(),
        2,
        "one actual old task and one actual successor task"
    );
    let successor_completion = watcher_completion::observe(&pin.cancel)
        .expect("actual RuntimeReady spawned a registered successor");
    tokio::time::timeout(Duration::from_secs(10), exit_entered.notified())
        .await
        .expect("old task never reached actual post_stream_exit");
    assert_eq!(observations.snapshot().event_count("post_stream_exit"), 1);
    let expected =
        InflightTurnIdentity::from_state(&load_inflight_state(&provider, channel.get()).unwrap());
    assert!(
        pin.adopt_if_current(&shared.tmux_watchers, |_| ())
            .is_some()
    );
    release_exit.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), old_completion.wait())
            .await
            .unwrap(),
        watcher_completion::Outcome::Returned
    );
    old_task.await.unwrap();
    let after = load_inflight_state(&provider, channel.get())
        .expect("cancelled old exit preserved managed successor row");
    assert!(expected.matches_state(&after));
    assert_eq!(after.turn_nonce, state.turn_nonce);
    let registered = shared
        .tmux_watchers
        .get(&channel)
        .expect("old exit preserved successor registry entry");
    assert!(Arc::ptr_eq(&registered.cancel, &pin.cancel));
    assert!(!registered.cancel.load(Ordering::Acquire));
    assert!(Arc::ptr_eq(&registered.turn_delivered, &pin.turn_delivered));
    drop(registered);
    assert!(
        pin.adopt_if_current(&shared.tmux_watchers, |_| ())
            .is_some()
    );
    until(
        || observations.snapshot().event_count("watcher_initialized") == 2,
        "actual successor watcher never entered its loop",
    )
    .await;
    // This uses actual spawn records and completion acknowledgements, never absence-as-joined.
    let finished = submitted.cancel_and_join_all().await;
    assert_eq!(finished.len(), 2);
    assert!(
        finished
            .iter()
            .all(|(_, outcome)| *outcome == watcher_completion::Outcome::Returned),
        "{finished:?}"
    );
    assert_eq!(
        successor_completion.wait().await,
        watcher_completion::Outcome::Returned
    );
    assert_eq!(observations.snapshot().event_count("post_stream_exit"), 2);
    assert!(!shared.tmux_watchers.contains_key(&channel));
    assert!(
        fake.take_calls()
            .iter()
            .all(|call| !call.starts_with("kill-session")),
        "a cancelled watcher must not kill the live replacement pane"
    );
}

#[tokio::test]
async fn codex_runtime_ready_row_absent_still_cannot_claim_a_watcher() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Codex;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mut state = runtime_seed(provider.clone(), 62_841_780);
    state.turn_nonce = Some("row-absent-replacement".into());
    let mut dirty = false;
    let observed = dispatch_process_handoff(
        &shared,
        &provider,
        &mut state,
        RuntimeHandoffLoopMessage::RuntimeReady {
            handoff: RuntimeHandoff::CodexTui {
                rollout_path: "/fixture/absent-native.jsonl".into(),
                thread_id: Some("absent-native".into()),
                tmux_session_name: "AgentDesk-codex-absent-replacement".into(),
                last_offset: 0,
            },
        },
        &mut dirty,
        false,
    )
    .await;
    assert_eq!(observed.outcome, Some(GuardedSaveOutcome::RowAbsent));
    assert_eq!(observed.claim_outcome, WatcherHandoffClaimOutcome::None);
    assert_eq!(observed.watcher_slots, 0);
    assert!(!observed.tmux_handed_off && !observed.watcher_relay_available);
    assert!(load_inflight_state(&provider, state.channel_id).is_none());
}
