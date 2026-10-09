//! The rebind's input fence: a closed gate refuses before any write, an admitted rebind keeps the
//! drain open until it returns, and an unprotected channel rebinds as before.

use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use crate::services::discord::{inflight, make_shared_data_for_tests};
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};
use futures::FutureExt;

/// A live Claude TUI pane over an orphan watcher row for `channel_id`; returns the pane name and
/// the row's path.
fn live_orphan(root: &std::path::Path, channel_id: u64) -> (String, std::path::PathBuf) {
    let tmux = format!(
        "AgentDesk-claude-c2rb{channel_id}-{}-cc",
        std::process::id()
    );
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux,
        RuntimeHandoffKind::ClaudeTui,
    )
    .expect("runtime-kind marker");
    let session_uuid = format!(
        "48fdb7f3-6325-4000-8000-{:012}",
        channel_id % 1_000_000_000_000
    );
    let transcript = root.join(format!("{session_uuid}.jsonl"));
    std::fs::write(&transcript, vec![b'x'; 4_096]).expect("transcript");
    let mut orphan = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        None,
        0,
        0,
        channel_id + 7,
        String::new(),
        Some(session_uuid),
        Some(tmux.clone()),
        Some(transcript.display().to_string()),
        None,
        4_096,
    );
    orphan.turn_source = inflight::TurnSource::ExternalInput;
    orphan.set_relay_owner_kind(inflight::RelayOwnerKind::Watcher);
    assert!(inflight::save_inflight_state_if_absent(&orphan).expect("orphan row"));
    let row = input_runtime::fence::population_root()
        .expect("runtime root")
        .join("discord_inflight/claude")
        .join(format!("{channel_id}.json"));
    (tmux, row)
}

#[test]
fn c2_closed_input_gate_refuses_rebind_before_any_write() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let (open, fenced) = (6_325_520_000_000_001_u64, 6_325_520_000_000_002_u64);
    let (open_tmux, _) = live_orphan(tmp.path(), open);
    let (fenced_tmux, fenced_row) = live_orphan(tmp.path(), fenced);
    let _open_live =
        InjectedLivenessGuard::set(HostSessionRef::tmux(&open_tmux), HostLiveness::Live);
    let _fenced_live =
        InjectedLivenessGuard::set(HostSessionRef::tmux(&fenced_tmux), HostLiveness::Live);
    let before = std::fs::read(&fenced_row).expect("fenced row");
    let gate = Gate::protect(provider.clone(), fenced).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let closing = gate.close().unwrap();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    // A spawned watcher never runs: each runtime is dropped with its call.
    let rebind = |shared: Arc<SharedData>, channel_id: u64, tmux: &str, from_offset: bool| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let tmux = Some(tmux.to_string());
        if from_offset {
            return runtime.block_on(rebind_inflight_for_channel_with_minimum_start_offset(
                &http, &shared, &provider, channel_id, tmux, None, None,
            ));
        }
        runtime.block_on(rebind_inflight_for_channel(
            &http,
            &shared,
            &provider,
            channel_id,
            tmux,
            ManualRebindOverrides::default(),
            None,
        ))
    };

    let open_shared = make_shared_data_for_tests();
    let rebound =
        rebind(open_shared.clone(), open, &open_tmux, false).expect("an unprotected pane rebinds");
    assert!(rebound.watcher_spawned);
    assert_eq!(open_shared.tmux_watchers.len(), 1);

    // Both rebind entries refuse before their first effect.
    for from_offset in [false, true] {
        let fenced_shared = make_shared_data_for_tests();
        let refused = rebind(fenced_shared.clone(), fenced, &fenced_tmux, from_offset);
        assert!(
            matches!(refused, Err(RebindError::InputFenced(_))),
            "{refused:?}"
        );
        assert_eq!(std::fs::read(&fenced_row).unwrap(), before, "row untouched");
        assert_eq!(fenced_shared.tmux_watchers.len(), 0, "no watcher claimed");
        let sessions = fenced_shared
            .core
            .try_lock()
            .expect("core free")
            .sessions
            .len();
        assert_eq!(sessions, 0, "no session registered");
    }
    assert!(
        input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={fenced}"))),
        "the refusal is a health reason"
    );
    assert!(closing.drain().now_or_never().is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn c2_admitted_rebind_holds_the_input_drain_until_it_returns() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    for from_offset in [false, true] {
        let channel = 6_325_520_000_000_003_u64 + u64::from(from_offset);
        let gate = Gate::protect(provider.clone(), channel).unwrap();
        let _health = input_runtime::fence::test_health::Clear::new(&gate);
        let shared = make_shared_data_for_tests();
        // The rebind is admitted, then parks on the session map this test holds.
        let core = shared.core.lock().await;
        let rebind = async {
            if from_offset {
                rebind_inflight_for_channel_with_minimum_start_offset(
                    &http, &shared, &provider, channel, None, None, None,
                )
                .await
            } else {
                rebind_inflight_for_channel(
                    &http,
                    &shared,
                    &provider,
                    channel,
                    None,
                    ManualRebindOverrides::default(),
                    None,
                )
                .await
            }
        };
        tokio::pin!(rebind);
        assert!(futures::poll!(&mut rebind).is_pending());
        let closing = gate.close().unwrap();
        assert!(
            closing.drain().now_or_never().is_none(),
            "the admitted rebind holds the drain"
        );
        drop(core);
        let result = rebind.await;
        assert!(
            matches!(result, Err(RebindError::ChannelNameMissing)),
            "{result:?}"
        );
        assert!(
            closing.drain().now_or_never().is_some(),
            "returning releases the drain"
        );
    }
}

/// An admitted rebind on a protected open channel, through either entry, unpinned or adopting a
/// pinned episode, completes its row write and watcher claim from async code; returning releases
/// the drain.
#[test]
fn c2b_admitted_rebind_writes_its_row_and_releases_the_drain() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    for (from_offset, pinned) in [(false, false), (false, true), (true, false), (true, true)] {
        let channel = 6_325_520_000_000_005_u64 + 2 * u64::from(from_offset) + u64::from(pinned);
        let (tmux, row) = live_orphan(tmp.path(), channel);
        let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux), HostLiveness::Live);
        let before = std::fs::read(&row).expect("orphan row");
        let pin = pinned.then(|| {
            inflight::InflightEpisodePin::from_state(
                &inflight::load_inflight_state(&provider, channel).expect("orphan state"),
            )
        });
        let gate = Gate::protect(provider.clone(), channel).unwrap();
        let _health = input_runtime::fence::test_health::Clear::new(&gate);
        let shared = make_shared_data_for_tests();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");

        let rebind = async {
            if from_offset {
                rebind_inflight_for_channel_with_minimum_start_offset(
                    &http,
                    &shared,
                    &provider,
                    channel,
                    Some(tmux.clone()),
                    None,
                    pin.as_ref(),
                )
                .await
            } else {
                rebind_inflight_for_channel(
                    &http,
                    &shared,
                    &provider,
                    channel,
                    Some(tmux.clone()),
                    ManualRebindOverrides::default(),
                    pin.as_ref(),
                )
                .await
            }
        };
        let rebound = runtime.block_on(rebind).unwrap_or_else(|error| {
            panic!("from_offset={from_offset} pinned={pinned}: admitted rebind failed: {error:?}")
        });

        assert!(rebound.watcher_spawned, "pinned={pinned}");
        assert_eq!(shared.tmux_watchers.len(), 1, "pinned={pinned}");
        assert_ne!(
            std::fs::read(&row).expect("rebound row"),
            before,
            "pinned={pinned}: the rebind rewrote its row"
        );
        assert!(
            !input_runtime::health_reasons()
                .iter()
                .any(|reason| reason.contains(&format!("channel={channel}"))),
            "pinned={pinned}: the admitted rebind saw a refused writer"
        );
        let closing = gate.close().unwrap();
        assert!(
            closing.drain().now_or_never().is_some(),
            "pinned={pinned}: returning releases the drain"
        );
        drop(runtime);
    }
}
