use super::*;
use chrono::{DateTime, Duration, Local, Utc};

#[tokio::test]
async fn utc_session_preserves_turn_instants_pg() {
    if std::env::var_os("AGENTDESK_TURNS_TIMESTAMP_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "db::turns::timestamp_tests::utc_session_preserves_turn_instants_pg",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("TZ", "Asia/Seoul")
            .env("AGENTDESK_TURNS_TIMESTAMP_CHILD", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "timestamp subprocess failed: {stdout}\n{stderr}"
        );
        print!("{stdout}");
        return;
    }
    assert_eq!(Local::now().offset().local_minus_utc(), 9 * 60 * 60);
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "turn_timestamps",
        "turn timestamp storage",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    sqlx::query("SET TIME ZONE 'UTC'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SHOW TIME ZONE")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "UTC"
    );
    let mut entry = PersistTurnOwned {
        turn_id: "timestamp-turn".into(),
        session_key: None,
        thread_id: None,
        thread_title: None,
        channel_id: "timestamp-channel".into(),
        agent_id: None,
        provider: None,
        session_id: None,
        dispatch_id: None,
        started_at: None,
        finished_at: None,
        duration_ms: None,
        token_usage: TurnTokenUsage::default(),
    };
    let mut failures = Vec::new();
    for (label, started, finished, expected) in [
        ("missing", None, None, None),
        ("blank", Some(" \t"), Some(""), None),
        (
            "legacy local midnight",
            Some(" 2026-10-28 00:00:00.123456 "),
            Some("2026-10-28 00:00:00.123456"),
            Some("2026-10-27T15:00:00.123456Z"),
        ),
        (
            "explicit offsets",
            Some("2026-10-28T00:00:00.123456+09:00"),
            Some("2026-10-27T11:00:00.123456-04:00"),
            Some("2026-10-27T15:00:00.123456Z"),
        ),
        (
            "UTC and postgres text",
            Some("2026-10-27T15:00:00Z"),
            Some("2026-10-27 15:00:00+00"),
            Some("2026-10-27T15:00:00Z"),
        ),
    ] {
        entry.started_at = started.map(str::to_string);
        entry.finished_at = finished.map(str::to_string);
        let before = Utc::now() - Duration::seconds(1);
        upsert_turn_owned_db(Some(&pool), &entry).await.unwrap();
        let after = Utc::now();
        let (started, finished, created): (DateTime<Utc>, DateTime<Utc>, DateTime<Utc>) =
            sqlx::query_as(
                "SELECT started_at, finished_at, created_at FROM turns WHERE turn_id = $1",
            )
            .bind(&entry.turn_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        for (column, stored) in [("started_at", started), ("finished_at", finished)] {
            let correct = if let Some(expected) = expected {
                stored
                    == DateTime::parse_from_rfc3339(expected)
                        .unwrap()
                        .with_timezone(&Utc)
            } else {
                stored >= before
                    && stored <= after
                    && (label != "missing" || (created >= before && created <= after))
            };
            if !correct {
                failures.push(format!("{label} {column}: stored={stored}, expected={expected:?}, actual={before}..{after}"));
            }
        }
    }
    for invalid_field in ["started_at", "finished_at"] {
        entry.started_at = Some("2026-10-27T15:00:00Z".into());
        entry.finished_at = Some("2026-10-27T15:00:00Z".into());
        if invalid_field == "started_at" {
            entry.started_at = Some("not-a-timestamp".into());
        } else {
            entry.finished_at = Some("2026-02-30 12:00:00".into());
        }
        assert!(upsert_turn_owned_db(Some(&pool), &entry).await.is_err());
        let stored: (DateTime<Utc>, DateTime<Utc>) =
            sqlx::query_as("SELECT started_at, finished_at FROM turns WHERE turn_id = $1")
                .bind(&entry.turn_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        let expected = DateTime::parse_from_rfc3339("2026-10-27T15:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(stored, (expected, expected));
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    pool.close().await;
    db.drop().await;
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
