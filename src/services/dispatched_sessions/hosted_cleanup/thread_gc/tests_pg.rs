use super::{gc_stale_thread_sessions_with_probe_pg, probe_locally_owned_tmux};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{TOKEN, owner, record, wire};
use crate::services::platform::tmux::SessionPresence;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::{Arc, Mutex};

async fn seed_row(pool: &PgPool, key: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO sessions (
             session_key, provider, identity_kind, discord_token_hash, channel_id,
             status, thread_channel_id, last_heartbeat,
             claude_session_id, raw_provider_session_id
         ) VALUES ($1, 'claude', 'discord_channel', $2, $1,
                   'idle', '1500628371829428390', NOW() - INTERVAL '2 hours',
                   'canonical-resume', 'native-resume') RETURNING id",
    )
    .bind(key)
    .bind(TOKEN)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn row_snapshot(pool: &PgPool, id: i64) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(s) FROM sessions s WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn assert_deleted(pool: &PgPool, key: &str) {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sessions WHERE session_key = $1)")
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap();
    assert!(!exists, "positive deletion control survived: {key}");
}

fn assert_resume_selectors(row: &Value) {
    assert_eq!(row["claude_session_id"], "canonical-resume");
    assert_eq!(row["raw_provider_session_id"], "native-resume");
}

#[tokio::test]
async fn thread_gc_rechecks_turn_occupancy_and_deadline_after_missing_probe_pg() {
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for change in [
        "turn-active",
        "awaiting-user",
        "awaiting-bg",
        "dispatch",
        "children",
        "open-child",
        "heartbeat",
        "thread-null",
    ] {
        let key = format!("test-host:AgentDesk-claude-gc-turn-{change}");
        let control = format!("test-host:AgentDesk-claude-gc-control-{change}");
        let id = seed_row(&pool, &key).await;
        seed_row(&pool, &control).await;
        let changed = Arc::new(Mutex::new(None));
        let deleted = gc_stale_thread_sessions_with_probe_pg(&pool, |probe_key| {
            let (pool, key, changed) = (pool.clone(), key.clone(), changed.clone());
            async move {
                if probe_key == key {
                    if change == "open-child" {
                        sqlx::query(
                            "INSERT INTO sessions (
                                 session_key, provider, status, parent_session_id
                             ) VALUES ($1, 'claude', 'turn_active', $2)",
                        )
                        .bind(format!("{key}-child"))
                        .bind(id)
                        .execute(&pool)
                        .await
                        .unwrap();
                    } else {
                        let sql = match change {
                            "turn-active" => {
                                "UPDATE sessions SET status = 'turn_active' WHERE id = $1"
                            }
                            "awaiting-user" => {
                                "UPDATE sessions SET status = 'awaiting_user' WHERE id = $1"
                            }
                            "awaiting-bg" => {
                                "UPDATE sessions SET status = 'awaiting_bg' WHERE id = $1"
                            }
                            "dispatch" => {
                                "UPDATE sessions SET active_dispatch_id = 'new-turn' WHERE id = $1"
                            }
                            "children" => "UPDATE sessions SET active_children = 1 WHERE id = $1",
                            "heartbeat" => {
                                "UPDATE sessions SET last_heartbeat = NOW() WHERE id = $1"
                            }
                            "thread-null" => {
                                "UPDATE sessions SET thread_channel_id = NULL WHERE id = $1"
                            }
                            _ => unreachable!(),
                        };
                        sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
                    }
                    let snapshot = row_snapshot(&pool, id).await;
                    *changed.lock().unwrap() = Some(snapshot);
                }
                SessionPresence::Missing
            }
        })
        .await;
        assert_eq!(deleted, [control.clone()], "{change}");
        assert_deleted(&pool, &control).await;
        let expected = changed
            .lock()
            .unwrap()
            .take()
            .expect("candidate was probed");
        let kept = row_snapshot(&pool, id).await;
        assert_eq!(kept, expected, "{change}");
        assert_resume_selectors(&kept);
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    db.drop().await;
    drop(root_guard);
}

#[tokio::test]
async fn thread_gc_rechecks_snapshot_identity_after_missing_probe_pg() {
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for change in ["raw", "provider", "kind", "token", "channel", "key"] {
        let key = format!("test-host:AgentDesk-claude-gc-snapshot-{change}");
        let control = format!("test-host:AgentDesk-claude-gc-snapshot-control-{change}");
        let id = seed_row(&pool, &key).await;
        seed_row(&pool, &control).await;
        let changed = Arc::new(Mutex::new(None));
        let deleted = gc_stale_thread_sessions_with_probe_pg(&pool, |probe_key| {
            let (pool, key, changed) = (pool.clone(), key.clone(), changed.clone());
            async move {
                if probe_key == key {
                    if change == "raw" {
                        let retired = wire(&record(&owner(&key), "new", HostedState::Retired));
                        sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE id = $1")
                            .bind(id)
                            .bind(retired)
                            .execute(&pool)
                            .await
                            .unwrap();
                    } else {
                        let sql = match change {
                            "provider" => "UPDATE sessions SET provider = 'codex' WHERE id = $1",
                            "kind" => {
                                "UPDATE sessions SET identity_kind = 'scheduled_snapshot' WHERE id = $1"
                            }
                            "token" => {
                                "UPDATE sessions SET discord_token_hash = 'discord_other' WHERE id = $1"
                            }
                            "channel" => {
                                "UPDATE sessions SET channel_id = 'new-channel' WHERE id = $1"
                            }
                            "key" => {
                                "UPDATE sessions SET session_key = session_key || '-new' WHERE id = $1"
                            }
                            _ => unreachable!(),
                        };
                        sqlx::query(sql).bind(id).execute(&pool).await.unwrap();
                    }
                    let snapshot = row_snapshot(&pool, id).await;
                    *changed.lock().unwrap() = Some(snapshot);
                }
                SessionPresence::Missing
            }
        })
        .await;
        assert_eq!(deleted, [control.clone()], "{change}");
        assert_deleted(&pool, &control).await;
        let expected = changed
            .lock()
            .unwrap()
            .take()
            .expect("candidate was probed");
        let kept = row_snapshot(&pool, id).await;
        assert_eq!(kept, expected, "{change}");
        assert_resume_selectors(&kept);
        sqlx::query("DELETE FROM sessions")
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
    db.drop().await;
    drop(root_guard);
}

#[tokio::test]
async fn thread_gc_keeps_unknown_host_and_probe_failure_before_delete_pg() {
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let key = |name: &str| format!("test-host:AgentDesk-claude-gc-refusals-{name}");
    let mut retained = Vec::new();
    for name in [
        "host-unknown",
        "host-unreadable",
        "owner-foreign",
        "owner-unreadable",
        "probe-failed",
    ] {
        let id = seed_row(&pool, &key(name)).await;
        retained.push((id, row_snapshot(&pool, id).await));
        let tmux_name = format!("AgentDesk-claude-gc-refusals-{name}");
        let host_path = crate::services::tmux_common::session_temp_path(&tmux_name, "host_kind");
        let owner_path = crate::services::tmux_common::tmux_owner_path(&tmux_name);
        match name {
            "host-unknown" => std::fs::write(host_path, "future-host").unwrap(),
            "host-unreadable" => std::fs::write(host_path, [0xff, 0xfe]).unwrap(),
            "owner-foreign" => std::fs::write(owner_path, "other-runtime-root").unwrap(),
            "owner-unreadable" => std::fs::write(owner_path, [0xff, 0xfe]).unwrap(),
            _ => {}
        }
    }
    let control = key("control");
    seed_row(&pool, &control).await;
    let probed = Arc::new(Mutex::new(Vec::new()));
    let deleted = gc_stale_thread_sessions_with_probe_pg(&pool, |probe_key| {
        let probed = probed.clone();
        async move {
            probed.lock().unwrap().push(probe_key.clone());
            assert!(!probe_key.ends_with("host-unknown"));
            assert!(!probe_key.ends_with("host-unreadable"));
            if probe_key.ends_with("owner-foreign") || probe_key.ends_with("owner-unreadable") {
                let presence = probe_locally_owned_tmux(probe_key).await;
                assert_eq!(presence, SessionPresence::ProbeFailed);
                presence
            } else if probe_key.ends_with("probe-failed") {
                SessionPresence::ProbeFailed
            } else {
                SessionPresence::Missing
            }
        }
    })
    .await;
    assert_eq!(deleted, [control.clone()]);
    assert_deleted(&pool, &control).await;
    let mut actual = probed.lock().unwrap().clone();
    actual.sort();
    let mut expected = vec![
        control,
        key("owner-foreign"),
        key("owner-unreadable"),
        key("probe-failed"),
    ];
    expected.sort();
    assert_eq!(actual, expected);
    for (id, before) in retained {
        let kept = row_snapshot(&pool, id).await;
        assert_eq!(kept, before);
        assert_resume_selectors(&kept);
    }
    pool.close().await;
    db.drop().await;
    drop(root_guard);
}
