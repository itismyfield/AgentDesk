use sqlx::PgPool;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn load_card_metadata_map_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let metadata_raw: Option<String> = conn.query_row(
        "SELECT metadata FROM kanban_cards WHERE id = ?1",
        [card_id],
        |row| row.get(0),
    )?;

    match metadata_raw {
        Some(raw) if !raw.trim().is_empty() => {
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            Ok(value.as_object().cloned().unwrap_or_default())
        }
        _ => Ok(serde_json::Map::new()),
    }
}

pub async fn load_card_metadata_map_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let metadata_raw = sqlx::query_scalar::<_, Option<String>>(
        "SELECT metadata::text FROM kanban_cards WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| anyhow::anyhow!("load postgres metadata for {card_id}: {error}"))?
    .flatten();

    match metadata_raw {
        Some(raw) if !raw.trim().is_empty() => {
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            Ok(value.as_object().cloned().unwrap_or_default())
        }
        _ => Ok(serde_json::Map::new()),
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn save_card_metadata_map_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if metadata.is_empty() {
        conn.execute(
            "UPDATE kanban_cards SET metadata = NULL WHERE id = ?1",
            [card_id],
        )?;
    } else {
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = ?2",
            sqlite_test::params![serde_json::to_string(metadata)?, card_id],
        )?;
    }
    Ok(())
}

pub async fn save_card_metadata_map_pg(
    pool: &PgPool,
    card_id: &str,
    metadata: &serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
    if metadata.is_empty() {
        sqlx::query(
            "UPDATE kanban_cards
             SET metadata = NULL,
                 updated_at = NOW()
             WHERE id = $1",
        )
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| anyhow::anyhow!("clear postgres metadata for {card_id}: {error}"))?;
    } else {
        sqlx::query(
            "UPDATE kanban_cards
             SET metadata = $1::jsonb,
                 updated_at = NOW()
             WHERE id = $2",
        )
        .bind(serde_json::to_string(metadata)?)
        .bind(card_id)
        .execute(pool)
        .await
        .map_err(|error| anyhow::anyhow!("save postgres metadata for {card_id}: {error}"))?;
    }
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn mark_api_reopen_skip_preflight_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    metadata.insert(
        "skip_preflight_once".to_string(),
        serde_json::Value::String("api_reopen".to_string()),
    );
    save_card_metadata_map_on_conn(conn, card_id, &metadata)
}

pub async fn mark_api_reopen_skip_preflight_on_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    metadata.insert(
        "skip_preflight_once".to_string(),
        serde_json::Value::String("api_reopen".to_string()),
    );
    save_card_metadata_map_pg(pool, card_id, &metadata).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn clear_api_reopen_skip_preflight_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    metadata.remove("skip_preflight_once");
    save_card_metadata_map_on_conn(conn, card_id, &metadata)
}

pub async fn clear_api_reopen_skip_preflight_on_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    metadata.remove("skip_preflight_once");
    save_card_metadata_map_pg(pool, card_id, &metadata).await
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn consume_api_reopen_preflight_skip_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    if matches!(
        metadata
            .get("skip_preflight_once")
            .and_then(|value| value.as_str()),
        Some("api_reopen") | Some("pmd_reopen")
    ) {
        metadata.remove("skip_preflight_once");
        metadata.insert(
            "preflight_status".to_string(),
            serde_json::Value::String("skipped".to_string()),
        );
        metadata.insert(
            "preflight_summary".to_string(),
            serde_json::Value::String("Skipped for API reopen".to_string()),
        );
        metadata.insert(
            "preflight_checked_at".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        save_card_metadata_map_on_conn(conn, card_id, &metadata)?;
    }
    Ok(())
}

pub async fn consume_api_reopen_preflight_skip_on_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    if matches!(
        metadata
            .get("skip_preflight_once")
            .and_then(|value| value.as_str()),
        Some("api_reopen") | Some("pmd_reopen")
    ) {
        metadata.remove("skip_preflight_once");
        metadata.insert(
            "preflight_status".to_string(),
            serde_json::Value::String("skipped".to_string()),
        );
        metadata.insert(
            "preflight_summary".to_string(),
            serde_json::Value::String("Skipped for API reopen".to_string()),
        );
        metadata.insert(
            "preflight_checked_at".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        save_card_metadata_map_pg(pool, card_id, &metadata).await?;
    }
    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub fn clear_reopen_preflight_cache_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_on_conn(conn, card_id)?;
    for key in [
        "skip_preflight_once",
        "preflight_status",
        "preflight_summary",
        "preflight_checked_at",
        "consultation_status",
        "consultation_result",
    ] {
        metadata.remove(key);
    }
    save_card_metadata_map_on_conn(conn, card_id, &metadata)
}

pub async fn clear_reopen_preflight_cache_on_pg(
    pool: &PgPool,
    card_id: &str,
) -> anyhow::Result<()> {
    let mut metadata = load_card_metadata_map_pg(pool, card_id).await?;
    for key in [
        "skip_preflight_once",
        "preflight_status",
        "preflight_summary",
        "preflight_checked_at",
        "consultation_status",
        "consultation_result",
    ] {
        metadata.remove(key);
    }
    save_card_metadata_map_pg(pool, card_id, &metadata).await
}
