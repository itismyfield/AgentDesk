use super::*;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;

async fn create_pool() -> (DispatchPostgresTestDb, PgPool) {
    let db = DispatchPostgresTestDb::create(
        "agentdesk_transcript_native_clear",
        "session transcript native clear",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    (db, pool)
}

fn ticket(nonce: &str) -> serde_json::Value {
    json!({"context": {"execution_nonce": nonce}, "old": {"session_id": "old"}, "baseline": 7})
}

fn unresolved(generation: i64, nonce: &str) -> NativeClearBoundary {
    NativeClearBoundary::Unresolved {
        generation: NativeClearGeneration(generation),
        ticket: ticket(nonce),
    }
}

async fn native_clear(pool: &PgPool, channel: &str, nonce: &str) -> NativeClearGeneration {
    let tx = begin_channel_clear_boundary_tx(pool).await.expect("begin");
    finish_native_channel_clear_boundary_tx(tx, channel, &ticket(nonce))
        .await
        .expect("native clear")
}

async fn legacy_clear(pool: &PgPool, channel: &str) {
    let tx = begin_channel_clear_boundary_tx(pool).await.expect("begin");
    finish_channel_clear_boundary_tx(tx, channel)
        .await
        .expect("legacy clear");
}

async fn state(pool: &PgPool, channel: &str) -> NativeClearBoundary {
    native_channel_clear_state(pool, channel)
        .await
        .expect("native clear state")
}

async fn resolve(
    pool: &PgPool,
    channel: &str,
    generation: NativeClearGeneration,
) -> NativeClearResolve {
    resolve_native_channel_clear(pool, channel, generation)
        .await
        .expect("resolve")
}

async fn persist(pool: &PgPool, turn_id: &str, channel: &str, started_at: DateTime<Utc>) -> bool {
    let entry = PersistSessionTranscript {
        turn_id,
        session_key: Some("native-clear-session"),
        channel_id: Some(channel),
        agent_id: None,
        provider: Some("claude"),
        dispatch_id: None,
        user_message: "question",
        assistant_message: "answer",
        events: &[],
        duration_ms: None,
        turn_started_at_millis: Some(started_at.timestamp_millis()),
    };
    persist_turn_db(Some(pool), entry).await.expect("persist")
}

async fn clock(pool: &PgPool) -> DateTime<Utc> {
    sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await
        .expect("clock")
}

/// C1 (launch inside the clear, crash before completion) and C2 (completed clear, then a normal
/// launch) leave identical legacy markers; only the native columns tell them apart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_columns_split_counterexamples_the_legacy_markers_cannot_pg() {
    let (db, pool) = create_pool().await;
    let c1 = begin_channel_clear_boundary_tx(&pool).await.expect("begin");
    let launch_c1 = clock(&pool).await;
    finish_native_channel_clear_boundary_tx(c1, "c1", &ticket("n1"))
        .await
        .expect("c1 clear");
    let g2 = native_clear(&pool, "c2", "n2").await;
    assert_eq!(resolve(&pool, "c2", g2).await, NativeClearResolve::Resolved);
    let launch_c2 = clock(&pool).await;

    let mut legacy_signals = Vec::new();
    for (channel, launch) in [("c1", launch_c1), ("c2", launch_c2)] {
        let signal: (bool, i64, i64) = sqlx::query_as(
            "SELECT $2 > cleared_at, clear_generation, cleared_through_id
               FROM channel_session_clear_boundaries WHERE channel_id = $1",
        )
        .bind(channel)
        .bind(launch)
        .fetch_one(&pool)
        .await
        .expect("legacy signal");
        legacy_signals.push(signal);
    }
    assert_eq!(legacy_signals, vec![(true, 1, 0), (true, 1, 0)]);
    assert_eq!(state(&pool, "c1").await, unresolved(1, "n1"));
    assert_eq!(state(&pool, "c2").await, NativeClearBoundary::Resolved);
    pool.close().await;
    db.drop().await;
}

/// The two-phase control writer and `record_channel_clear_boundary` (the SQL a rolled-back binary
/// also runs) keep the native columns and only advance the generation, superseding the ticket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn older_boundary_writers_keep_native_columns_and_supersede_them_pg() {
    let (db, pool) = create_pool().await;
    for (channel, two_phase) in [("two-phase", true), ("record", false)] {
        native_clear(&pool, channel, "n").await;
        if two_phase {
            legacy_clear(&pool, channel).await;
        } else {
            record_channel_clear_boundary(Some(&pool), channel)
                .await
                .expect("record clear");
        }
        let row: (
            i64,
            Option<i64>,
            Option<serde_json::Value>,
            Option<DateTime<Utc>>,
        ) = sqlx::query_as(
            "SELECT clear_generation, native_clear_generation, native_clear_ticket,
                        native_clear_resolved_at
                   FROM channel_session_clear_boundaries WHERE channel_id = $1",
        )
        .bind(channel)
        .fetch_one(&pool)
        .await
        .expect("boundary row");
        assert_eq!(row, (2, Some(1), Some(ticket("n")), None), "{channel}");
        assert_eq!(
            state(&pool, channel).await,
            NativeClearBoundary::Superseded,
            "{channel}"
        );
    }
    pool.close().await;
    db.drop().await;
}

/// A transcript persisted on the channel after the native frontier supersedes the clear; one on
/// another channel does not. Rows without a correlation, or no row at all, are legacy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_past_frontier_supersedes_and_uncorrelated_rows_are_legacy_pg() {
    let (db, pool) = create_pool().await;
    assert_eq!(state(&pool, "none").await, NativeClearBoundary::Legacy);
    legacy_clear(&pool, "legacy").await;
    assert_eq!(state(&pool, "legacy").await, NativeClearBoundary::Legacy);

    native_clear(&pool, "chat", "n").await;
    let later = Utc::now() + Duration::minutes(1);
    assert!(persist(&pool, "other-channel", "other", later).await);
    assert_eq!(state(&pool, "chat").await, unresolved(1, "n"));
    assert!(persist(&pool, "after-clear", "chat", later).await);
    assert_eq!(state(&pool, "chat").await, NativeClearBoundary::Superseded);
    pool.close().await;
    db.drop().await;
}

/// Completion is a generation CAS: a repeat, or an older generation after a consecutive native
/// clear, changes nothing, and each native clear reopens the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_marks_only_the_current_unresolved_generation_pg() {
    let (db, pool) = create_pool().await;
    let g1 = native_clear(&pool, "cas", "n1").await;
    assert_eq!(
        resolve(&pool, "cas", g1).await,
        NativeClearResolve::Resolved
    );
    assert_eq!(state(&pool, "cas").await, NativeClearBoundary::Resolved);
    assert_eq!(
        resolve(&pool, "cas", g1).await,
        NativeClearResolve::NotCurrent
    );

    let g2 = native_clear(&pool, "cas", "n2").await;
    assert_eq!(g2, NativeClearGeneration(2));
    assert_eq!(state(&pool, "cas").await, unresolved(2, "n2"));
    assert_eq!(
        resolve(&pool, "cas", g1).await,
        NativeClearResolve::NotCurrent
    );
    assert_eq!(state(&pool, "cas").await, unresolved(2, "n2"));
    assert_eq!(
        resolve(&pool, "cas", g2).await,
        NativeClearResolve::Resolved
    );
    assert_eq!(state(&pool, "cas").await, NativeClearBoundary::Resolved);
    pool.close().await;
    db.drop().await;
}

/// The generation and its correlation commit together: an uncommitted or rejected native write
/// leaves no row, and the schema refuses a half-written correlation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_write_is_all_or_nothing_and_partial_correlation_is_refused_pg() {
    let (db, pool) = create_pool().await;
    let mut tx = begin_channel_clear_boundary_tx(&pool).await.expect("begin");
    write_native_channel_clear_boundary(&mut tx, "c0", &ticket("n0"))
        .await
        .expect("uncommitted native write");
    drop(tx);
    let tx = begin_channel_clear_boundary_tx(&pool).await.expect("begin");
    finish_native_channel_clear_boundary_tx(tx, "c0", &json!("not-an-object"))
        .await
        .expect_err("a non-object ticket is refused");
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_session_clear_boundaries")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(rows, 0);

    native_clear(&pool, "native", "n").await;
    legacy_clear(&pool, "legacy").await;
    for (channel, set) in [
        ("native", "native_clear_ticket = NULL"),
        ("native", "native_clear_generation = NULL"),
        ("legacy", "native_clear_generation = 1"),
        ("legacy", "native_clear_resolved_at = NOW()"),
    ] {
        let error = sqlx::query(&format!(
            "UPDATE channel_session_clear_boundaries SET {set} WHERE channel_id = $1"
        ))
        .bind(channel)
        .execute(&pool)
        .await
        .expect_err(set);
        assert_eq!(
            error.as_database_error().and_then(|error| error.code()),
            Some(std::borrow::Cow::Borrowed("23514")),
            "{set}"
        );
    }
    pool.close().await;
    db.drop().await;
}

/// The four existing boundary readers see a native clear exactly as they see a legacy one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_boundary_readers_treat_native_and_legacy_clears_alike_pg() {
    let (db, pool) = create_pool().await;
    let earlier = Utc::now() - Duration::minutes(1);
    let later = Utc::now() + Duration::minutes(1);
    let mut observed = Vec::new();
    for (channel, native) in [("readers-legacy", false), ("readers-native", true)] {
        assert!(persist(&pool, &format!("{channel}-before"), channel, earlier).await);
        if native {
            native_clear(&pool, channel, "n").await;
        } else {
            legacy_clear(&pool, channel).await;
        }
        let late_stored = persist(&pool, &format!("{channel}-late"), channel, earlier).await;
        assert!(persist(&pool, &format!("{channel}-after"), channel, later).await);
        let recent = fetch_recent_channel_pairs(&pool, channel, 10)
            .await
            .expect("recent");
        let mut tx = pool.begin().await.expect("begin");
        let frontier = fetch_channel_frontier_tx(&mut tx, channel)
            .await
            .expect("frontier");
        let bounded = fetch_channel_pairs_up_to_frontier_tx(&mut tx, channel, frontier, 10)
            .await
            .expect("bounded");
        tx.commit().await.expect("commit");
        let fence = capture_channel_clear_fence(Some(&pool), channel).await;
        observed.push((late_stored, recent, bounded, fence.generation));
    }
    let pair = ChannelTranscriptPair {
        user_message: "question".to_string(),
        assistant_message: "answer".to_string(),
    };
    let expected = (false, vec![pair.clone()], vec![pair], 1);
    assert_eq!(observed, vec![expected.clone(), expected]);
    pool.close().await;
    db.drop().await;
}

/// The raw record and the boundary classification read the same row shapes the same way.
#[test]
fn record_and_classification_agree_on_every_row_shape() {
    let t = || Some(json!({"context": {"execution_nonce": "n"}}));
    let unresolved_t = NativeClearBoundary::Unresolved {
        generation: NativeClearGeneration(3),
        ticket: t().unwrap(),
    };
    let cases: Vec<(
        Option<NativeClearStateRow>,
        Option<NativeClearBoundary>,
        Option<(bool, bool)>,
    )> = vec![
        (None, Some(NativeClearBoundary::Legacy), None),
        (
            Some((3, None, t(), false, false)),
            Some(NativeClearBoundary::Legacy),
            None,
        ),
        (
            Some((3, Some(3), None, true, false)),
            Some(NativeClearBoundary::Resolved),
            Some((true, false)),
        ),
        (
            Some((4, Some(3), None, false, false)),
            Some(NativeClearBoundary::Superseded),
            Some((false, true)),
        ),
        (
            Some((3, Some(3), t(), false, true)),
            Some(NativeClearBoundary::Superseded),
            Some((false, false)),
        ),
        (
            Some((3, Some(3), None, false, false)),
            None,
            Some((false, false)),
        ),
        (
            Some((3, Some(3), t(), false, false)),
            Some(unresolved_t),
            Some((false, false)),
        ),
    ];
    for (row, boundary, record) in cases {
        let classified = classify_native_clear_boundary(row.clone()).ok();
        assert_eq!(classified, boundary, "{row:?}");
        let read = native_clear_record::from_row(row.clone()).map(|r| (r.resolved, r.superseded));
        assert_eq!(read, record, "{row:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_keeps_the_ticket_after_resolve_and_supersession_pg() {
    let (db, pool) = create_pool().await;
    assert_eq!(native_channel_clear_record(&pool, "r").await.unwrap(), None);
    let g1 = native_clear(&pool, "r", "n1").await;
    let read = |pool: PgPool| async move {
        native_channel_clear_record(&pool, "r")
            .await
            .unwrap()
            .unwrap()
    };
    let first = read(pool.clone()).await;
    assert_eq!(
        (first.generation, first.ticket, first.resolved),
        (g1, Some(ticket("n1")), false)
    );
    assert_eq!(resolve(&pool, "r", g1).await, NativeClearResolve::Resolved);
    let resolved = read(pool.clone()).await;
    assert_eq!(
        (resolved.ticket, resolved.resolved),
        (Some(ticket("n1")), true)
    );
    let g2 = native_clear(&pool, "r", "n2").await;
    legacy_clear(&pool, "r").await;
    let superseded = read(pool.clone()).await;
    assert_eq!(superseded.generation, g2);
    assert_eq!(superseded.ticket, Some(ticket("n2")));
    assert!(superseded.superseded && !superseded.resolved);
    assert_eq!(state(&pool, "r").await, NativeClearBoundary::Superseded);
    pool.close().await;
    db.drop().await;
}
