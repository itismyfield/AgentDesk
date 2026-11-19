use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use uuid::Uuid;

/// One debt effect of `episode` in `state`, with a revoked grant and an attempt in `phase`.
async fn seed(pool: &PgPool, episode: &str, state: &str, phase: &str) {
    let (effect, grant, attempt) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO o_herdr_debt_effects(id,provider,channel_id,episode,input_key,coverage,plan_sha,snapshots,provenance,payload,payload_sha,unit_key,piece_index,bot_id,state) VALUES($1,'claude','7',$2,'i','c','p','[]','{}','body','h','{\"kind\":\"body\"}',0,1,$3)")
        .bind(effect).bind(episode).bind(state).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO o_herdr_debt_grants(id,effect_id,episode,claim_core,state) VALUES($1,$2,$3,'{}','revoked')")
        .bind(grant).bind(effect).bind(episode).execute(pool).await.unwrap();
    let witness = (phase == "no_effect").then(|| {
        serde_json::json!({"version": 1, "kind": "transport_no_effect",
        "attempt": attempt.to_string(), "effect": effect.to_string(), "payload_sha": "h"})
    });
    sqlx::query("INSERT INTO o_herdr_debt_attempts(id,effect_id,grant_id,payload_sha,phase,no_effect_witness) VALUES($1,$2,$3,'h',$4,$5)")
        .bind(attempt).bind(effect).bind(grant).bind(phase).bind(witness).execute(pool).await.unwrap();
    sqlx::query("UPDATE o_herdr_debt_effects SET attempt_id=$2 WHERE id=$1")
        .bind(effect)
        .bind(attempt)
        .execute(pool)
        .await
        .unwrap();
}

async fn rows(pool: &PgPool, episode: &str) -> i64 {
    sqlx::query_scalar("SELECT (SELECT count(*) FROM o_herdr_debt_effects WHERE episode=$1) + (SELECT count(*) FROM o_herdr_debt_grants WHERE episode=$1) + (SELECT count(*) FROM o_herdr_debt_attempts a JOIN o_herdr_debt_effects e ON e.id=a.effect_id WHERE e.episode=$1)")
        .bind(episode).fetch_one(pool).await.unwrap()
}

// A ready effect whose attempt proved no effect goes with its attempt and grant while the reason
// row stays; an in-flight one keeps its whole episode and writes no reason.
#[tokio::test]
async fn a_refused_episode_debt_is_deleted_attempt_first_and_an_in_flight_one_stays_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key =
        |nonce| crate::services::tui_o::channel_policy::episode_key::identity("claude", 7, nonce);
    seed(&pool, "ready", "ready", "no_effect").await;
    seed(&pool, "flight", "reserved", "reserved").await;
    seed(&pool, "other", "ready", "no_effect").await;
    let ready = key("turn-ready").unwrap();
    discard_episode(&pool, "claude", 7, "ready", &ready)
        .await
        .unwrap();
    assert_eq!(rows(&pool, "ready").await, 0);
    assert!(disposition::record(&pool, &ready).await.unwrap().is_some());
    let flight = key("turn-flight").unwrap();
    assert!(
        discard_episode(&pool, "claude", 7, "flight", &flight)
            .await
            .is_err()
    );
    assert_eq!(
        (rows(&pool, "flight").await, rows(&pool, "other").await),
        (3, 3)
    );
    assert!(disposition::record(&pool, &flight).await.unwrap().is_none());
    pool.close().await;
    drop(db);
}

// The key follows the turn nonce alone: a later view of the same turn reuses the first reason.
#[test]
fn a_rows_reason_key_ignores_its_owner_message_and_generation() {
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        7,
        None,
        1,
        2,
        3,
        "prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = Some("turn-a".into());
    let first = identity(&ProviderKind::Claude, &row).unwrap();
    (
        row.request_owner_user_id,
        row.user_msg_id,
        row.born_generation,
    ) = (9, 8, 6);
    assert_eq!(identity(&ProviderKind::Claude, &row).as_ref(), Some(&first));
    row.turn_nonce = Some("turn-b".into());
    assert_ne!(identity(&ProviderKind::Claude, &row).as_ref(), Some(&first));
}

// A direct input's row starts at its prompt's end; its closure names the native turn's opener.
// Offsets naming a normalized replay namespace close nothing unless their raw witness names the turn.
#[cfg(unix)]
#[tokio::test]
async fn a_closure_names_the_native_opener_and_never_a_normalized_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    let line = |value: serde_json::Value| [serde_json::to_vec(&value).unwrap(), b"\n".to_vec()];
    let event = |kind: &str| serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": "t1"}});
    let meta =
        line(serde_json::json!({"type": "session_meta", "payload": {"id": "s-codex", "cwd": "/"}}))
            .concat();
    let opener = line(event("task_started")).concat();
    let prompt = line(
        serde_json::json!({"type": "response_item", "payload": {"type": "message",
        "role": "user", "content": [{"type": "input_text", "text": "hi"}]}}),
    )
    .concat();
    let rest = [
        line(event("agent_message")).concat(),
        line(event("task_complete")).concat(),
    ];
    let bytes = [&meta, &opener, &prompt, &rest[0], &rest[1]]
        .map(Vec::as_slice)
        .concat();
    std::fs::write(&path, bytes).unwrap();
    let opened = meta.len() as u64;
    let prompt_end = opened + (opener.len() + prompt.len()) as u64;
    let file = std::fs::File::open(&path).unwrap().metadata().unwrap();
    let file = crate::services::tui_o::shadow::capture::file_identity(&file);
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Codex,
        7,
        None,
        1,
        2,
        3,
        "hi".into(),
        None,
        None,
        None,
        None,
        0,
    );
    (row.session_id, row.output_path) = (Some("s-codex".into()), Some(path.display().to_string()));
    (row.tui_terminal_source_file_identity, row.turn_start_offset) = (Some(file), Some(prompt_end));
    let closed = closure::of(&ProviderKind::Codex, &row).await.unwrap();
    assert!(
        prompt_end > opened,
        "the row starts inside its turn, past the opener"
    );
    assert_eq!(
        (closed.from, closed.dev, closed.ino),
        (opened, file.0, file.1)
    );
    let raw = serde_json::json!({"path": path.display().to_string(), "file_identity": file,
        "session_id": "s-codex", "native_turn_id": "t1", "turn_start": opened,
        "prefix_record_end": opened, "prefix_len": 0});
    let marker = |raw: serde_json::Value| {
        let marker = serde_json::json!({"version": 1, "install_id": "install", "phase": "installed",
            "prior": {"output_path": null, "last_offset": 0, "turn_start_offset": null}, "raw": raw});
        Some(serde_json::from_value(marker).unwrap())
    };
    row.legacy_replay_coordinate = marker(serde_json::Value::Null);
    assert_eq!(closure::of(&ProviderKind::Codex, &row).await, None);
    (row.turn_start_offset, row.legacy_replay_coordinate) = (Some(0), marker(raw));
    let closed = closure::of(&ProviderKind::Codex, &row).await.unwrap();
    assert_eq!(closed.from, opened, "the raw witness names the turn");
}

// A straddle's range is closed with its own reason and closure; a repeated attempt reuses that
// one row, so one notice is posted.
#[tokio::test]
async fn an_unplaced_range_is_closed_once_with_its_reason_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let closed = disposition::closure::Closure {
        session_id: "s-codex".into(),
        path: "rollout.jsonl".into(),
        dev: 1,
        ino: 2,
        from: 10,
        until: Some(40),
    };
    close_unplaced(&pool, None, 7_651, &closed).await.unwrap();
    close_unplaced(&pool, None, 7_651, &closed).await.unwrap();
    let rows: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT reason, closure FROM tui_output_dispositions")
            .fetch_all(&pool)
            .await
            .unwrap();
    let value = serde_json::to_value(&closed).unwrap();
    assert_eq!(rows, [("\"unplaced\"".to_string(), value)]);
    let read = disposition::closure::of_channel(&pool, 7_651)
        .await
        .unwrap();
    assert_eq!(read, [closed]);
    pool.close().await;
}
