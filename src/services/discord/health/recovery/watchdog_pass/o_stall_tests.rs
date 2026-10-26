//! The watchdog entry point on an O-owned channel left open the way 10-23 01:51Z was.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude::UserId;

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::transcript_binding_stall as binding_stall;
use crate::services::provider::CancelToken;
use crate::services::tui_o::writer::progress::{ChannelProgress, Publisher};

#[derive(Clone, Default)]
pub(super) struct Captured(pub(super) Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

pub(super) fn watcher(tmux: &str, output: &std::path::Path) -> discord::TmuxWatcherHandle {
    discord::TmuxWatcherHandle {
        tmux_session_name: tmux.to_string(),
        output_path: output.to_string_lossy().into_owned(),
        paused: Arc::new(AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(AtomicBool::new(false)),
        pause_epoch: Arc::new(AtomicU64::new(0)),
        turn_delivered: Arc::new(AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(AtomicI64::new(0)),
    }
}

/// The pass over an O turn drained `still_secs` ago in a mailbox open `turn_age_secs`, above a
/// 30-minute self-refreshed Legacy row; returns its WARN logs after checking it cleaned nothing.
async fn pass_on_a_drained_o_turn(
    channel: ChannelId,
    turn_age_secs: u64,
    still_secs: u64,
) -> Option<String> {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tempdir = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tempdir.path(),
    );
    if !crate::services::platform::tmux::is_available() {
        eprintln!("SOFT_SKIP: tmux unavailable");
        return None;
    }
    let provider = ProviderKind::Codex;
    let user_msg = MessageId::new(channel.get() + 1);
    // A per-run name whose pane exits on its own, so the fixture never kills a session.
    let tmux = format!(
        "AgentDesk-codex-7333-o-stall-{}-{}",
        channel.get(),
        std::process::id()
    );
    assert!(
        crate::services::platform::tmux::create_session(&tmux, None, "sleep 30")
            .expect("start tmux fixture")
            .status
            .success()
    );
    let _owned = crate::services::tui_o::cutover::test_override::force_channels(&[(
        channel.get(),
        RuntimeHandoffKind::CodexTui,
    )]);

    let mut registry = HealthRegistry::new();
    registry.started_at_unix = chrono::Utc::now().timestamp() - 3_600;
    let shared = discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let token = Arc::new(CancelToken::new());
    assert!(
        discord::mailbox_try_start_turn(&shared, channel, token.clone(), UserId::new(7), user_msg)
            .await
    );
    let mailbox = shared.mailbox_peek(channel).expect("mailbox actor");
    mailbox
        .age_active_turn_for_test(Duration::from_secs(turn_age_secs))
        .await;

    let rollout = tempdir.path().join("rollout-7333.jsonl");
    std::fs::write(
        &rollout,
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
    )
    .expect("rollout fixture");
    let len = std::fs::metadata(&rollout).expect("rollout len").len();
    let ended = chrono::Utc::now().timestamp() - 1_800;
    filetime::set_file_mtime(&rollout, filetime::FileTime::from_unix_time(ended, 0))
        .expect("age rollout");
    let mut row = discord::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        7,
        user_msg.get(),
        user_msg.get() + 1,
        "TURN-3".to_string(),
        Some("01a16662".to_string()),
        Some(tmux.clone()),
        Some(rollout.to_string_lossy().into_owned()),
        None,
        len,
    );
    row.started_at = (chrono::Local::now() - chrono::Duration::minutes(30))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    row.last_tool_name = Some("exec_command".to_string());
    row.has_post_tool_text = false;
    discord::inflight::save_inflight_state(&row).expect("bridge save refreshes updated_at");
    shared
        .tmux_watchers
        .insert(channel, watcher(&tmux, &rollout));

    let actor = Publisher::new(channel.get());
    actor.publish(ChannelProgress {
        debt: 0,
        unsealed: false,
        serial: 96,
    });
    binding_stall::o_witness_backdate_for_tests(channel.get(), 96, still_secs);

    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(captured.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let guard = tracing::subscriber::set_default(subscriber);
    let cleaned = run_stall_watchdog_pass(&registry, &provider).await;
    drop(guard);
    let logs = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();

    assert!(
        logs.contains("o_drained_turn_open"),
        "the snapshot names the open mailbox:\n{logs}"
    );
    assert!(
        !logs.contains("STALL-WATCHDOG: shadow_verdict"),
        "the capture axis is not judged for an O-owned channel:\n{logs}"
    );
    assert_eq!(cleaned, 0);
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_some(),
        "paging cleans nothing"
    );
    assert!(discord::inflight::load_inflight_state(&provider, channel.get()).is_some());
    Some(logs)
}

/// 10-23 01:51Z: O posted the footer and owed nothing, and the mailbox stayed open for 30 minutes.
#[tokio::test]
async fn a_drained_o_turn_pages_without_the_capture_axis_despite_a_self_refreshed_row() {
    let channel = ChannelId::new(7_333_401);
    let Some(logs) = pass_on_a_drained_o_turn(channel, 1_800, 400).await else {
        return;
    };
    assert!(
        logs.contains(&format!("suspected stall on channel {}", channel.get())),
        "the drained turn pages:\n{logs}"
    );
}

/// The page waits for the turn the verdict judged, however old the Legacy row is.
#[tokio::test]
async fn a_drained_o_turn_younger_than_the_page_threshold_does_not_page_on_an_old_row() {
    let channel = ChannelId::new(7_333_411);
    let Some(logs) = pass_on_a_drained_o_turn(channel, 450, 350).await else {
        return;
    };
    assert!(
        !logs.contains("suspected stall on channel"),
        "a 450 s turn is short of the 600 s page threshold:\n{logs}"
    );
}
