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

/// Writes a Claude transcript under `dir` and a TUI-direct row resting at its offset 4096 with no
/// live lease, so a rebind fences the row forward; returns the transcript.
fn seed_tui_direct_row(dir: &std::path::Path, channel_id: u64, tmux: &str) -> std::path::PathBuf {
    let transcript = dir
        .join("projects")
        .join("53400000-0000-4000-8000-000000000990.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).expect("projects dir");
    let mut bytes = vec![b'x'; 4_095];
    bytes.push(b'\n');
    bytes.extend_from_slice(
        br#"{"type":"assistant","message":{"content":[{"type":"text","text":"P9PRE_BODY_A"}]}}"#,
    );
    std::fs::write(&transcript, bytes).expect("transcript");
    let mut row = super::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        None,
        crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
        5_340_990_001,
        5_340_990_002,
        "tui prompt".to_string(),
        Some("53400000-0000-4000-8000-000000000990".to_string()),
        Some(tmux.to_string()),
        Some(transcript.display().to_string()),
        None,
        4_096,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.turn_start_offset = Some(4_096);
    row.external_turn_id = Some("turn-p9pre".to_string());
    row.turn_source = super::inflight::TurnSource::ExternalInput;
    assert!(super::inflight::save_inflight_state_if_absent(&row).expect("row"));
    transcript
}

fn append(path: &std::path::Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    std::io::Write::write_all(&mut file, bytes).unwrap();
}

/// A TUI-direct row resting at offset 4096 of a Claude transcript on a listed, unadmitted pane.
/// Fields drop in order, so the shared env lock is released last.
struct WithheldTuiDirect {
    shared: std::sync::Arc<SharedData>,
    channel_id: u64,
    tmux_session: String,
    transcript: std::path::PathBuf,
    _live: InjectedLivenessGuard,
    _env: Vec<crate::config::TestEnvVarGuard>,
    _tmp: tempfile::TempDir,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl WithheldTuiDirect {
    fn new(channel_id: u64) -> Self {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("tempdir");
        let set = |key, path: &std::path::Path| {
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(key, path)
        };
        let env = vec![
            set("AGENTDESK_ROOT_DIR", tmp.path()),
            set("CLAUDE_CONFIG_DIR", tmp.path()),
        ];
        let tmux_session = format!("AgentDesk-claude-p9prerep-{}-cc", std::process::id());
        let live =
            InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux_session), HostLiveness::Live);
        crate::services::tmux_common::write_tmux_runtime_kind_marker(
            &tmux_session,
            RuntimeHandoffKind::ClaudeTui,
        )
        .expect("runtime-kind marker");
        crate::services::tui_prompt_dedupe::install_herdr_execution(&tmux_session, "p9pre-rebind");
        let transcript = seed_tui_direct_row(tmp.path(), channel_id, &tmux_session);
        adoption::ADOPT_FENCE_FORWARD_TEST_CUSTODY
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(channel_id.to_string());
        Self {
            shared: crate::services::discord::make_shared_data_for_tests(),
            channel_id,
            tmux_session,
            transcript,
            _live: live,
            _env: env,
            _tmp: tmp,
            _lock: lock,
        }
    }

    /// `/api/inflight/rebind` on the pane, as an operator or the watcher respawn calls it.
    fn rebind(&self) -> Result<RebindOutcome, RebindError> {
        let http = std::sync::Arc::new(serenity::Http::new("Bot test-token"));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        runtime.block_on(rebind_inflight_for_channel(
            &http,
            &self.shared,
            &ProviderKind::Claude,
            self.channel_id,
            Some(self.tmux_session.clone()),
            ManualRebindOverrides::default(),
            None,
        ))
    }

    fn row(&self) -> super::inflight::InflightTurnState {
        super::inflight::load_inflight_state_read_only(&ProviderKind::Claude, self.channel_id)
            .expect("the row survives a withheld rebind")
    }

    /// The fence-forward notices this channel posted, each with its dead-letter record.
    fn notices(&self) -> Vec<String> {
        let dispatches = adoption::ADOPT_FENCE_FORWARD_DISPATCHES
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let channel = self.channel_id.to_string();
        let own = dispatches
            .iter()
            .filter(|(record, _)| record.channel_id == channel);
        own.map(|(record, notice)| format!("{} {notice}", record.reason))
            .collect()
    }
}

// The watcher respawn repeats a withheld rebind while the pane keeps writing: no call adopts,
// fences or announces, so the row's cursor stays on the bytes no watcher has relayed.
#[test]
fn a_repeated_rebind_on_a_withheld_pane_adopts_fences_and_announces_nothing() {
    let pane = WithheldTuiDirect::new(5_340_990_000_000_001);
    let cursor = |row: &super::inflight::InflightTurnState| {
        (row.turn_start_offset, row.last_offset, row.relay_owner_kind)
    };
    let seeded = cursor(&pane.row());
    let first = pane
        .rebind()
        .expect_err("a withheld pane is not a successful rebind");
    assert!(
        matches!(first, RebindError::WatcherWithheld { .. }),
        "{first:?}"
    );
    assert_eq!(cursor(&pane.row()), seeded, "the first call adopts nothing");
    // The pane keeps producing while no watcher reads it.
    let tail = br#"
{"type":"assistant","message":{"content":[{"type":"text","text":"P9PRE_BODY_B"}]}}"#;
    append(&pane.transcript, tail);

    let second = pane.rebind().expect_err("still withheld");
    assert!(
        matches!(second, RebindError::WatcherWithheld { .. }),
        "{second:?}"
    );
    assert_eq!(
        cursor(&pane.row()),
        seeded,
        "the repeat adopts nothing either"
    );
    assert_eq!(
        pane.notices(),
        Vec::<String>::new(),
        "no fence-forward notice"
    );
    assert_eq!(pane.shared.tmux_watchers.len(), 0, "no watcher claimed");
}

/// Parks this thread's next claim past the rebind's pre-check, at its own admission read, while
/// the reconcile withdraws the pane's admission; the claim resumes once the withdrawal landed.
fn withdraw_at_the_claim(tmux: &str, nonce: &str) -> std::thread::JoinHandle<()> {
    use crate::services::tui_prompt_dedupe::{ADMISSION_PAUSE, ADMISSION_PAUSE_SKIPS};
    let (paused_tx, paused_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    ADMISSION_PAUSE_SKIPS.set(1);
    ADMISSION_PAUSE.set(Some((paused_tx, resume_rx)));
    let (tmux, nonce) = (tmux.to_string(), nonce.to_string());
    std::thread::spawn(move || {
        let wait = std::time::Duration::from_secs(30);
        paused_rx
            .recv_timeout(wait)
            .expect("the claim reached its admission");
        crate::services::tui_prompt_dedupe::withhold_herdr_execution(&tmux, Some(&nonce));
        resume_tx.send(()).unwrap();
    })
}

// Admission withdrawn between the rebind's adoption and its claim: the adoption stands with its
// range kept, and a repeat while the pane keeps writing adopts, keeps and queues nothing more.
#[test]
fn a_rebind_withheld_after_its_adoption_keeps_it_and_a_repeat_changes_nothing_pg() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let set = |key, path: &std::path::Path| {
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(key, path)
    };
    let _env = [
        set("AGENTDESK_ROOT_DIR", tmp.path()),
        set("CLAUDE_CONFIG_DIR", tmp.path()),
    ];
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime");
    let db = runtime.block_on(crate::db::auto_queue::test_support::TestPostgresDb::create());
    let pool = runtime.block_on(db.connect_and_migrate());
    let channel_id = 5_340_990_000_000_011_u64;
    let tmux = format!("AgentDesk-claude-p9prelate-{}-cc", std::process::id());
    let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux), HostLiveness::Live);
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux,
        RuntimeHandoffKind::ClaudeTui,
    )
    .expect("runtime-kind marker");
    crate::services::tui_prompt_dedupe::install_herdr_execution(&tmux, "p9pre-late");
    crate::services::tui_prompt_dedupe::admit_herdr_execution(&tmux, "p9pre-late");
    let transcript = seed_tui_direct_row(tmp.path(), channel_id, &tmux);
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    // The pane's sessions row names the transcript's session, as a launched TUI pane's does.
    let [session_key, _] = crate::services::discord::adk_session::build_session_key_candidates(
        &shared.token_hash,
        &ProviderKind::Claude,
        &tmux,
    );
    runtime.block_on(async {
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                   discord_token_hash, channel_id, claude_session_id)
             VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, $4)",
        )
        .bind(&session_key)
        .bind(&shared.token_hash)
        .bind(channel_id.to_string())
        .bind("53400000-0000-4000-8000-000000000990")
        .execute(&pool)
        .await
        .unwrap();
    });
    let http = std::sync::Arc::new(serenity::Http::new("Bot test-token"));
    let rebind = || {
        runtime.block_on(rebind_inflight_for_channel(
            &http,
            &shared,
            &ProviderKind::Claude,
            channel_id,
            Some(tmux.clone()),
            ManualRebindOverrides::default(),
            None,
        ))
    };
    let cursor = || {
        let row = super::inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel_id)
            .expect("the row survives a withheld rebind");
        let path = row.output_path.clone();
        (
            row.turn_start_offset,
            row.last_offset,
            path,
            row.effective_relay_owner_kind(),
        )
    };
    // What reached PG: the kept ranges and the channel notices queued for posting. Read after the
    // runtime ran the notice task it spawned.
    let posted = || {
        runtime.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let channel = channel_id.to_string();
            let kept: Vec<String> = sqlx::query_scalar(
                "SELECT reason FROM relay_dead_letter WHERE channel_id = $1 ORDER BY id",
            )
            .bind(&channel)
            .fetch_all(&pool)
            .await
            .unwrap();
            let notices: Vec<String> = sqlx::query_scalar(
                "SELECT content FROM message_outbox WHERE target = $1 ORDER BY id",
            )
            .bind(format!("channel:{channel}"))
            .fetch_all(&pool)
            .await
            .unwrap();
            (kept, notices)
        })
    };
    let unread_end = std::fs::metadata(&transcript).unwrap().len();

    let withdrawal = withdraw_at_the_claim(&tmux, "p9pre-late");
    let first = rebind();
    withdrawal.join().unwrap();
    assert!(
        matches!(first, Err(RebindError::WatcherWithheld { .. })),
        "{first:?}"
    );
    let adopted = cursor();
    let path = Some(transcript.display().to_string());
    assert_eq!(
        adopted,
        (
            Some(unread_end),
            unread_end,
            path,
            super::inflight::RelayOwnerKind::Watcher
        ),
        "the adoption completed and fenced past the kept range"
    );
    let (kept, notices) = posted();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert!(kept[0].contains("range_start=4096"), "{kept:?}");
    assert_eq!(shared.tmux_watchers.len(), 0, "no watcher claimed");

    // The pane keeps producing while no watcher reads it, and the respawn repeats the rebind.
    append(
        &transcript,
        br#"
{"type":"assistant","message":{"content":[{"type":"text","text":"P9PRE_BODY_LATE"}]}}"#,
    );
    let second = rebind();
    assert!(
        matches!(second, Err(RebindError::WatcherWithheld { .. })),
        "{second:?}"
    );
    assert_eq!(
        cursor(),
        adopted,
        "no second adoption, no cursor moved past the new bytes"
    );
    assert_eq!(posted(), (kept, notices), "nothing kept or announced twice");
    assert_eq!(shared.tmux_watchers.len(), 0, "no watcher claimed");
    drop(shared);
    runtime.block_on(async {
        pool.close().await;
        db.drop().await;
    });
}
