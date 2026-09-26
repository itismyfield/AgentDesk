//! Custody notice contracts: episodes come from the real boot custody pass, and each notice is a
//! row in a real PostgreSQL message_outbox.

use super::custody_notice::{enqueue_custody_notices, notices};
use super::nondestructive_loader_tests::{CLAUDE, Env, G, row};
use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use crate::services::message_outbox::delivery_bot_for_target_session;
use serde_json::json;
use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// target, bot, source, reason code, session key, content and status of one outbox row.
type Row = (String, String, String, String, String, String, String);

/// The custody env first, then the database, in the harness's lock order.
async fn harness(label: &str) -> Option<(Env, DispatchPostgresTestDb, PgPool)> {
    let env = Env::new();
    let db = DispatchPostgresTestDb::try_create("agentdesk_boot_custody_notice", label).await?;
    let pool = db.connect_and_migrate().await;
    Some((env, db, pool))
}

async fn finish(db: DispatchPostgresTestDb, pool: PgPool) {
    pool.close().await;
    db.drop().await;
}

/// One boot: the real custody pass, then the notice pass over what it left.
async fn boot(env: &Env, pool: Option<&PgPool>) -> usize {
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    enqueue_custody_notices(&custody(env), &CLAUDE, pool).await
}

async fn rows(pool: &PgPool) -> Vec<Row> {
    sqlx::query_as(
        "SELECT target, bot, source, reason_code, session_key, content, status
         FROM message_outbox WHERE source = 'boot_custody_notice' ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Marks every notice row `status`, created ten minutes ago, past any rolling dedupe window.
async fn settle(pool: &PgPool, status: &str) {
    sqlx::query(
        "UPDATE message_outbox SET status = $1,
         created_at = created_at - INTERVAL '10 minutes',
         dedupe_expires_at = dedupe_expires_at - INTERVAL '10 minutes'",
    )
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

fn custody(env: &Env) -> PathBuf {
    env.dir().with_file_name("discord_custody").join("claude")
}

fn episode_dirs(env: &Env) -> Vec<PathBuf> {
    let dirs = fs::read_dir(custody(env)).into_iter().flatten().flatten();
    let mut dirs: Vec<PathBuf> = dirs.map(|dir| dir.path()).collect();
    dirs.sort();
    dirs
}

fn name(dir: &Path) -> String {
    dir.file_name().unwrap().to_string_lossy().into_owned()
}

/// Every file under `dir` with its bytes.
fn snapshot(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let (mut files, mut stack) = (Vec::new(), vec![dir.to_path_buf()]);
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            stack.extend(fs::read_dir(&path).unwrap().flatten().map(|e| e.path()));
        } else {
            files.push((path.clone(), fs::read(&path).unwrap()));
        }
    }
    files.sort();
    files
}

/// Seeds an owner-1 ExternalInput row whose transcript turn exists or is missing.
fn tui_direct(env: &Env, channel_id: u64, transcript: bool) -> InflightTurnState {
    let mut state = row(channel_id, Some("AgentDesk-claude-notice"));
    (state.request_owner_user_id, state.turn_source) = (1, TurnSource::ExternalInput);
    let out = env.dir().with_file_name(format!("{channel_id}.jsonl"));
    fs::create_dir_all(env.dir()).unwrap();
    if transcript {
        fs::write(&out, "turn\n").unwrap();
    }
    (state.output_path, state.turn_start_offset) = (Some(out.display().to_string()), Some(0));
    env.seed(&state, 0);
    state
}

/// Writes an earlier build's episode of `turn`, keyed by its start time and offset 0.
fn earlier_build_episode(env: &Env, turn: &InflightTurnState, episode: &str) {
    let (dir, row) = (
        custody(env).join(episode),
        serde_json::to_vec(turn).unwrap(),
    );
    let marker = json!({ "episode": { "provider": "claude", "channel_id": turn.channel_id,
        "anchor_id": 0, "anchorless": [turn.started_at, 0] }, "tui_direct": true });
    fs::create_dir_all(dir.join("rev-0000")).unwrap();
    fs::write(dir.join("episode.json"), marker.to_string()).unwrap();
    fs::write(dir.join("rev-0000/0-row.json"), row).unwrap();
    fs::write(dir.join("rev-0000/manifest.json"), r#"{"complete":true}"#).unwrap();
}

/// Seeds a pending-start record for `tmux`, as the writer persists one before any inflight row.
fn pending_start(channel_id: u64, tmux: &str) {
    let root = crate::services::discord::runtime_store::tui_direct_pending_start_root().unwrap();
    let record = json!({ "provider": "claude", "channel_id": channel_id, "tmux_session_name": tmux,
        "prompt_text": "queued", "anchor_message_id": channel_id + 1,
        "lease_relay_owner": "watcher", "generation": G, "created_at_ms": 1, "observed_at_ms": 1 });
    let name = format!("claude_{channel_id}_{}.json", channel_id + 1);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join(name), record.to_string()).unwrap();
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `run` with this thread's warnings captured as text.
fn warnings<T>(run: impl FnOnce() -> T) -> (T, String) {
    let (logs, fmt) = (Logs::default(), tracing_subscriber::fmt().with_ansi(false));
    let writer = logs.clone();
    let subscriber = fmt.with_max_level(tracing::Level::WARN);
    let subscriber = subscriber.with_writer(move || writer.clone()).finish();
    crate::logging::test_capture::pin_callsite_interest();
    let out = tracing::subscriber::with_default(subscriber, run);
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    (out, text)
}

// Contract: a TUI-direct turn in custody is one notify-bot row for its channel naming the episode,
// and boots after that row was sent, past any rolling window, add none.
#[tokio::test(flavor = "current_thread")]
async fn a_tui_direct_turn_is_one_outbox_row_across_boots_pg() {
    let Some((env, db, pool)) = harness("boot custody notice across boots").await else {
        return;
    };
    tui_direct(&env, 5_998_001, true);
    assert_eq!(boot(&env, Some(&pool)).await, 1);
    settle(&pool, "sent").await;
    boot(&env, Some(&pool)).await;
    boot(&env, Some(&pool)).await;

    let [dir] = episode_dirs(&env).try_into().unwrap();
    let [(target, bot, source, reason, session, text, status)] =
        rows(&pool).await.try_into().unwrap();
    let fields = [&target, &bot, &source, &reason, &status].map(String::as_str);
    let (source_label, reason_code) = ("boot_custody_notice", "boot_custody.notice");
    assert_eq!(
        fields,
        [
            "channel:5998001",
            "notify",
            source_label,
            reason_code,
            "sent"
        ]
    );
    assert_eq!(
        delivery_bot_for_target_session(&target, &bot, Some(&session)),
        "notify"
    );
    assert!(
        text.contains(&name(&dir)) && !text.contains("보존 실패"),
        "{text}"
    );
    finish(db, pool).await;
}

// Contract: a notice row that failed for good releases its key, and the next boot enqueues it
// again.
#[tokio::test(flavor = "current_thread")]
async fn a_failed_notice_row_is_enqueued_again_by_the_next_boot_pg() {
    let Some((env, db, pool)) = harness("boot custody notice after a failed row").await else {
        return;
    };
    tui_direct(&env, 5_998_011, true);
    boot(&env, Some(&pool)).await;
    settle(&pool, "failed").await;
    boot(&env, Some(&pool)).await;

    let statuses: Vec<String> = rows(&pool).await.into_iter().map(|row| row.6).collect();
    assert_eq!(statuses, ["failed", "pending"]);
    finish(db, pool).await;
}

// Contract: two anchorless turns the real constructor starts in one second with the same start
// inputs are two rows.
#[tokio::test(flavor = "current_thread")]
async fn anchorless_turns_started_in_the_same_second_are_two_rows_pg() {
    let Some((env, db, pool)) = harness("boot custody notice same second").await else {
        return;
    };
    let seeded = tui_direct(&env, 5_998_021, true);
    let (tmux, out) = (seeded.tmux_session_name, seeded.output_path);
    let start = || {
        let (text, tmux, out) = (String::new(), tmux.clone(), out.clone());
        let mut turn = InflightTurnState::new(
            CLAUDE, 5_998_021, None, 1, 0, 0, text, None, tmux, out, None, 0,
        );
        turn.turn_source = TurnSource::ExternalInput;
        turn
    };
    let (first, second) = loop {
        let pair = (start(), start());
        if pair.0.started_at == pair.1.started_at {
            break pair;
        }
    };
    assert_eq!(first.finalizer_turn_id, second.finalizer_turn_id);
    for turn in [&first, &second] {
        env.seed(turn, 0);
        boot(&env, Some(&pool)).await;
    }
    assert_eq!(rows(&pool).await.len(), 2);
    finish(db, pool).await;
}

// Contract: an earlier build's episode of an anchorless turn, keyed by start time, is one row of
// its own beside the turn's current episode, which stays one row after its offset is rewritten.
#[tokio::test(flavor = "current_thread")]
async fn an_earlier_builds_episode_of_a_turn_is_one_row_of_its_own_pg() {
    let Some((env, db, pool)) = harness("boot custody notice earlier build").await else {
        return;
    };
    let mut turn = tui_direct(&env, 5_998_031, true);
    turn.user_msg_id = 0;
    earlier_build_episode(&env, &turn, &"0".repeat(64));
    for offset in [0, 1] {
        turn.turn_start_offset = Some(offset);
        env.seed(&turn, 0);
        boot(&env, Some(&pool)).await;
    }
    assert_eq!((episode_dirs(&env).len(), rows(&pool).await.len()), (2, 2));
    finish(db, pool).await;
}

// Contract: a row written without a finalizer turn id or turn nonce, as older builds wrote it,
// is one row across boots and after the id is written back.
#[tokio::test(flavor = "current_thread")]
async fn a_row_without_persisted_turn_ids_is_one_row_across_boots_and_backfill_pg() {
    let Some((env, db, pool)) = harness("boot custody notice raw row").await else {
        return;
    };
    let mut turn = tui_direct(&env, 5_998_041, true);
    turn.user_msg_id = 0;
    let path = env.seed(&turn, 0);
    let mut raw = serde_json::to_value(&turn).unwrap();
    for field in ["finalizer_turn_id", "turn_nonce"] {
        raw.as_object_mut().unwrap().remove(field);
    }
    for backfill in [false, false, true] {
        fs::write(&path, raw.to_string()).unwrap();
        if backfill {
            backfill_finalizer_turn_id_under_lock(&env.dir(), &path, &CLAUDE).unwrap();
        }
        boot(&env, Some(&pool)).await;
    }
    assert_eq!((episode_dirs(&env).len(), rows(&pool).await.len()), (1, 1));
    finish(db, pool).await;
}

// Contract: no custody, or custody that holds no TUI-direct turn, enqueues nothing.
#[tokio::test(flavor = "current_thread")]
async fn custody_without_a_tui_direct_turn_enqueues_nothing_pg() {
    let Some((env, db, pool)) = harness("boot custody notice without a turn").await else {
        return;
    };
    assert_eq!(boot(&env, Some(&pool)).await, 0);
    env.seed(&row(5_998_051, None), 0);
    assert_eq!(boot(&env, Some(&pool)).await, 0);
    assert_eq!((episode_dirs(&env).len(), rows(&pool).await.len()), (1, 0));
    finish(db, pool).await;
}

// Contract: without PostgreSQL, or with one that refuses connections, the pass returns, enqueues
// nothing and leaves custody unchanged; the next boot with PostgreSQL enqueues the notice.
#[tokio::test(flavor = "current_thread")]
async fn without_postgres_the_notice_waits_for_the_next_boot_pg() {
    let Some((env, db, pool)) = harness("boot custody notice fail-open").await else {
        return;
    };
    tui_direct(&env, 5_998_061, true);
    assert_eq!(boot(&env, None).await, 0);
    let (dir, dead_url) = (custody(&env), "postgres://postgres@127.0.0.1:1/none");
    let before = snapshot(&dir);
    let dead = sqlx::postgres::PgPoolOptions::new().acquire_timeout(Duration::from_secs(2));
    let dead = dead.connect_lazy(dead_url).unwrap();
    let pass = enqueue_custody_notices(&dir, &CLAUDE, Some(&dead));
    let passed = tokio::time::timeout(Duration::from_secs(30), pass).await;
    assert_eq!((passed, snapshot(&dir) == before), (Ok(0), true));
    assert!(rows(&pool).await.is_empty());
    assert_eq!(boot(&env, Some(&pool)).await, 1);
    assert_eq!(rows(&pool).await.len(), 1);
    finish(db, pool).await;
}

// Contract: the boot reaper's first caller starts the notice pass with the pool it is given.
#[tokio::test(flavor = "current_thread")]
async fn the_boot_reaper_starts_the_notice_pass_pg() {
    let Some((env, db, pool)) = harness("boot custody notice from the reaper").await else {
        return;
    };
    tui_direct(&env, 5_998_071, true);
    let guard = BootReapOnce::default();
    reap_inflight_rows_at_boot_with_guard(&guard, &CLAUDE, Some(pool.clone())).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while rows(&pool).await.is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rows(&pool).await.len(), 1);
    finish(db, pool).await;
}

// Contract: the notice says output may be missing, names the episode, reports a failed copy,
// and promises no retry or later action.
#[tokio::test]
async fn the_notice_reports_a_failed_copy_and_promises_nothing_more() {
    let env = Env::new();
    tui_direct(&env, 5_998_081, false);
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    let [notice] = notices(&custody(&env), &CLAUDE).try_into().unwrap();
    let ([dir], text) = (episode_dirs(&env).try_into().unwrap(), notice.text);
    let lines = [
        "재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음",
        &name(&dir),
        "보존 실패",
    ];
    assert!(lines.iter().all(|line| text.contains(line)), "{text}");
    for promise in ["재시도", "다시 보", "나중에", "확인하겠", "상태"] {
        assert!(!text.contains(promise), "{text}");
    }
}

// Contract: a DM session's notice is sent by the provider bot that owns the DM.
#[tokio::test]
async fn a_dm_sessions_notice_is_delivered_by_the_provider_bot() {
    let env = Env::new();
    let mut turn = tui_direct(&env, 5_998_091, true);
    turn.tmux_session_name = Some("AgentDesk-claude-dm-343742347".to_string());
    env.seed(&turn, 0);
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    let [notice] = notices(&custody(&env), &CLAUDE).try_into().unwrap();
    assert_eq!(notice.target, "channel:5998091");
    let bot = delivery_bot_for_target_session(&notice.target, &notice.bot, Some(&notice.session));
    assert_eq!(bot, "claude");
}

// Contract: a DM turn that custody holds only as its pending-start record is enqueued for the
// provider bot that owns the DM.
#[tokio::test(flavor = "current_thread")]
async fn a_dm_turn_held_only_as_a_pending_start_is_sent_by_the_provider_bot_pg() {
    let Some((env, db, pool)) = harness("boot custody notice pending-start dm").await else {
        return;
    };
    pending_start(5_998_111, "AgentDesk-claude-dm-343742347");
    assert_eq!(boot(&env, Some(&pool)).await, 1);
    let [(target, bot, _, _, session, ..)] = rows(&pool).await.try_into().unwrap();
    let bot = delivery_bot_for_target_session(&target, &bot, Some(&session));
    assert_eq!(bot, "claude");
    finish(db, pool).await;
}

// Contract: an episode is one row whatever copies a custody root holds: another node's root with
// only the same markers, and this root with its copies gone, add none.
#[tokio::test(flavor = "current_thread")]
async fn an_episode_is_one_row_whatever_copies_its_root_holds_pg() {
    let Some((env, db, pool)) = harness("boot custody notice copy availability").await else {
        return;
    };
    tui_direct(&env, 5_998_101, true);
    let mut anchorless = tui_direct(&env, 5_998_102, true);
    anchorless.user_msg_id = 0;
    env.seed(&anchorless, 0);
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    let (dirs, markers_only) = (episode_dirs(&env), env.dir().with_file_name("other_node"));
    for dir in &dirs {
        let other = markers_only.join(name(dir));
        fs::create_dir_all(&other).unwrap();
        fs::copy(dir.join("episode.json"), other.join("episode.json")).unwrap();
    }
    enqueue_custody_notices(&markers_only, &CLAUDE, Some(&pool)).await;
    settle(&pool, "sent").await;
    boot(&env, Some(&pool)).await;
    let revs = dirs
        .iter()
        .flat_map(|dir| fs::read_dir(dir).unwrap().flatten());
    let revs: Vec<PathBuf> = revs
        .map(|rev| rev.path())
        .filter(|rev| rev.is_dir())
        .collect();
    for (n, rev) in revs.iter().enumerate() {
        fs::rename(rev, env.dir().with_file_name(format!("moved-{n}"))).unwrap();
    }
    enqueue_custody_notices(&custody(&env), &CLAUDE, Some(&pool)).await;
    assert_eq!(
        (dirs.len(), revs.is_empty(), rows(&pool).await.len()),
        (2, false, 2)
    );
    finish(db, pool).await;
}

// Contract: an episode whose marker cannot be read or parsed is a warning naming its path and
// the failed step; the other episodes still get their notice and custody is left as it was.
#[tokio::test]
async fn an_unreadable_or_garbled_marker_is_warned_and_skipped() {
    let env = Env::new();
    tui_direct(&env, 5_998_121, true);
    let (unreadable, garbled) = (
        custody(&env).join("a".repeat(64)),
        custody(&env).join("b".repeat(64)),
    );
    fs::create_dir_all(unreadable.join("episode.json")).unwrap();
    fs::create_dir_all(&garbled).unwrap();
    fs::write(garbled.join("episode.json"), "{not json").unwrap();
    reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE, None).await;
    let before = snapshot(&custody(&env));
    let (found, logs) = warnings(|| notices(&custody(&env), &CLAUDE));
    assert_eq!((found.len(), snapshot(&custody(&env)) == before), (1, true));
    for (dir, step) in [
        (&unreadable, "read episode.json"),
        (&garbled, "parse episode.json"),
    ] {
        let line = logs
            .lines()
            .find(|line| line.contains(&dir.display().to_string()));
        assert!(line.is_some_and(|line| line.contains(step)), "{logs}");
    }
}
