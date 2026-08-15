//! Narrow PostgreSQL-only `intake-outbox` operator commands.

const NULL_TSV_FIELD: &str = "\\N";

fn escape_tsv_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn optional_tsv_field(value: Option<&str>) -> String {
    value
        .map(escape_tsv_field)
        .unwrap_or_else(|| NULL_TSV_FIELD.to_string())
}

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
            escape_tsv_field(&row.channel_id),
            escape_tsv_field(&row.user_msg_id),
            row.attempt_no,
            row.parent_outbox_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| NULL_TSV_FIELD.to_string()),
            row.dispatched_at
                .map(|at| at.to_rfc3339())
                .unwrap_or_else(|| NULL_TSV_FIELD.to_string()),
            optional_tsv_field(row.claim_owner.as_deref()),
            escape_tsv_field(&row.provider),
            row.provider_nonempty,
        );
    }
    Ok(())
}

/// Settles an open handoff without `force_fail_and_retry_as_new`, whose child
/// insert could redeliver an already delivered turn.
pub(crate) async fn cmd_settle(
    id: i64,
    reason: &str,
    status: crate::db::intake_outbox_status::IntakeOutboxStatus,
) -> Result<(), String> {
    use crate::db::intake_outbox_delivery_proof as proof;

    let config = crate::config::load().map_err(|error| format!("load config: {error}"))?;
    let pool = crate::db::postgres::connect(&config)
        .await?
        .ok_or_else(|| "postgres pool unavailable for intake settlement".to_string())?;
    let result = async {
        let mut transaction = pool.begin().await?;
        let won = proof::settle_unknown_by_operator(&mut transaction, id, status, reason).await?;
        transaction.commit().await?;
        Ok::<_, sqlx::Error>(won)
    }
    .await;
    pool.close().await;
    match result {
        Ok(true) => {
            println!("settled intake_outbox {id} as unknown");
            Ok(())
        }
        Ok(false) => Err(format!(
            "intake_outbox {id} is absent or not in {}",
            status.as_str()
        )),
        Err(error) => Err(format!("settle intake_outbox {id}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsv_fields_escape_row_delimiters_and_distinguish_null() {
        assert_eq!(
            escape_tsv_field("tab\tline\nreturn\rslash\\"),
            "tab\\tline\\nreturn\\rslash\\\\"
        );
        assert_eq!(optional_tsv_field(Some("-")), "-");
        assert_eq!(optional_tsv_field(Some("")), "");
        assert_eq!(optional_tsv_field(None), NULL_TSV_FIELD);
    }
}
