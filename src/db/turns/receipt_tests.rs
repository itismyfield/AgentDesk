use super::*;
use chrono::{DateTime, Duration, SubsecRound, Utc};

/// Postgres keeps microseconds; instants compared after a round trip are taken at that precision.
fn now_us() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

async fn database(
    prefix: &str,
) -> (
    crate::dispatch::test_support::DispatchPostgresTestDb,
    PgPool,
) {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(prefix, "turn receipts")
        .await;
    let pool = db.connect_and_migrate().await;
    (db, pool)
}

fn receipt<'a>(
    turn_id: &'a str,
    origins: &'a [String],
    outcome: TerminalOutcome,
    confirmed_at: DateTime<Utc>,
) -> TurnReceipt<'a> {
    TurnReceipt {
        turn_id,
        channel_id: "7340",
        agent_id: Some("receipt-agent"),
        provider: "claude",
        session_key: Some("receipt-session"),
        started_at: None,
        consumed_origin_ids: origins,
        outcome,
        confirmed_at,
    }
}

type Stored = (Vec<String>, Option<String>, Option<DateTime<Utc>>);

async fn stored(pool: &PgPool, table: &str, turn_id: &str) -> Stored {
    let sql = match table {
        "turns" => {
            "SELECT consumed_origin_ids, terminal_outcome, terminal_confirmed_at FROM turns WHERE turn_id = $1"
        }
        "turns_archive" => {
            "SELECT consumed_origin_ids, terminal_outcome, terminal_confirmed_at FROM turns_archive WHERE turn_id = $1"
        }
        other => panic!("unknown table {other}"),
    };
    sqlx::query_as(sql)
        .bind(turn_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn origins(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

/// A second terminal for the same execution keeps the first outcome and instant; its origins
/// join the first, and replaying either write changes nothing.
#[tokio::test]
async fn first_terminal_wins_origins_accumulate_and_replays_are_idempotent_pg() {
    let (db, pool) = database("turn_receipt_first").await;
    let first_at = now_us() - Duration::seconds(30);
    let (a, b) = (origins(&["G-a"]), origins(&["G-b", "G-a"]));
    let first = receipt("discord:7340:1", &a, TerminalOutcome::Completed, first_at);
    let second = receipt("discord:7340:1", &b, TerminalOutcome::Cancelled, now_us());
    record_turn_receipt(&pool, &first).await.unwrap();
    record_turn_receipt(&pool, &second).await.unwrap();
    let expected = (
        origins(&["G-a", "G-b"]),
        Some("completed".to_string()),
        Some(first_at),
    );
    assert_eq!(stored(&pool, "turns", "discord:7340:1").await, expected);
    record_turn_receipt(&pool, &first).await.unwrap();
    record_turn_receipt(&pool, &second).await.unwrap();
    assert_eq!(stored(&pool, "turns", "discord:7340:1").await, expected);
    pool.close().await;
    db.drop().await;
}

/// Two producers ending one execution at once leave exactly one outcome, the one whose instant
/// is stored with it, and both origins.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_terminals_keep_one_outcome_with_its_own_instant_pg() {
    let (db, pool) = database("turn_receipt_race").await;
    for round in 0..8 {
        let turn = format!("discord:7340:{}", 100 + round);
        let (left_at, right_at) = (now_us() - Duration::seconds(5), now_us());
        let (left_pool, right_pool) = (pool.clone(), pool.clone());
        let (left_turn, right_turn) = (turn.clone(), turn.clone());
        let left = tokio::spawn(async move {
            let ids = origins(&["G-left"]);
            let r = receipt(&left_turn, &ids, TerminalOutcome::Completed, left_at);
            record_turn_receipt(&left_pool, &r).await
        });
        let right = tokio::spawn(async move {
            let ids = origins(&["G-right"]);
            let r = receipt(&right_turn, &ids, TerminalOutcome::Failed, right_at);
            record_turn_receipt(&right_pool, &r).await
        });
        left.await.unwrap().unwrap();
        right.await.unwrap().unwrap();
        let (ids, outcome, at) = stored(&pool, "turns", &turn).await;
        assert_eq!(ids, origins(&["G-left", "G-right"]), "round {round}");
        let pair = (outcome.as_deref(), at);
        assert!(
            pair == (Some("completed"), Some(left_at)) || pair == (Some("failed"), Some(right_at)),
            "round {round}: {pair:?}"
        );
    }
    pool.close().await;
    db.drop().await;
}

/// The analytics writer and the receipt writer share a row in either order without either
/// erasing the other's columns; a row only analytics wrote is never a terminal receipt.
#[tokio::test]
async fn analytics_rows_and_receipts_share_a_row_in_either_order_pg() {
    let (db, pool) = database("turn_receipt_analytics").await;
    let analytics = |turn_id: &str| PersistTurnOwned {
        turn_id: turn_id.into(),
        session_key: Some("receipt-session".into()),
        thread_id: None,
        thread_title: None,
        channel_id: "7340".into(),
        agent_id: Some("receipt-agent".into()),
        provider: Some("claude".into()),
        session_id: Some("provider-session".into()),
        dispatch_id: None,
        started_at: Some("2026-10-27T15:00:00Z".into()),
        finished_at: Some("2026-10-27T15:01:00Z".into()),
        duration_ms: Some(60_000),
        token_usage: TurnTokenUsage {
            input_tokens: 11,
            ..TurnTokenUsage::default()
        },
    };
    let ids = origins(&["G-shared"]);
    let at = now_us();
    upsert_turn_owned_db(Some(&pool), &analytics("discord:7340:21"))
        .await
        .unwrap();
    assert_eq!(
        stored(&pool, "turns", "discord:7340:21").await,
        (Vec::new(), None, None),
        "an analytics-only row carries no receipt"
    );
    assert_eq!(
        find_origin_receipt(&pool, "G-shared", "receipt-agent", &[])
            .await
            .unwrap(),
        None
    );
    record_turn_receipt(
        &pool,
        &receipt("discord:7340:21", &ids, TerminalOutcome::Empty, at),
    )
    .await
    .unwrap();
    record_turn_receipt(
        &pool,
        &receipt("discord:7340:22", &ids, TerminalOutcome::Empty, at),
    )
    .await
    .unwrap();
    upsert_turn_owned_db(Some(&pool), &analytics("discord:7340:22"))
        .await
        .unwrap();
    for turn in ["discord:7340:21", "discord:7340:22"] {
        let (receipt_ids, outcome, confirmed) = stored(&pool, "turns", turn).await;
        assert_eq!(
            (receipt_ids, outcome.as_deref(), confirmed),
            (ids.clone(), Some("empty"), Some(at)),
            "{turn}"
        );
        let (session_id, tokens): (Option<String>, i64) =
            sqlx::query_as("SELECT session_id, input_tokens FROM turns WHERE turn_id = $1")
                .bind(turn)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (session_id.as_deref(), tokens),
            (Some("provider-session"), 11),
            "{turn}"
        );
    }
    pool.close().await;
    db.drop().await;
}

/// The origin lookup answers only for the agent's own executions: by agent id or by one of its
/// channels.
#[tokio::test]
async fn an_origin_receipt_is_found_only_for_its_agent_or_channel_pg() {
    let (db, pool) = database("turn_receipt_scope").await;
    let ids = origins(&["G-scoped"]);
    let mut foreign = receipt("discord:9999:1", &ids, TerminalOutcome::Completed, now_us());
    foreign.agent_id = Some("other-agent");
    foreign.channel_id = "9999";
    record_turn_receipt(&pool, &foreign).await.unwrap();
    let lookup = |agent: &'static str, channels: Vec<String>| {
        let pool = pool.clone();
        async move {
            find_origin_receipt(&pool, "G-scoped", agent, &channels)
                .await
                .unwrap()
                .map(|found| found.turn_id)
        }
    };
    assert_eq!(lookup("receipt-agent", vec!["7340".into()]).await, None);
    assert_eq!(
        lookup("other-agent", Vec::new()).await.as_deref(),
        Some("discord:9999:1")
    );
    assert_eq!(
        lookup("receipt-agent", vec!["9999".into()])
            .await
            .as_deref(),
        Some("discord:9999:1")
    );
    pool.close().await;
    db.drop().await;
}

/// Retention moves an aged receipt to the archive with the same three columns, and the origin
/// lookup still finds it there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retention_keeps_receipt_columns_and_the_origin_lookup_reads_the_archive_pg() {
    let (db, pool) = database("turn_receipt_retention").await;
    let ids = origins(&["G-aged", "G-aged-2"]);
    let aged = now_us() - Duration::days(91);
    let turn = "discord:7340:91";
    record_turn_receipt(&pool, &receipt(turn, &ids, TerminalOutcome::Failed, aged))
        .await
        .unwrap();
    let before = stored(&pool, "turns", turn).await;
    crate::services::maintenance::jobs::db_retention::db_retention_job(&pool, false)
        .await
        .unwrap();
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE turn_id = $1")
        .bind(turn)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 0, "the aged row left the hot table");
    assert_eq!(stored(&pool, "turns_archive", turn).await, before);
    let found = find_origin_receipt(&pool, "G-aged-2", "receipt-agent", &[])
        .await
        .unwrap()
        .expect("an archived receipt is still found");
    assert_eq!(
        (found.turn_id.as_str(), found.terminal_outcome.as_str()),
        (turn, "failed")
    );
    pool.close().await;
    db.drop().await;
}

/// Under a non-UTC host clock and a non-UTC database session, the terminal instant is stored
/// as given and a local start string is read as the host's local time.
#[tokio::test]
async fn receipt_instants_survive_non_utc_host_and_session_zones_pg() {
    if std::env::var_os("AGENTDESK_TURN_RECEIPT_TZ_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "db::turns::receipt_tests::receipt_instants_survive_non_utc_host_and_session_zones_pg",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TZ", "Asia/Seoul")
            .env("AGENTDESK_TURN_RECEIPT_TZ_CHILD", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "receipt timezone subprocess failed: {stdout}\n{stderr}"
        );
        return;
    }
    assert_eq!(chrono::Local::now().offset().local_minus_utc(), 9 * 60 * 60);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "turn_receipt_tz",
        "turn receipt time zones",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    sqlx::query("SET TIME ZONE 'America/New_York'")
        .execute(&pool)
        .await
        .unwrap();
    let confirmed = DateTime::parse_from_rfc3339("2026-10-27T15:00:00.123456Z")
        .unwrap()
        .with_timezone(&Utc);
    let ids = origins(&["G-tz"]);
    let mut entry = receipt(
        "discord:7340:9",
        &ids,
        TerminalOutcome::Completed,
        confirmed,
    );
    entry.started_at = Some("2026-10-28 00:00:00");
    record_turn_receipt(&pool, &entry).await.unwrap();
    let (started, finished, terminal): (DateTime<Utc>, DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as(
            "SELECT started_at, finished_at, terminal_confirmed_at FROM turns WHERE turn_id = $1",
        )
        .bind("discord:7340:9")
        .fetch_one(&pool)
        .await
        .unwrap();
    let local_midnight = DateTime::parse_from_rfc3339("2026-10-27T15:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(
        (started, finished, terminal),
        (local_midnight, confirmed, confirmed)
    );
    let found = find_origin_receipt(&pool, "G-tz", "receipt-agent", &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.terminal_confirmed_at, confirmed);
    pool.close().await;
    db.drop().await;
}
