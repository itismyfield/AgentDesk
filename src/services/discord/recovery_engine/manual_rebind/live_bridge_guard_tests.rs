//! Both rebind entries, pinned or not, rewrite an orphan row and claim its watcher.

use super::*;
use crate::services::discord::{inflight, make_shared_data_for_tests};
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};

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
    let root = inflight::inflight_runtime_root().expect("runtime root");
    let row = inflight::inflight_state_path(&root, &ProviderKind::Claude, channel_id);
    (tmux, row)
}

/// A rebind through either entry, pinned or not, writes its row and claims its watcher.
#[test]
fn rebind_writes_its_row_and_claims_its_watcher_through_either_entry() {
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
            panic!("from_offset={from_offset} pinned={pinned}: rebind failed: {error:?}")
        });

        assert!(rebound.watcher_spawned, "pinned={pinned}");
        assert_eq!(shared.tmux_watchers.len(), 1, "pinned={pinned}");
        assert_ne!(
            std::fs::read(&row).expect("rebound row"),
            before,
            "pinned={pinned}: the rebind rewrote its row"
        );
        drop(runtime);
    }
}
