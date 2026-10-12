//! Legacy restore effects are observed through the public entry in a child with fake tmux.
use super::super::n4d2_entry_support::{fixture_with_pool, isolated, list_sessions, root};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::legacy_supervision::test_support;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::input_runtime::fence::{Gate, HoldUnknownForTest, Mode};
use crate::services::discord::task_supervisor::n4d_spawn_observation::Scope;
use crate::services::tmux_common as tc;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use tokio::sync::Notify;

type Point = (u64, &'static str);
static PAUSES: LazyLock<Mutex<BTreeMap<Point, Arc<Pause>>>> = LazyLock::new(Mutex::default);
static ATTEMPTS: LazyLock<Mutex<BTreeMap<Point, usize>>> = LazyLock::new(Mutex::default);

struct Pause {
    entered: Notify,
    resume: Notify,
}

pub(super) async fn at(channel: u64, stage: &'static str) {
    let pause = PAUSES.lock().unwrap().remove(&(channel, stage));
    if let Some(pause) = pause {
        pause.entered.notify_one();
        pause.resume.notified().await;
    }
}

pub(super) fn attempt(channel: u64, effect: &'static str) {
    *ATTEMPTS
        .lock()
        .unwrap()
        .entry((channel, effect))
        .or_default() += 1;
}

fn arm(channel: u64, stage: &'static str) -> Arc<Pause> {
    let pause = Arc::new(Pause {
        entered: Notify::new(),
        resume: Notify::new(),
    });
    assert!(
        PAUSES
            .lock()
            .unwrap()
            .insert((channel, stage), pause.clone())
            .is_none()
    );
    pause
}

fn attempts(channel: u64) -> BTreeMap<&'static str, usize> {
    ATTEMPTS
        .lock()
        .unwrap()
        .iter()
        .filter_map(|(&(id, effect), &count)| (id == channel).then_some((effect, count)))
        .collect()
}

async fn protect(fixture: &InstallFixture, mode: Mode) -> Arc<Gate> {
    let gate = Gate::protect(fixture.provider.clone(), fixture.channel.get()).unwrap();
    if mode != Mode::LegacyOpen {
        let closing = Arc::new(gate.close().unwrap());
        if mode != Mode::Closing {
            let transition = fixture
                .shared
                .freeze_legacy_queue(
                    closing.clone(),
                    crate::services::turn_orchestrator::QueuePersistenceContext::new(
                        &fixture.provider,
                        &fixture.shared.token_hash,
                        None,
                    ),
                )
                .await
                .unwrap();
            match mode {
                Mode::Frozen => {}
                Mode::LedgerOpen => closing.open_ledger().unwrap(),
                Mode::Held => closing.hold().unwrap(),
                Mode::Handback => closing.begin_handback().unwrap(),
                _ => unreachable!(),
            }
            drop(transition);
        }
    }
    assert_eq!(gate.mode(), mode);
    assert!(
        crate::services::discord::input_runtime::fence::lookup(
            &fixture.provider,
            fixture.channel.get(),
        )
        .is_some()
    );
    gate
}

fn seed_row(fixture: &InstallFixture) -> PathBuf {
    let mut row = InflightTurnState::new(
        fixture.provider.clone(),
        fixture.channel.get(),
        Some("n3bc-old".into()),
        7,
        111,
        112,
        "old prompt".into(),
        Some("old-native".into()),
        Some(fixture.session.clone()),
        Some(fixture.output.clone()),
        None,
        0,
    );
    row.turn_start_offset = Some(0);
    row.turn_nonce = Some(format!("n3bc-nonce-{}", fixture.channel.get()));
    row.session_key = Some(
        crate::services::discord::adk_session::build_namespaced_session_key(
            &fixture.shared.token_hash,
            &fixture.provider,
            &fixture.session,
        ),
    );
    test_support::seed_backfill_row(&row)
}

async fn seed_dispatch_rebind(fixture: &InstallFixture, row: &Path) -> String {
    let mut content: serde_json::Value =
        serde_json::from_slice(&std::fs::read(row).unwrap()).unwrap();
    let dispatch = format!("n3bc-dispatch-{}", fixture.channel.get());
    content["dispatch_id"] = serde_json::Value::String(dispatch.clone());
    let key = content["session_key"].as_str().unwrap();
    let nonce = content["turn_nonce"].as_str().unwrap();
    let pool = fixture.shared.pg_pool.as_ref().unwrap();
    sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
        .bind(&dispatch)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(sqlx::query("UPDATE sessions SET status = 'turn_active', active_turn_nonce = $2, active_dispatch_id = NULL WHERE session_key = $1")
        .bind(key).bind(nonce).execute(pool).await.unwrap().rows_affected(), 1);
    std::fs::write(row, serde_json::to_vec_pretty(&content).unwrap()).unwrap();
    dispatch
}

fn mutations(discord: &test_support::MockDiscord, channel: u64) -> Vec<String> {
    discord
        .requests_for(channel)
        .into_iter()
        .filter(|request| {
            ["DELETE ", "PATCH ", "POST "]
                .iter()
                .any(|method| request.starts_with(method))
        })
        .collect()
}

async fn pair(
    pool: &sqlx::PgPool,
    channel: u64,
) -> (InstallFixture, InstallFixture, test_support::MockDiscord) {
    let (protected, discord) =
        fixture_with_pool(ProviderKind::Claude, channel, Some(pool.clone())).await;
    let mut sibling =
        InstallFixture::new_with_pool(ProviderKind::Claude, channel + 1, Some(pool.clone())).await;
    sibling.shared = protected.shared.clone();
    sibling.registry = protected.registry.clone();
    sibling.http = protected.http.clone();
    protected.shared.settings.write().await.allowed_channel_ids = vec![channel, channel + 1];
    for fixture in [&protected, &sibling] {
        std::fs::write(tc::session_temp_path(&fixture.session, "host_kind"), "tmux").unwrap();
        std::fs::write(tc::session_temp_path(&fixture.session, "generation"), "4").unwrap();
        test_support::age_file(
            Path::new(&tc::session_temp_path(&fixture.session, "generation")),
            3600,
        );
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &fixture.shared.token_hash,
            &fixture.provider,
            &fixture.session,
        );
        crate::services::discord::inflight::seed_session_row_hashed(
            pool,
            &key,
            fixture.channel.get(),
            &fixture.shared.token_hash,
            None,
        )
        .await;
    }
    list_sessions(&[&protected.session, &sibling.session]);
    (protected, sibling, discord)
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    files: BTreeMap<PathBuf, (Vec<u8>, std::time::SystemTime)>,
    postgres: Vec<serde_json::Value>,
    core: Option<String>,
    mailbox: Option<String>,
    watcher: Option<String>,
    attempts: BTreeMap<&'static str, usize>,
    kills: Vec<String>,
}

async fn snapshot(fixture: &InstallFixture, row: &Path) -> Snapshot {
    snapshot_with_core(fixture, row, None).await
}

async fn snapshot_with_core(
    fixture: &InstallFixture,
    row: &Path,
    held_core: Option<Option<String>>,
) -> Snapshot {
    let directory = Path::new(&fixture.output).parent().unwrap();
    let stem = Path::new(&fixture.output)
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap();
    let mut files: BTreeMap<_, _> = test_support::tree_fingerprint(directory)
        .into_iter()
        .filter(|(path, _)| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(stem)
        })
        .collect();
    for path in [row.to_path_buf(), row.with_extension("json.lock")] {
        if let Some(value) = test_support::fingerprint(&path) {
            files.insert(path, value);
        }
    }
    let postgres = sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT to_jsonb(s) FROM sessions s WHERE channel_id = $1 ORDER BY id",
    )
    .bind(fixture.channel.get().to_string())
    .fetch_all(fixture.shared.pg_pool.as_ref().unwrap())
    .await
    .unwrap();
    let core = match held_core {
        Some(core) => core,
        None => fixture
            .shared
            .core
            .lock()
            .await
            .sessions
            .get(&fixture.channel)
            .map(|s| {
                format!(
                    "{:?}|{:?}|{:?}|{:?}|{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{:?}",
                    s.session_id,
                    s.current_path,
                    s.channel_name,
                    s.channel_id,
                    s.memento_context_loaded,
                    s.memento_reflected,
                    s.cleared,
                    s.history,
                    s.pending_uploads,
                    s.last_active,
                    s.remote_profile_name,
                    s.category_name,
                    s.born_generation,
                    s.worktree,
                )
            }),
    };
    let mailbox = if let Some(handle) = fixture.shared.mailbox_peek(fixture.channel) {
        let s = handle.snapshot().await;
        Some(format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{}|{:?}",
            s.cancel_token
                .as_ref()
                .map(|token| (Arc::as_ptr(token), token.cancelled.load(Ordering::Acquire))),
            s.active_user_message_id,
            s.active_turn_nonce,
            s.active_request_owner,
            s.active_turn_kind,
            s.pending_user_dispatch,
            s.recovery_started_at,
            s.turn_started_at,
            s.intervention_queue,
            s.active_absorbed_source_ids,
            s.pending_user_dispatch_source_ids,
            s.claim_observation,
            s.pending_user_dispatch_since,
            s.recently_valve_cleared_dispatch,
            s.pending_user_dispatch_lease_held_by_caller,
            s.cancel_token.as_ref().and_then(|token| token.turn_nonce()),
        ))
    } else {
        None
    };
    let watcher = fixture.shared.tmux_watchers.get(&fixture.channel).map(|h| {
        format!(
            "{}|{}|{:p}|{:p}|{}|{}|{:?}|{}|{}|{}",
            h.tmux_session_name,
            h.output_path,
            Arc::as_ptr(&h.cancel),
            Arc::as_ptr(&h.paused),
            h.cancel.load(Ordering::Acquire),
            h.paused.load(Ordering::Acquire),
            *h.resume_offset.lock().unwrap(),
            h.pause_epoch.load(Ordering::Acquire),
            h.turn_delivered.load(Ordering::Acquire),
            h.last_heartbeat_ts_ms.load(Ordering::Acquire),
        )
    });
    let kills = std::fs::read_to_string(root().join("tmux-kills"))
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(&fixture.session))
        .map(str::to_owned)
        .collect();
    let attempts = attempts(fixture.channel.get())
        .into_iter()
        .filter(|(effect, _)| *effect != "core_candidate")
        .collect();
    Snapshot {
        files,
        postgres,
        core,
        mailbox,
        watcher,
        attempts,
        kills,
    }
}

#[tokio::test]
async fn n3bc_protected_legacy_restore_six_modes_keeps_effects_zero_pg() {
    if !isolated(
        module_path!(),
        "n3bc_protected_legacy_restore_six_modes_keeps_effects_zero_pg",
    ) {
        return;
    }
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(20));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, mode) in [
        Mode::LegacyOpen,
        Mode::Closing,
        Mode::Frozen,
        Mode::LedgerOpen,
        Mode::Handback,
        Mode::Held,
    ]
    .into_iter()
    .enumerate()
    {
        let (protected, sibling, _discord) = pair(&pool, 632_530_000 + index as u64 * 10).await;
        let row = seed_row(&protected);
        std::fs::write(
            tc::session_dead_marker_path(&protected.session),
            "stale dead marker",
        )
        .unwrap();
        let _gate = protect(&protected, mode).await;
        let _unknown = HoldUnknownForTest::new();
        let before = snapshot(&protected, &row).await;
        assert!(before.core.is_none() && before.watcher.is_none());
        let submitted = Scope::new(&protected.shared);
        restore_tmux_watchers(&protected.http, &protected.shared).await;
        assert_eq!(
            attempts(protected.channel.get())
                .get("core_candidate")
                .copied()
                .unwrap_or(0),
            0,
            "protected channel never enters the Core registration candidates mode={mode:?}"
        );
        assert_eq!(
            snapshot(&protected, &row).await,
            before,
            "protected Legacy restore mode={mode:?}"
        );
        assert!(
            protected
                .shared
                .core
                .lock()
                .await
                .sessions
                .contains_key(&sibling.channel),
            "Legacy sibling CoreState restored mode={mode:?}"
        );
        assert!(
            protected
                .shared
                .tmux_watchers
                .contains_key(&sibling.channel),
            "Legacy sibling watcher restored mode={mode:?}"
        );
        assert_eq!(
            submitted.submissions(),
            1,
            "only the Legacy sibling spawns mode={mode:?}"
        );
        crate::services::discord::tmux_reaper::cleanup_orphan_tmux_sessions(&protected.shared)
            .await;
        assert_eq!(
            snapshot(&protected, &row).await,
            before,
            "public orphan preserves the restored protected nonowner mode={mode:?}"
        );
        submitted.cancel_and_join_all().await;
    }
    pool.close().await;
    db.drop().await;
}

async fn run_stage(pool: &sqlx::PgPool, stage: &'static str, channel: u64) {
    let (protected, sibling, discord) = pair(pool, channel).await;
    let row = seed_row(&protected);
    let dispatch = if stage == "mailbox" {
        Some(seed_dispatch_rebind(&protected, &row).await)
    } else {
        None
    };
    let late_row = (stage == "placeholder").then(|| {
        std::fs::remove_file(&row).unwrap();
        arm(channel, "core_lock")
    });
    if stage == "dead_probe" {
        std::fs::write(
            tc::session_dead_marker_path(&protected.session),
            "old live-pane tombstone",
        )
        .unwrap();
    }
    if stage == "recovery_snapshot" {
        use crate::services::provider::CancelToken;
        use crate::services::turn_orchestrator::{ChannelMailboxRegistry, RecoveryKickoffResult};
        assert_eq!(
            protected
                .shared
                .mailbox(protected.channel)
                .recovery_kickoff(
                    Arc::new(CancelToken::from_persisted_turn_nonce(Some(format!(
                        "n3bc-nonce-{channel}"
                    )))),
                    serenity::model::id::UserId::new(7),
                    Some(serenity::model::id::MessageId::new(111)),
                )
                .await,
            RecoveryKickoffResult::Activated
        );
        ChannelMailboxRegistry::global_recovery_done(protected.channel)
            .unwrap()
            .mark_done();
    }
    let original_core = snapshot(&protected, &row).await.core;
    assert!(original_core.is_none());
    let pause = arm(channel, stage);
    let submitted = Scope::new(&protected.shared);
    let restore = restore_tmux_watchers(&protected.http, &protected.shared);
    tokio::pin!(restore);
    if let Some(late_row) = late_row {
        tokio::select! {
            _ = late_row.entered.notified() => {},
            () = &mut restore => panic!("restore completed before late-row Core boundary"),
            _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("restore missed late-row Core boundary"),
        }
        assert!(
            !row.exists(),
            "the scan had no row to turn into a RestoredWatcherTurn"
        );
        assert_eq!(seed_row(&protected), row);
        late_row.resume.notify_one();
    }
    tokio::select! {
        _ = pause.entered.notified() => {},
        () = &mut restore => panic!("restore completed before {stage} latch"),
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("restore missed {stage} latch"),
    }
    if let Some(dispatch) = dispatch {
        let rebound = sqlx::query_scalar::<_, Option<String>>(
            "SELECT active_dispatch_id FROM sessions WHERE channel_id = $1",
        )
        .bind(channel.to_string())
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            rebound.as_deref(),
            Some(dispatch.as_str()),
            "mailbox latch follows the completed actual PG CAS"
        );
    }
    if stage == "placeholder" {
        assert!(
            mutations(&discord, channel)
                .iter()
                .any(|request| request.starts_with("DELETE ")),
            "placeholder latch follows an actual Discord DELETE"
        );
        assert!(
            protected
                .shared
                .ui
                .placeholder_cleanup
                .terminal_cleanup_committed(
                    &protected.provider,
                    protected.channel,
                    serenity::model::id::MessageId::new(112),
                ),
            "placeholder latch follows successful HTTP response and committed local cleanup"
        );
    }
    let _gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
    let _unknown = HoldUnknownForTest::new();
    let before = if stage == "core_locked" {
        snapshot_with_core(&protected, &row, Some(original_core)).await
    } else {
        snapshot(&protected, &row).await
    };
    let http_before = mutations(&discord, channel);
    pause.resume.notify_one();
    tokio::time::timeout(Duration::from_secs(15), restore)
        .await
        .expect("latched restore finishes");
    assert_eq!(
        snapshot(&protected, &row).await,
        before,
        "effects after protection at {stage}"
    );
    assert_eq!(
        mutations(&discord, channel),
        http_before,
        "HTTP effects after protection at {stage}"
    );
    assert!(
        protected
            .shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&sibling.channel),
        "Legacy sibling CoreState restored at {stage}"
    );
    assert!(
        protected
            .shared
            .tmux_watchers
            .contains_key(&sibling.channel),
        "Legacy sibling watcher restored at {stage}"
    );
    assert_eq!(
        submitted.submissions(),
        1,
        "only sibling spawned at {stage}"
    );
    crate::services::discord::tmux_reaper::cleanup_orphan_tmux_sessions(&protected.shared).await;
    assert_eq!(
        snapshot(&protected, &row).await,
        before,
        "public orphan after latch {stage}"
    );
    assert_eq!(mutations(&discord, channel), http_before);
    submitted.cancel_and_join_all().await;
}

#[tokio::test]
async fn n3bc_restore_named_await_windows_preserve_installation_snapshot_pg() {
    if !isolated(
        module_path!(),
        "n3bc_restore_named_await_windows_preserve_installation_snapshot_pg",
    ) {
        return;
    }
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(20));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (index, stage) in [
        "scan",
        "recovery_snapshot",
        "generation",
        "dead_probe",
        "mailbox",
        "core_locked",
        "placeholder",
        "host",
    ]
    .into_iter()
    .enumerate()
    {
        run_stage(&pool, stage, 632_531_000 + index as u64 * 10).await;
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_restore_real_core_lock_wait_rechecks_protection_pg() {
    if !isolated(
        module_path!(),
        "n3bc_restore_real_core_lock_wait_rechecks_protection_pg",
    ) {
        return;
    }
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(20));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (protected, sibling, _discord) = pair(&pool, 632_532_000).await;
    let row = seed_row(&protected);
    let pause = arm(protected.channel.get(), "core_lock");
    let submitted = Scope::new(&protected.shared);
    let restore = restore_tmux_watchers(&protected.http, &protected.shared);
    tokio::pin!(restore);
    tokio::select! {
        _ = pause.entered.notified() => {},
        () = &mut restore => panic!("restore completed before Core lock"),
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("restore missed Core lock"),
    }
    let core = protected.shared.core.lock().await;
    assert!(!core.sessions.contains_key(&protected.channel));
    pause.resume.notify_one();
    tokio::select! {
        biased;
        () = &mut restore => panic!("restore cannot finish while its Core lock is held"),
        _ = tokio::time::sleep(Duration::from_millis(30)) => {},
    }
    let _gate = Gate::protect(protected.provider.clone(), protected.channel.get()).unwrap();
    let _unknown = HoldUnknownForTest::new();
    let before = snapshot_with_core(&protected, &row, Some(None)).await;
    drop(core);
    tokio::time::timeout(Duration::from_secs(15), restore)
        .await
        .expect("restore completes after Core lock release");
    assert_eq!(
        snapshot(&protected, &row).await,
        before,
        "protection installed during the real Core lock wait"
    );
    assert!(
        protected
            .shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&sibling.channel)
    );
    assert!(
        protected
            .shared
            .tmux_watchers
            .contains_key(&sibling.channel)
    );
    assert_eq!(submitted.submissions(), 1);
    crate::services::discord::tmux_reaper::cleanup_orphan_tmux_sessions(&protected.shared).await;
    assert_eq!(snapshot(&protected, &row).await, before);
    submitted.cancel_and_join_all().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_restore_other_provider_protection_is_channel_wide_pg() {
    if !isolated(
        module_path!(),
        "n3bc_restore_other_provider_protection_is_channel_wide_pg",
    ) {
        return;
    }
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(20));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (protected, sibling, _discord) = pair(&pool, 632_532_100).await;
    let row = seed_row(&protected);
    let _gate = Gate::protect(ProviderKind::Codex, protected.channel.get()).unwrap();
    let _unknown = HoldUnknownForTest::new();
    let before = snapshot(&protected, &row).await;
    let submitted = Scope::new(&protected.shared);
    restore_tmux_watchers(&protected.http, &protected.shared).await;
    assert_eq!(snapshot(&protected, &row).await, before);
    assert_eq!(
        attempts(protected.channel.get())
            .get("core_candidate")
            .copied()
            .unwrap_or(0),
        0
    );
    assert!(
        protected
            .shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&sibling.channel)
    );
    assert!(
        protected
            .shared
            .tmux_watchers
            .contains_key(&sibling.channel)
    );
    assert_eq!(submitted.submissions(), 1);
    crate::services::discord::tmux_reaper::cleanup_orphan_tmux_sessions(&protected.shared).await;
    assert_eq!(snapshot(&protected, &row).await, before);
    submitted.cancel_and_join_all().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_restore_off_registry_keeps_legacy_adoption_and_old_row_semantics_pg() {
    if !isolated(
        module_path!(),
        "n3bc_restore_off_registry_keeps_legacy_adoption_and_old_row_semantics_pg",
    ) {
        return;
    }
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(20));
    assert!(!crate::services::discord::input_runtime::fence::any_protected());
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (legacy, sibling, _discord) = pair(&pool, 632_532_200).await;
    let row = seed_row(&legacy);
    let generation = PathBuf::from(tc::session_temp_path(&legacy.session, "generation"));
    let old_mtime = std::fs::metadata(&generation).unwrap().modified().unwrap();
    std::fs::write(
        tc::session_dead_marker_path(&legacy.session),
        "stale live-pane tombstone",
    )
    .unwrap();
    let row_before = test_support::fingerprint(&row).unwrap();
    let submitted = Scope::new(&legacy.shared);
    restore_tmux_watchers(&legacy.http, &legacy.shared).await;
    assert!(
        !crate::services::discord::input_runtime::fence::any_protected(),
        "off means the actual registry is empty"
    );
    assert_eq!(std::fs::read_to_string(&generation).unwrap(), "20");
    assert_eq!(
        std::fs::metadata(&generation).unwrap().modified().unwrap(),
        old_mtime,
        "old numeric generation adoption preserves wrapper identity mtime"
    );
    assert!(
        !Path::new(&tc::session_dead_marker_path(&legacy.session)).exists(),
        "live pane clears the old marker off"
    );
    assert_ne!(
        test_support::fingerprint(&row).unwrap(),
        row_before,
        "old row is compatibility-backfilled and readopted off"
    );
    let mailbox = legacy
        .shared
        .mailbox_peek(legacy.channel)
        .unwrap()
        .snapshot()
        .await;
    assert!(
        mailbox.cancel_token.is_some() && mailbox.active_user_message_id.is_some(),
        "Legacy row registers its mailbox off"
    );
    for fixture in [&legacy, &sibling] {
        assert!(
            legacy
                .shared
                .core
                .lock()
                .await
                .sessions
                .contains_key(&fixture.channel)
        );
        assert!(legacy.shared.tmux_watchers.contains_key(&fixture.channel));
    }
    assert_eq!(submitted.submissions(), 2);
    submitted.cancel_and_join_all().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn n3bc_restore_recovery_signal_wait_preserves_marker_after_protection_pg() {
    if !isolated(
        module_path!(),
        "n3bc_restore_recovery_signal_wait_preserves_marker_after_protection_pg",
    ) {
        return;
    }
    use crate::services::provider::CancelToken;
    use crate::services::turn_orchestrator::{ChannelMailboxRegistry, RecoveryKickoffResult};
    crate::services::discord::runtime_store::set_process_generation_for_tests(Some(20));
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (protected, sibling, _discord) = pair(&pool, 632_532_300).await;
    let row = seed_row(&protected);
    assert_eq!(
        protected
            .shared
            .mailbox(protected.channel)
            .recovery_kickoff(
                Arc::new(CancelToken::from_persisted_turn_nonce(Some(
                    "recovery-signal-window".into()
                ))),
                serenity::model::id::UserId::new(7),
                Some(serenity::model::id::MessageId::new(111)),
            )
            .await,
        RecoveryKickoffResult::Activated
    );
    let signal = ChannelMailboxRegistry::global_recovery_done(protected.channel).unwrap();
    let pause = arm(protected.channel.get(), "recovery_snapshot");
    let submitted = Scope::new(&protected.shared);
    let restore = restore_tmux_watchers(&protected.http, &protected.shared);
    tokio::pin!(restore);
    tokio::select! {
        _ = pause.entered.notified() => {},
        () = &mut restore => panic!("restore completed before recovery snapshot"),
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("restore missed recovery snapshot"),
    }
    pause.resume.notify_one();
    tokio::select! {
        biased;
        () = &mut restore => panic!("recovery signal must remain pending before completion"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {},
    }
    let _gate = Gate::protect(protected.provider.clone(), protected.channel.get()).unwrap();
    let _unknown = HoldUnknownForTest::new();
    let before = snapshot(&protected, &row).await;
    assert!(
        protected
            .shared
            .mailbox_peek(protected.channel)
            .unwrap()
            .snapshot()
            .await
            .recovery_started_at
            .is_some()
    );
    signal.mark_done();
    tokio::time::timeout(Duration::from_secs(15), restore)
        .await
        .expect("recovery signal continuation finishes");
    assert_eq!(
        snapshot(&protected, &row).await,
        before,
        "signal response cannot clear the protected recovery marker"
    );
    assert!(
        protected
            .shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&sibling.channel)
    );
    assert!(
        protected
            .shared
            .tmux_watchers
            .contains_key(&sibling.channel)
    );
    assert_eq!(submitted.submissions(), 1);
    crate::services::discord::tmux_reaper::cleanup_orphan_tmux_sessions(&protected.shared).await;
    assert_eq!(snapshot(&protected, &row).await, before);
    submitted.cancel_and_join_all().await;
    pool.close().await;
    db.drop().await;
}
