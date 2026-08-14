//! Narrow PostgreSQL-only `intake-outbox` operator commands.

pub(crate) async fn cmd_dispatched_audit() -> Result<(), String> {
    let config = crate::config::load().map_err(|error| format!("load config: {error}"))?;
    let pool = crate::db::postgres::connect(&config)
        .await?
        .ok_or_else(|| "postgres pool unavailable for dispatched audit".to_string())?;
    let result = crate::db::intake_outbox_dispatched_audit::list_dispatched_audit(&pool).await;
    pool.close().await;
    let rows = result.map_err(|error| format!("list dispatched intake_outbox rows: {error}"))?;

    if rows.is_empty() {
        println!("(no dispatched intake_outbox rows)");
        return Ok(());
    }

    println!(
        "id\tchannel_id\tuser_msg_id\tattempt_no\tparent_outbox_id\tdispatched_at\tclaim_owner\tprovider\tprovider_nonempty"
    );
    for row in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.id,
            row.channel_id,
            row.user_msg_id,
            row.attempt_no,
            row.parent_outbox_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
            row.dispatched_at
                .map(|at| at.to_rfc3339())
                .unwrap_or_else(|| "-".to_string()),
            row.claim_owner.as_deref().unwrap_or("-"),
            row.provider,
            row.provider_nonempty,
        );
    }
    Ok(())
}
