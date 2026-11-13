//! Plain-notice SQL on the caller's connection: provider rows and the completed plain INSERT.

use serde_json::Value;
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

/// Raw rows in server order; the caller decodes them one by one and may stop early.
pub(crate) async fn provider_rows(
    connection: &mut PgConnection,
    channel: &str,
) -> Result<Vec<PgRow>, sqlx::Error> {
    sqlx::query(
        "SELECT provider, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx
           FROM agents
          WHERE $1 IN (BTRIM(discord_channel_id), BTRIM(discord_channel_alt),
                       BTRIM(discord_channel_cc), BTRIM(discord_channel_cdx))",
    )
    .bind(channel)
    .fetch_all(connection)
    .await
}

/// Column values already validated and normalized by the service; stored as given.
pub(crate) struct PlainRow<'a> {
    pub(crate) target: &'a str,
    pub(crate) content: &'a str,
    pub(crate) bot: &'a str,
    pub(crate) source: &'a str,
    pub(crate) reason_code: Option<&'a str>,
    pub(crate) session_key: Option<&'a str>,
    pub(crate) attachment_filename: Option<&'a str>,
    pub(crate) attachment_content_type: Option<&'a str>,
    pub(crate) attachment_data: Option<&'a [u8]>,
    pub(crate) session_notice: Option<Value>,
}

pub(crate) async fn insert_plain(
    connection: &mut PgConnection,
    row: PlainRow<'_>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO message_outbox
         (target, content, bot, source, reason_code, session_key, attachment_filename, attachment_content_type, attachment_data, session_notice)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         RETURNING id",
    )
    .bind(row.target)
    .bind(row.content)
    .bind(row.bot)
    .bind(row.source)
    .bind(row.reason_code)
    .bind(row.session_key)
    .bind(row.attachment_filename)
    .bind(row.attachment_content_type)
    .bind(row.attachment_data)
    .bind(row.session_notice)
    .fetch_one(connection)
    .await
}
