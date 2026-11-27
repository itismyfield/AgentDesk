use super::*;

const CHANNEL: &str = "retired-boundary";

async fn snapshot(
    pool: &PgPool,
) -> Result<(
    Vec<ChannelTranscriptPair>,
    i64,
    Vec<ChannelTranscriptPair>,
    ChannelClearFence,
)> {
    let recent = fetch_recent_channel_pairs(pool, CHANNEL, 10).await?;
    let mut tx = pool.begin().await?;
    let frontier = fetch_channel_frontier_tx(&mut tx, CHANNEL).await?;
    let bounded = fetch_channel_pairs_up_to_frontier_tx(&mut tx, CHANNEL, frontier, 10).await?;
    tx.commit().await?;
    let fence = capture_channel_clear_fence(Some(pool), CHANNEL).await;
    assert!(fence.generation > 0, "boundary observation must succeed");
    Ok((recent, frontier, bounded, fence))
}

type NativeMarker = (
    Option<i64>,
    Option<serde_json::Value>,
    Option<chrono::DateTime<chrono::Utc>>,
);

async fn native_marker(pool: &PgPool) -> Result<NativeMarker> {
    Ok(sqlx::query_as("SELECT native_clear_generation, native_clear_ticket, native_clear_resolved_at FROM channel_session_clear_boundaries WHERE channel_id = $1")
        .bind(CHANNEL).fetch_one(pool).await?)
}

async fn set_native_marker(pool: &PgPool, present: bool) -> Result<()> {
    sqlx::query("UPDATE channel_session_clear_boundaries SET native_clear_generation = CASE WHEN $2 THEN 1 END, native_clear_ticket = CASE WHEN $2 THEN '{}'::jsonb END, native_clear_resolved_at = NULL WHERE channel_id = $1")
        .bind(CHANNEL).bind(present).execute(pool).await?;
    Ok(())
}

fn entry(turn_id: &str) -> PersistSessionTranscript<'_> {
    PersistSessionTranscript {
        turn_id,
        session_key: Some("retired-clear-session"),
        channel_id: Some(CHANNEL),
        agent_id: None,
        provider: Some("claude"),
        dispatch_id: None,
        user_message: "fresh input",
        assistant_message: "fresh answer",
        events: &[],
        duration_ms: None,
        turn_started_at_millis: Some(i64::MAX),
    }
}

#[tokio::test]
async fn unresolved_retired_marker_preserves_general_clear_readers_and_turn_fences_pg() -> Result<()>
{
    let db = crate::db::dispatch_test_db::DispatchPostgresTestDb::create(
        "agentdesk_retired_clear_boundary",
        "retired clear compatibility",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    sqlx::query("INSERT INTO channel_session_clear_boundaries (channel_id, cleared_at, cleared_through_id, clear_generation) VALUES ($1, '2026-01-01 00:00:00+00', 0, 1)")
        .bind(CHANNEL).execute(&pool).await?;
    for (turn, created_at) in [
        ("before", "2025-12-31T23:59:59Z"),
        ("equal", "2026-01-01T00:00:00Z"),
        ("after", "2026-01-01T00:00:01Z"),
    ] {
        sqlx::query("INSERT INTO session_transcripts (turn_id, channel_id, user_message, assistant_message, created_at) VALUES ($1, $2, $1, 'answer', $3)")
            .bind(turn).bind(CHANNEL)
            .bind(chrono::DateTime::parse_from_rfc3339(created_at)?).execute(&pool).await?;
    }
    let legacy = snapshot(&pool).await?;
    assert_eq!(
        legacy.0,
        vec![ChannelTranscriptPair {
            user_message: "after".into(),
            assistant_message: "answer".into()
        }]
    );
    assert_eq!(legacy.2, legacy.0);
    set_native_marker(&pool, true).await?;
    let marker = native_marker(&pool).await?;
    assert_eq!(marker, (Some(1), Some(serde_json::json!({})), None));
    assert_eq!(
        snapshot(&pool).await?,
        legacy,
        "native marker must not affect general readers or intake fence"
    );
    assert!(
        persist_turn_db_with_clear_fence(Some(&pool), entry("admitted"), Some(legacy.3.clone()))
            .await?,
        "unresolved marker must not block an admitted turn"
    );

    for transactional in [true, false] {
        if transactional {
            let tx = begin_channel_clear_boundary_tx(&pool).await?;
            finish_channel_clear_boundary_tx(tx, CHANNEL).await?;
        } else {
            record_channel_clear_boundary(Some(&pool), CHANNEL).await?;
        }
        assert_eq!(
            native_marker(&pool).await?,
            marker,
            "ordinary clear must preserve retired columns"
        );
        let native = snapshot(&pool).await?;
        assert!(
            native.0.is_empty() && native.2.is_empty(),
            "ordinary clear must cover all committed pairs"
        );
        assert_eq!(native.3.generation, if transactional { 2 } else { 3 });
        set_native_marker(&pool, false).await?;
        assert_eq!(
            snapshot(&pool).await?,
            native,
            "native and legacy boundary decisions must match"
        );
        set_native_marker(&pool, true).await?;
        assert!(
            !persist_turn_db_with_clear_fence(Some(&pool), entry("stale"), Some(legacy.3.clone()))
                .await?,
            "ordinary clear must still fence an earlier turn"
        );
        assert!(
            persist_turn_db_with_clear_fence(
                Some(&pool),
                entry(if transactional { "fresh-2" } else { "fresh-3" }),
                Some(native.3)
            )
            .await?,
            "fresh input must pass with an unresolved retired marker"
        );
    }
    pool.close().await;
    db.drop().await;
    Ok(())
}

async fn context_boundary(pool: &PgPool, session_id: Option<&str>) -> Result<()> {
    record_recent_context_boundary(
        Some(pool),
        CHANNEL,
        &RecentContextBoundary {
            kind: RecentContextBoundaryKind::Cleared,
            previous_session: session_id.map(|id| PreviousProviderSession {
                provider: "claude".into(),
                session_id: id.into(),
                path: None,
            }),
        },
    )
    .await
}

async fn prompt_boundary_session(pool: &PgPool) -> Result<Option<String>> {
    let (_, boundary) = fetch_recent_channel_prompt_context(pool, CHANNEL, 3).await?;
    Ok(boundary.and_then(|boundary| {
        boundary
            .previous_session
            .map(|previous| previous.session_id)
    }))
}

#[tokio::test]
async fn recent_context_reclear_preserves_unconsumed_and_replaces_with_the_new_session_pg()
-> Result<()> {
    let db = crate::db::dispatch_test_db::DispatchPostgresTestDb::create(
        "agentdesk_reclear_context",
        "recent context re-clear",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    assert!(persist_turn_db(Some(&pool), entry("session-a")).await?);
    context_boundary(&pool, Some("session-a")).await?;
    context_boundary(&pool, None).await?;
    let after_reclear = prompt_boundary_session(&pool).await?;

    let fence = capture_channel_clear_fence(Some(&pool), CHANNEL).await;
    assert!(persist_turn_db_with_clear_fence(Some(&pool), entry("session-b"), Some(fence)).await?);
    assert_eq!(prompt_boundary_session(&pool).await?, None);
    context_boundary(&pool, Some("session-b")).await?;
    let after_replacement = prompt_boundary_session(&pool).await?;

    let fence = capture_channel_clear_fence(Some(&pool), CHANNEL).await;
    assert!(
        persist_turn_db_with_clear_fence(Some(&pool), entry("completed-b"), Some(fence)).await?
    );
    context_boundary(&pool, None).await?;
    let after_consumed = prompt_boundary_session(&pool).await?;

    pool.close().await;
    db.drop().await;
    assert_eq!(after_reclear.as_deref(), Some("session-a"));
    assert_eq!(after_replacement.as_deref(), Some("session-b"));
    assert_eq!(after_consumed, None);
    Ok(())
}

#[tokio::test]
async fn recent_context_consumes_on_first_visible_post_clear_completion_pg() -> Result<()> {
    let db = crate::db::dispatch_test_db::DispatchPostgresTestDb::create(
        "agentdesk_context_consumption",
        "recent context consumption",
    )
    .await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    assert!(persist_turn_db(Some(&pool), entry("before")).await?);
    context_boundary(&pool, Some("session-a")).await?;
    let fence = capture_channel_clear_fence(Some(&pool), CHANNEL).await;

    let assert_previous = |id| assert_eq!(id, Some("session-a".to_owned()));
    assert_previous(prompt_boundary_session(&pool).await?);
    assert!(
        persist_turn_db_with_clear_fence(Some(&pool), entry("before"), Some(fence.clone())).await?
    );
    assert_previous(prompt_boundary_session(&pool).await?);
    for channel_context in [false, true] {
        assert!(
            persist_turn_db_in_context(
                Some(&pool),
                entry("excluded"),
                Some(fence.clone()),
                channel_context,
            )
            .await?
        );
        assert_previous(prompt_boundary_session(&pool).await?);
    }

    pool.close().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    assert_previous(prompt_boundary_session(&pool).await?);
    for _ in 0..2 {
        assert!(
            persist_turn_db_with_clear_fence(Some(&pool), entry("visible"), Some(fence.clone()))
                .await?
        );
        let (pairs, boundary) = fetch_recent_channel_prompt_context(&pool, CHANNEL, 3).await?;
        assert!(boundary.is_none());
        assert_eq!(
            pairs,
            [ChannelTranscriptPair {
                user_message: "fresh input".into(),
                assistant_message: "fresh answer".into(),
            }]
        );
    }
    pool.close().await;
    let pool = db.connect_and_migrate_with_max_connections(4).await;
    assert_eq!(prompt_boundary_session(&pool).await?, None);
    pool.close().await;
    db.drop().await;
    Ok(())
}
