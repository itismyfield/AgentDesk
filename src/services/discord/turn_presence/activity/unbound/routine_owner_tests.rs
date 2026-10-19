use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::provider::ProviderKind;

const TOKEN: &str = "routine-owner-token";
const CURRENT: u64 = 69_183_001;
const THREAD: u64 = 69_183_900;

fn key(name: &str) -> String {
    adk_session::build_namespaced_session_key(TOKEN, &ProviderKind::Claude, name)
}

async fn run(pool: &sqlx::PgPool, id: &str, owned: &str, turn: Option<&str>, channel: Option<u64>) {
    let result = channel.map(|channel| serde_json::json!({"channel_id": channel.to_string()}));
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, status, turn_id, result_json, owned_tmux_session,
                                   finished_at)
         VALUES ($1, 'r', 'succeeded', $2, $3, $4, NOW())",
    )
    .bind(id)
    .bind(turn)
    .bind(result)
    .bind(owned)
    .execute(pool)
    .await
    .unwrap();
}

/// Only a run that recorded the session's exact namespaced key and started a turn in a known
/// channel attributes it; a failed read attributes nothing.
#[tokio::test(flavor = "current_thread")]
async fn routine_turn_channels_need_exact_started_records_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    sqlx::query("INSERT INTO routines (id, script_ref, name) VALUES ('r', 'r.js', 'r')")
        .execute(&pool)
        .await
        .unwrap();
    let name = |tail: &str| format!("AgentDesk-claude-routine-{tail}");
    // A finished fresh run keeps its recorded orphan attributed.
    run(
        &pool,
        "orphan",
        &key(&name("orphan")),
        Some("t1"),
        Some(THREAD),
    )
    .await;
    run(
        &pool,
        "here-1",
        &key(&name("here")),
        Some("t2"),
        Some(THREAD),
    )
    .await;
    run(
        &pool,
        "here-2",
        &key(&name("here")),
        Some("t3"),
        Some(CURRENT),
    )
    .await;
    run(
        &pool,
        "unstarted",
        &key(&name("unstarted")),
        None,
        Some(THREAD),
    )
    .await;
    run(
        &pool,
        "no-channel",
        &key(&name("no-channel")),
        Some("t4"),
        None,
    )
    .await;
    run(
        &pool,
        "mixed-1",
        &key(&name("mixed")),
        Some("t5"),
        Some(THREAD),
    )
    .await;
    run(&pool, "mixed-2", &key(&name("mixed")), None, Some(THREAD)).await;
    let other = format!("claude/other-token/host:{}", name("other-key"));
    run(&pool, "other-key", &other, Some("t6"), Some(THREAD)).await;
    let names: Vec<String> = [
        "orphan",
        "here",
        "unstarted",
        "no-channel",
        "mixed",
        "other-key",
        // A persistent routine records no owned session at all.
        "persistent",
    ]
    .into_iter()
    .map(name)
    .chain(["claude".to_string()])
    .collect();
    let read = routine_turn_channels(&pool, TOKEN, CURRENT, &names).await;
    let expected = [(name("orphan"), THREAD), (name("here"), CURRENT)];
    assert_eq!(read, Ok(expected.into_iter().collect()));
    pool.close().await;
    let failed = routine_turn_channels(&pool, TOKEN, CURRENT, &names).await;
    assert!(failed.is_err(), "{failed:?}");
    db.drop().await;
}
