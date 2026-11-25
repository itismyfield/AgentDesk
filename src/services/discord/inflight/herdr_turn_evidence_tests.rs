//! A recorded Herdr start, its durable row and the watcher's evidence after token loss; the
//! watcher paths' tests take their Herdr rows from `mark_herdr`.
use super::herdr_turn_evidence::{HerdrTurnEvidence, PersistedHerdrTurnStart};
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedOwner, HostedState};
use crate::services::discord::turn_finalizer::CompletionSignal::{Done, PausedLive};
use crate::services::provider::CancelToken;
use crate::services::provider::cancel_token_claude_interrupt::{
    HerdrInterruptState, HerdrTurnStart,
};
use crate::services::session_host::{HostKind, HostedRuntimeLocator};
use serde_json::json;
use std::sync::Arc;

const LOGICAL: &str = "AgentDesk-herdr-evidence-unlisted";
const EXEC: &str = "exec-a";

/// A Herdr row's saved start, the channel's current hosted record and its own terminal on disk.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in crate::services::discord) struct HerdrCase {
    start: bool,
    hosted: (&'static str, HostedState),
    terminal: bool,
}

/// What a watcher path must hold: no start, another current execution, a Pending or Retired one,
/// no own terminal yet. `OWN` is the end it settles.
pub(in crate::services::discord) const HELD: [HerdrCase; 5] = [
    case(false, EXEC, HostedState::Bound, true),
    case(true, "exec-b", HostedState::Bound, true),
    case(true, EXEC, HostedState::Pending, true),
    case(true, EXEC, HostedState::Retired, true),
    case(true, EXEC, HostedState::Bound, false),
];
pub(in crate::services::discord) const OWN: HerdrCase = case(true, EXEC, HostedState::Bound, true);

const fn case(start: bool, nonce: &'static str, state: HostedState, terminal: bool) -> HerdrCase {
    let hosted = (nonce, state);
    HerdrCase {
        start,
        hosted,
        terminal,
    }
}

/// Makes `row` a Claude Herdr turn of `EXEC` read from `path`, with the channel's hosted record.
pub(in crate::services::discord) async fn mark_herdr(
    pool: &sqlx::PgPool,
    token_hash: &str,
    row: &mut InflightTurnState,
    path: &Path,
    case: HerdrCase,
) {
    let at = chrono::Utc::now();
    let turn = claude_turn(at + chrono::Duration::seconds(1));
    let prompt = turn.lines().next().unwrap().to_owned() + "\n";
    std::fs::write(path, if case.terminal { turn } else { prompt }).unwrap();
    row.host_locator = Some(herdr_locator());
    row.hosted_execution_nonce = Some(EXEC.into());
    row.herdr_turn_start = case.start.then(|| PersistedHerdrTurnStart {
        turn_nonce: row.turn_nonce.clone().unwrap(),
        start: HerdrTurnStart::at_end_of(EXEC, path, Some(at)),
    });
    hosted(pool, token_hash, row, case.hosted).await;
}

/// The channel's current hosted record for `row`'s provider.
async fn hosted(
    pool: &sqlx::PgPool,
    token_hash: &str,
    row: &InflightTurnState,
    (nonce, state): (&str, HostedState),
) {
    use crate::db::dispatched_sessions::hosted_execution::tests::{record, wire};
    let (provider, channel) = (row.provider.as_str(), row.channel_id.to_string());
    let owner = owner(provider, &channel, token_hash);
    let mut record = record(&owner, nonce, state);
    if let Some(expected) = record.expected.as_mut() {
        expected.binding_provider = provider.into();
    }
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, identity_kind,
                               discord_token_hash, channel_id, hosted_execution)
         VALUES ($1, $2, 'idle', 'discord_channel', $3, $4, $5)",
    )
    .bind(format!("{provider}/herdr-evidence-{channel}"))
    .bind(provider)
    .bind(token_hash)
    .bind(&channel)
    .bind(wire(&record))
    .execute(pool)
    .await
    .unwrap();
}

fn herdr_locator() -> host_locator::PersistedHostLocator {
    host_locator::PersistedHostLocator::Known(HostedRuntimeLocator {
        execution_node: None,
        host_kind: HostKind::Herdr,
        host_session_id: "herdr".into(),
        pane: Some("p1".into()),
    })
}

fn identity(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).unwrap();
    (meta.dev(), meta.ino())
}

fn setup() -> (tempfile::TempDir, crate::config::TestEnvVarGuard, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    (temp, env, inflight_runtime_root().unwrap())
}

fn claude_turn(at: chrono::DateTime<chrono::Utc>) -> String {
    let at = at.to_rfc3339();
    [
        json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": "q"}}),
        json!({"type": "assistant", "timestamp": at, "message": {"role": "assistant",
            "stop_reason": "end_turn", "content": [{"type": "text", "text": "답"}]}}),
        json!({"type": "system", "subtype": "turn_duration", "timestamp": at}),
    ]
    .map(|record| format!("{record}\n"))
    .concat()
}

fn codex_turn(turn: &str) -> String {
    let record =
        |kind: &str| json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn}});
    format!("{}\n{}\n", record("task_started"), record("task_complete"))
}

/// A saved row of `channel` on a Herdr host (or plain tmux) whose transcript holds `body`.
fn row(
    root: &Path,
    provider: &ProviderKind,
    channel: u64,
    body: &str,
    herdr: bool,
) -> (PathBuf, InflightTurnState) {
    std::fs::create_dir_all(root).unwrap();
    let path = root.join(format!("{channel}.jsonl"));
    std::fs::write(&path, body).unwrap();
    let mut row = InflightTurnState::new(
        provider.clone(),
        channel,
        None,
        1,
        77,
        18,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    // Production does not yet bind a managed Codex row's hosted nonce (S3b); fixtures set it here.
    row.hosted_execution_nonce = Some(format!("exec-{channel}"));
    row.host_locator = herdr.then(herdr_locator);
    save_inflight_state_in_root(root, &row).unwrap();
    (path, row)
}

/// Each executor's recorded start reaches its row and survives a stale save; once the token is
/// gone the watcher holds the row on an earlier turn's terminal and ends it on its own terminal.
#[tokio::test(flavor = "current_thread")]
async fn same_herdr_row_restored_after_restart_finalizes_identically_pg() {
    let (_temp, _env, root) = setup();
    let (db, shared) = pg_shared().await;
    install_turn_start_sink();
    let at = chrono::Utc::now();
    // A later turn's open tool call and abort after t1's own terminal must not take it away.
    let later = [
        json!({"type": "event_msg", "payload": {"type": "task_started", "turn_id": "t2"}}),
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "shell",
            "arguments": "{}", "call_id": "c2"}}),
        json!({"type": "event_msg", "payload": {"type": "turn_aborted", "turn_id": "t2"}}),
    ]
    .map(|record| format!("{record}\n"))
    .concat();
    let cases = [
        (
            ProviderKind::Claude,
            claude_turn(at - chrono::Duration::seconds(60)),
            claude_turn(at + chrono::Duration::seconds(1)),
            Some(at),
        ),
        (
            ProviderKind::Codex,
            codex_turn("t0"),
            codex_turn("t1") + &later,
            None,
        ),
    ];
    for (n, (provider, previous, own, submitted_at)) in cases.into_iter().enumerate() {
        let channel = 8_399_200 + n as u64;
        let (path, stale, token, state) =
            begun(&shared, &root, &provider, channel, &previous).await;
        let nonce = stale.hosted_execution_nonce.as_deref().unwrap();
        assert!(state.record_turn_start(HerdrTurnStart::at_end_of(nonce, &path, submitted_at)));
        save_inflight_state_in_root(&root, &stale).unwrap();
        drop((state, token));
        let restored = load_inflight_state(&provider, channel);
        let held = herdr_fresh_idle_signal(&shared, &provider, restored.as_ref(), LOGICAL, Done);
        assert_eq!(held.await, PausedLive, "{provider:?}: an earlier terminal");
        std::fs::write(&path, previous + &own).unwrap();
        let ended =
            herdr_fresh_idle_signal(&shared, &provider, restored.as_ref(), LOGICAL, PausedLive);
        assert_eq!(ended.await, Done, "{provider:?}: its own terminal");
    }
    db.drop().await;
}

/// A cold start's file is pinned at the turn's first own record and no later save unpins or moves
/// it; only its own terminal in that file ends the turn.
#[tokio::test(flavor = "current_thread")]
async fn cold_start_file_is_pinned_at_its_first_own_record_pg() {
    let (_temp, _env, root) = setup();
    let (db, shared) = pg_shared().await;
    install_turn_start_sink();
    let at = chrono::Utc::now();
    let turn = claude_turn(at + chrono::Duration::seconds(1));
    let prompt = turn.lines().next().unwrap().to_owned() + "\n";
    for (n, replaced) in [false, true].into_iter().enumerate() {
        let (provider, channel) = (ProviderKind::Claude, 8_399_230 + n as u64);
        let (path, stale, _token, state) = begun(&shared, &root, &provider, channel, "").await;
        std::fs::remove_file(&path).unwrap();
        let nonce = stale.hosted_execution_nonce.as_deref().unwrap();
        assert!(state.record_turn_start(HerdrTurnStart::at_end_of(nonce, &path, Some(at))));
        let unpinned = load_inflight_state(&provider, channel).unwrap();
        std::fs::write(&path, &prompt).unwrap();
        state.source_observed(identity(&path));
        // A stale unpinned snapshot and one naming another file both keep the first pin.
        let mut other = load_inflight_state(&provider, channel).unwrap();
        other.herdr_turn_start.as_mut().unwrap().start.file = Some((0, 0));
        for snapshot in [unpinned, other] {
            save_inflight_state_in_root(&root, &snapshot).unwrap();
        }
        if replaced {
            std::fs::rename(&path, path.with_extension("first")).unwrap();
        }
        std::fs::write(&path, &turn).unwrap();
        let restored = load_inflight_state(&provider, channel);
        let signal =
            herdr_fresh_idle_signal(&shared, &provider, restored.as_ref(), LOGICAL, PausedLive);
        assert_eq!(
            signal.await,
            if replaced { PausedLive } else { Done },
            "replaced={replaced}"
        );
    }
    db.drop().await;
}

/// A saved Herdr row of `channel` whose token began its turn, with the channel's execution Bound.
async fn begun(
    shared: &crate::services::discord::SharedData,
    root: &Path,
    provider: &ProviderKind,
    channel: u64,
    body: &str,
) -> (
    PathBuf,
    InflightTurnState,
    Arc<CancelToken>,
    Arc<HerdrInterruptState>,
) {
    let (path, mut stale) = row(root, provider, channel, body, true);
    let token = Arc::new(CancelToken::new());
    stale.turn_nonce = token.turn_nonce().map(str::to_owned);
    save_inflight_state_in_root(root, &stale).unwrap();
    let bound = (
        stale.hosted_execution_nonce.as_deref().unwrap(),
        HostedState::Bound,
    );
    hosted(
        shared.pg_pool.as_ref().unwrap(),
        &shared.token_hash,
        &stale,
        bound,
    )
    .await;
    let owner = owner(provider.as_str(), &channel.to_string(), "hash");
    let state = token.prepare_herdr_interrupt(provider.clone(), &owner);
    (path, stale, token, state)
}

async fn pg_shared() -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    Arc<crate::services::discord::SharedData>,
) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().pg_pool = Some(db.connect_and_migrate().await);
    (db, shared)
}

fn owner(provider: &str, channel: &str, token_hash: &str) -> HostedOwner {
    HostedOwner {
        provider: provider.into(),
        discord_token_hash: token_hash.into(),
        channel_id: channel.into(),
        logical_key: LOGICAL.into(),
        owner_node: "test-node".into(),
        runtime_root: "/adk/runtime".into(),
    }
}

/// Before input there is no start; a start of another execution or of the previous turn on the
/// same execution names no terminal of this row, and a successor's save drops it.
#[test]
fn terminal_before_input_or_from_previous_turn_is_held() {
    let (_temp, _env, root) = setup();
    let provider = ProviderKind::Codex;
    let (path, mut row) = row(&root, &provider, 8_399_210, &codex_turn("t1"), true);
    let own = row.hosted_execution_nonce.clone().unwrap();
    let starts = [
        (None, "before input"),
        (
            Some((row.turn_nonce.clone().unwrap(), "exec-earlier".to_string())),
            "another execution",
        ),
        (
            Some(("previous-turn".into(), own.clone())),
            "the previous turn",
        ),
    ];
    for (start, case) in starts {
        row.herdr_turn_start = start.map(|(turn_nonce, execution_nonce)| PersistedHerdrTurnStart {
            turn_nonce,
            start: HerdrTurnStart {
                execution_nonce,
                source: path.clone(),
                file: Some(identity(&path)),
                offset: 0,
                submitted_at: None,
            },
        });
        let evidence = row.herdr_turn_evidence(LOGICAL, Some(own.as_str()));
        assert_eq!(evidence, HerdrTurnEvidence::Insufficient, "{case}");
    }
    save_inflight_state_in_root(&root, &row).unwrap();
    let saved = load_inflight_state(&provider, row.channel_id).unwrap();
    assert_eq!(saved.herdr_turn_start, None);
}

/// A row on no Herdr host keeps the watcher's own judgment and needs no Herdr start.
#[test]
fn native_tmux_completion_with_verified_pin_unchanged() {
    let (_temp, _env, root) = setup();
    let (_, row) = row(&root, &ProviderKind::Claude, 8_399_220, "", false);
    assert_eq!(
        row.herdr_turn_evidence(LOGICAL, None),
        HerdrTurnEvidence::NotHerdr
    );
}
