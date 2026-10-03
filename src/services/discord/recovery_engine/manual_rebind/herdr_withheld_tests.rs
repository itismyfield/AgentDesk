//! A rebind onto a pane the Herdr admission withholds reports the withhold, never a reused
//! watcher, and claims nothing; the same rebind on a Legacy pane spawns as before.

use super::super::*;
use crate::services::session_host::test_support::InjectedLivenessGuard;
use crate::services::session_host::{HostLiveness, HostSessionRef};

/// `/api/inflight/rebind` on a live Claude TUI pane over an orphan row, with tmux answering live.
fn rebind(listed: bool) -> (Result<RebindOutcome, RebindError>, usize) {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let channel_id = 5_340_840_000_000_001_u64 + u64::from(listed);
    // `-cc` keeps the channel name bound to Claude in the default settings.
    let tmux_session = format!(
        "AgentDesk-claude-p84rebind-{}-{listed}-cc",
        std::process::id()
    );
    let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux_session), HostLiveness::Live);
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux_session,
        RuntimeHandoffKind::ClaudeTui,
    )
    .expect("runtime-kind marker");
    if listed {
        crate::services::tui_prompt_dedupe::install_herdr_execution(&tmux_session, "p8-4-rebind");
    }
    let session_uuid = "48fdb7f3-5340-4000-8000-000000000084";
    let transcript = tmp.path().join(format!("{session_uuid}.jsonl"));
    std::fs::write(&transcript, vec![b'x'; 4_096]).expect("transcript");
    let mut orphan = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        0,
        0,
        1_518_888_000_000_000_084,
        String::new(),
        Some(session_uuid.to_string()),
        Some(tmux_session.clone()),
        Some(transcript.display().to_string()),
        None,
        4_096,
    );
    orphan.turn_source = super::inflight::TurnSource::ExternalInput;
    orphan.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
    assert!(super::inflight::save_inflight_state_if_absent(&orphan).expect("orphan row"));

    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = std::sync::Arc::new(serenity::Http::new("Bot test-token"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    let result = runtime.block_on(rebind_inflight_for_channel(
        &http,
        &shared,
        &provider,
        channel_id,
        Some(tmux_session),
        ManualRebindOverrides::default(),
        None,
    ));
    // A spawned watcher never runs: its current-thread runtime is dropped with this frame.
    (result, shared.tmux_watchers.len())
}

#[test]
fn a_rebind_on_a_withheld_herdr_pane_reports_the_withhold_not_a_reused_watcher() {
    let (legacy, watchers) = rebind(false);
    let legacy = legacy.expect("a Legacy pane rebinds");
    assert!(legacy.watcher_spawned, "a Legacy pane spawns its watcher");
    assert_eq!(watchers, 1);

    let (withheld, watchers) = rebind(true);
    let error = withheld.expect_err("a withheld pane is not a successful rebind");
    let named = |name: &String| name.contains("p84rebind");
    assert!(
        matches!(&error, RebindError::WatcherWithheld { tmux_session } if named(tmux_session)),
        "{error:?}"
    );
    assert!(error.to_string().contains("host not admitted"), "{error}");
    assert_eq!(watchers, 0, "no watcher claimed, spawned or reused");
}

/// Repeat a withheld rebind: second call must be safe (no duplicate adoption/post/state reversion).
#[test]
fn withheld_rebind_repeated_is_idempotent() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let channel_id = 5_340_840_000_000_099_u64;
    let tmux_session = format!("AgentDesk-claude-p84idempotent-{}-cc", std::process::id());
    let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux_session), HostLiveness::Live);
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux_session,
        RuntimeHandoffKind::ClaudeTui,
    )
    .expect("runtime-kind marker");

    // Install withheld execution
    crate::services::tui_prompt_dedupe::install_herdr_execution(&tmux_session, "idempotent-nonce");
    crate::services::tui_prompt_dedupe::withhold_herdr_execution(
        &tmux_session,
        Some("idempotent-nonce"),
    );

    let session_uuid = "48fdb7f3-5340-4000-8000-000000000099";
    let transcript = tmp.path().join(format!("{session_uuid}.jsonl"));
    std::fs::write(&transcript, vec![b'x'; 4_096]).expect("transcript");

    let mut orphan = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id,
        None,
        0,
        0,
        1_518_888_000_000_000_099,
        String::new(),
        Some(session_uuid.to_string()),
        Some(tmux_session.clone()),
        Some(transcript.display().to_string()),
        None,
        4_096,
    );
    orphan.turn_source = super::inflight::TurnSource::ExternalInput;
    orphan.set_relay_owner_kind(super::inflight::RelayOwnerKind::Watcher);
    assert!(super::inflight::save_inflight_state_if_absent(&orphan).expect("orphan row"));

    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = std::sync::Arc::new(serenity::Http::new("Bot test-token"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");

    // First call: should return WatcherWithheld
    let result1 = runtime.block_on(rebind_inflight_for_channel(
        &http,
        &shared,
        &provider,
        channel_id,
        Some(tmux_session.clone()),
        ManualRebindOverrides::default(),
        None,
    ));
    let err1 = result1.expect_err("first withheld rebind should error");
    assert!(matches!(&err1, RebindError::WatcherWithheld { .. }));

    // Second call: must also return WatcherWithheld (not crash, not change state)
    let result2 = runtime.block_on(rebind_inflight_for_channel(
        &http,
        &shared,
        &provider,
        channel_id,
        Some(tmux_session.clone()),
        ManualRebindOverrides::default(),
        None,
    ));
    let err2 = result2.expect_err("second withheld rebind should also error");
    assert!(matches!(&err2, RebindError::WatcherWithheld { .. }));

    // Verify no spurious watcher was spawned
    assert_eq!(
        shared.tmux_watchers.len(),
        0,
        "no watcher should be spawned for withheld pane"
    );
}
