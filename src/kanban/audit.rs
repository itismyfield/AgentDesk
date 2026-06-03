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
