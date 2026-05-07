//! Audit logging helpers for kanban transitions.

/// Log a kanban state transition to the PostgreSQL audit tables.
pub(super) async fn log_transition_audit_pg(
    pg_pool: &sqlx::PgPool,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(card_id)
    .bind(from)
    .bind(to)
    .bind(source)
    .bind(result)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("insert postgres kanban audit for {card_id}: {error}"))?;

    sqlx::query(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('kanban_card', $1, $2, $3)",
    )
    .bind(card_id)
    .bind(format!("{from}->{to} ({result})"))
    .bind(source)
    .execute(pg_pool)
    .await
    .map_err(|error| format!("insert postgres audit log for {card_id}: {error}"))?;

    Ok(())
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn log_audit_on_conn(
    conn: &sqlite_test::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    log_audit(conn, card_id, from, to, source, result);
}

/// Log a kanban state transition to audit_logs table.
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn log_audit(
    conn: &sqlite_test::Connection,
    card_id: &str,
    from: &str,
    to: &str,
    source: &str,
    result: &str,
) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kanban_audit_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            card_id TEXT,
            from_status TEXT,
            to_status TEXT,
            source TEXT,
            result TEXT,
            created_at DATETIME DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result) VALUES (?1, ?2, ?3, ?4, ?5)",
        sqlite_test::params![card_id, from, to, source, result],
    )
    .ok();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS audit_logs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            entity_type TEXT,
            entity_id   TEXT,
            action      TEXT,
            timestamp   DATETIME DEFAULT CURRENT_TIMESTAMP,
            actor       TEXT
        )",
    )
    .ok();
    conn.execute(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
         VALUES ('kanban_card', ?1, ?2, ?3)",
        sqlite_test::params![card_id, format!("{from}->{to} ({result})"), source],
    )
    .ok();
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::kanban::test_support::test_db;

    #[test]
    fn log_audit_preserves_kanban_and_generic_audit_rows() {
        let db = test_db();
        let conn = db.lock().unwrap();

        log_audit(&conn, "card-audit-helper", "review", "done", "hook", "OK");

        let kanban_row: (String, String, String, String) = conn
            .query_row(
                "SELECT from_status, to_status, source, result
                 FROM kanban_audit_logs
                 WHERE card_id = 'card-audit-helper'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            kanban_row,
            (
                "review".to_string(),
                "done".to_string(),
                "hook".to_string(),
                "OK".to_string()
            )
        );

        let generic_row: (String, String) = conn
            .query_row(
                "SELECT action, actor
                 FROM audit_logs
                 WHERE entity_type = 'kanban_card'
                   AND entity_id = 'card-audit-helper'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            generic_row,
            ("review->done (OK)".to_string(), "hook".to_string())
        );
    }
}
