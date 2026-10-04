use super::*;
use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};

struct Root(Option<std::ffi::OsString>);
impl Root {
    fn new(root: &Path) -> Self {
        let old = std::env::var_os("AGENTDESK_ROOT_DIR");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root) };
        Self(old)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        unsafe {
            match &self.0 {
                Some(old) => std::env::set_var("AGENTDESK_ROOT_DIR", old),
                None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
            }
        }
    }
}

fn row(channel: u64, owner: u64) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        owner,
        7,
        0,
        "prompt".into(),
        None,
        Some("n1a-retirement-fixture".into()),
        None,
        None,
        0,
    );
    row.turn_source = TurnSource::ExternalInput;
    row.set_relay_owner_kind(RelayOwnerKind::Watcher);
    row.restart_mode = Some(crate::services::discord::InflightRestartMode::DrainRestart);
    row
}
fn save(row: &InflightTurnState) -> PathBuf {
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &ProviderKind::Claude,
        row.channel_id,
    );
    let _guard = inflight::lock_inflight_state_path(&path).unwrap();
    std::fs::write(&path, serde_json::to_vec(row).unwrap()).unwrap();
    path
}
fn pending(channel: u64) -> TuiDirectPendingStart {
    TuiDirectPendingStart {
        provider: "claude".into(),
        channel_id: channel,
        tmux_session_name: "n1a-retirement-fixture".into(),
        prompt_text: "prompt".into(),
        anchor_message_id: 7,
        lease_relay_owner: "tmux_watcher".into(),
        lease_runtime_kind: Some("claude_tui".into()),
        lease_turn_id: None,
        lease_session_key: None,
        generation: 0,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: None,
    }
}

#[test]
fn n1a_retirement_removes_synthetic_residue_without_delivery_evidence() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    let channel = 63250101;
    let state = row(channel, 1);
    let path = save(&state);
    persist(&pending(channel)).unwrap();
    let marker = markers::AbortedAnchorMarker::for_abort(
        "claude".into(),
        channel,
        7,
        "n1a-retirement-fixture".into(),
        0,
        None,
    );
    markers::record(&marker).unwrap();
    let mut deferred = marker.clone();
    deferred.anchor_message_id = 8;
    deferred.origin = markers::MarkerOrigin::DeferredClaim;
    markers::record(&deferred).unwrap();
    let before = std::fs::read(&path).unwrap();
    let custody = root_dir.path().join("custody-copy.json");
    std::fs::copy(&path, &custody).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(
        !inflight::load_inflight_state(&ProviderKind::Claude, channel)
            .unwrap()
            .terminal_delivery_committed
    );
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        channel
    ));
    let result = retire_channel(&ProviderKind::Claude, channel).unwrap();
    assert!(
        !path.exists(),
        "synthetic row must retire despite DrainRestart"
    );
    assert_eq!(
        result,
        Retirement {
            removed: 4,
            retry_pending: false
        }
    );
    assert!(!pending_synthetic_start_present("claude", channel));
    assert!(records_for_channel("claude", channel).is_empty());
    assert!(markers::load_for_channel("claude", channel).is_empty());
    let copied: InflightTurnState =
        serde_json::from_slice(&std::fs::read(custody).unwrap()).unwrap();
    assert!(!copied.terminal_delivery_committed);
    assert_eq!(
        retire_channel(&ProviderKind::Claude, channel).unwrap(),
        Retirement::default()
    );
}

#[test]
fn n1a_retirement_preserves_real_discord_row_and_other_channels() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    let channel = 63250102;
    let path = save(&row(channel, 42));
    let other = save(&row(channel + 1, 1));
    let bytes = std::fs::read(&path).unwrap();
    let other_bytes = std::fs::read(&other).unwrap();
    persist(&pending(channel)).unwrap();
    persist(&pending(channel + 1)).unwrap();
    let result = retire_channel(&ProviderKind::Claude, channel).unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "Discord input remains Legacy responsibility"
    );
    assert_eq!(std::fs::read(other).unwrap(), other_bytes);
    assert_eq!(result.removed, 1);
    assert!(!result.retry_pending);
    assert_eq!(records_for_channel("claude", channel + 1).len(), 1);
    delete(&pending(channel + 1));
}

#[test]
fn n1a_retirement_read_failure_leaves_population_untouched() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    let channel = 63250104;
    let path = save(&row(channel, 1));
    persist(&pending(channel)).unwrap();
    let bad = root().unwrap().join(format!("claude_{channel}_8.json"));
    std::fs::write(&bad, "invalid").unwrap();
    assert!(retire_channel(&ProviderKind::Claude, channel).is_err());
    assert!(path.exists());
    assert!(pending_synthetic_start_present("claude", channel));
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        channel
    ));
    delete(&pending(channel));
}

#[test]
fn n1a_retirement_race_preserves_the_replacement_snapshot() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    let channel = 63250105;
    let path = save(&row(channel, 1));
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *PAUSE.lock().unwrap_or_else(|e| e.into_inner()) = Some((channel, reached_tx, resume_rx));
    let retire =
        std::thread::spawn(move || retire_channel(&ProviderKind::Claude, channel).unwrap());
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap();
    save(&row(channel, 42));
    let bytes = std::fs::read(&path).unwrap();
    resume_tx.send(()).unwrap();
    assert!(retire.join().unwrap().retry_pending);
    assert_eq!(
        std::fs::read(path).unwrap(),
        bytes,
        "replacement must not be retired"
    );
}

// Contention requires Unix-only exclusive marker claims.
#[cfg(unix)]
#[test]
fn n1a_retirement_delete_contention_returns_residue_for_retry() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    let channel = 63250106;
    let path = save(&row(channel, 1));
    let marker = markers::AbortedAnchorMarker::for_abort(
        "claude".into(),
        channel,
        7,
        "n1a-retirement-fixture".into(),
        0,
        None,
    );
    markers::record(&marker).unwrap();
    let held = markers::try_claim_marker(&marker).unwrap();
    let result = retire_channel(&ProviderKind::Claude, channel).unwrap();
    assert!(!path.exists());
    assert!(result.retry_pending);
    assert_eq!(markers::load_for_channel("claude", channel).len(), 1);
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        channel
    ));
    drop(held);
    let retry = retire_channel(&ProviderKind::Claude, channel).unwrap();
    assert_eq!(
        retry,
        Retirement {
            removed: 1,
            retry_pending: false
        }
    );
    assert!(markers::load_for_channel("claude", channel).is_empty());
}

// A file parent induces ENOTDIR only on Unix.
#[cfg(unix)]
#[test]
fn n1a_retirement_post_snapshot_io_error_is_retryable() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    let channel = 63250107;
    persist(&pending(channel)).unwrap();
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *PAUSE.lock().unwrap_or_else(|e| e.into_inner()) = Some((channel, reached_tx, resume_rx));
    let retire =
        std::thread::spawn(move || retire_channel(&ProviderKind::Claude, channel).unwrap());
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap();
    let pending_root = root().unwrap();
    let moved = pending_root.with_extension("held");
    std::fs::rename(&pending_root, &moved).unwrap();
    std::fs::write(&pending_root, b"unreadable parent").unwrap();
    resume_tx.send(()).unwrap();
    assert!(
        retire.join().unwrap().retry_pending,
        "IO errors are never evidence of absence"
    );
    assert!(moved.join(format!("claude_{channel}_7.json")).exists());
    std::fs::remove_file(&pending_root).unwrap();
    std::fs::rename(moved, pending_root).unwrap();
    let retry = retire_channel(&ProviderKind::Claude, channel).unwrap();
    assert_eq!(
        retry,
        Retirement {
            removed: 1,
            retry_pending: false
        }
    );
}

#[test]
fn n1a_retirement_covers_monitor_rebind_and_watcher_synthetics() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    for (i, source) in [
        TurnSource::MonitorTriggered,
        TurnSource::ExternalAdopted,
        TurnSource::ExternalInput,
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 63250120 + i as u64;
        let mut synthetic = row(channel, 0);
        synthetic.user_msg_id = 0;
        synthetic.rebind_origin = source != TurnSource::ExternalInput;
        synthetic.turn_source = source;
        let path = save(&synthetic);
        let retired = retire_channel(&ProviderKind::Claude, channel).unwrap();
        assert!(
            !path.exists(),
            "every existing synthetic classifier branch retires"
        );
        assert_eq!(
            retired,
            Retirement {
                removed: 1,
                retry_pending: false
            }
        );
    }
}

#[test]
fn n1c_boot_confirms_only_selected_committed_channels_after_full_retirement() {
    use crate::services::agent_protocol::RuntimeHandoffKind::{ClaudeTui, CodexTui};
    use crate::services::tui_o::cutover::test_override;
    use crate::services::tui_o::turn_mode::{TestConfirmation, transcript_turns};
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let root_dir = tempfile::tempdir().unwrap();
    let _root = Root::new(root_dir.path());
    let [retired, unreadable, unselected, codex] = [63250201, 63250202, 63250203, 63250204];
    #[allow(unused_mut)]
    let mut owned = vec![
        (retired, ClaudeTui),
        (unreadable, ClaudeTui),
        (unselected, ClaudeTui),
        (codex, CodexTui),
    ];
    #[cfg(unix)]
    let partial = 63250205;
    #[cfg(unix)]
    owned.push((partial, ClaudeTui));
    let _boot = test_override::force_channels(&owned);
    let synthetic = save(&row(retired, 1));
    persist(&pending(retired)).unwrap();
    let kept = save(&row(unselected, 1));
    let unread_row = save(&row(unreadable, 1));
    let bad = root().unwrap().join(format!("claude_{unreadable}_8.json"));
    std::fs::write(&bad, "invalid").unwrap();
    #[cfg(unix)]
    let (marker, held) = {
        let marker = markers::AbortedAnchorMarker::for_abort(
            "claude".into(),
            partial,
            7,
            "n1a-retirement-fixture".into(),
            0,
            None,
        );
        markers::record(&marker).unwrap();
        let held = markers::try_claim_marker(&marker).unwrap();
        (marker, held)
    };

    let mut config = TuiOConfig::default();
    assert!(confirm_at_boot(&ProviderKind::Claude, None).is_empty());
    assert!(confirm_at_boot(&ProviderKind::Claude, Some(&config)).is_empty());
    assert!(synthetic.exists(), "an empty selection retires nothing");

    config.turn.channels = owned
        .iter()
        .map(|(channel, _)| *channel)
        .filter(|&channel| channel != unselected)
        .collect();
    let (reached_tx, reached_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *PAUSE.lock().unwrap_or_else(|e| e.into_inner()) = Some((retired, reached_tx, resume_rx));
    let shared = test_override::shared_channels();
    let boot = std::thread::spawn(move || {
        let _boot = shared();
        confirm_at_boot(&ProviderKind::Claude, Some(&config))
    });
    reached_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap();
    assert!(
        !transcript_turns(retired),
        "confirmation follows the completed retirement"
    );
    resume_tx.send(()).unwrap();
    assert_eq!(boot.join().unwrap(), [retired]);
    let _confirmed = TestConfirmation::confirmed(retired);
    assert!(!synthetic.exists());
    assert!(!pending_synthetic_start_present("claude", retired));
    for refused in [unreadable, unselected, codex] {
        assert!(
            !transcript_turns(refused),
            "{refused} stays on Legacy turns"
        );
    }
    assert!(kept.exists() && unread_row.exists() && bad.exists());
    #[cfg(unix)]
    {
        assert!(
            !transcript_turns(partial),
            "a retirement that left residue keeps Legacy turns"
        );
        drop(held);
        markers::delete(&marker);
    }
}
