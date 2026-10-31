use super::dto::ActivityHeatmapResponse;
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

pub async fn activity_heatmap_pg(
    pool: &PgPool,
    date: String,
) -> Result<ActivityHeatmapResponse, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT EXTRACT(HOUR FROM td.created_at)::BIGINT AS hour,
                td.to_agent_id,
                COUNT(*)::BIGINT AS cnt
           FROM task_dispatches td
          WHERE td.created_at >= $1::date
            AND td.created_at < $1::date + INTERVAL '1 day'
            AND td.to_agent_id IS NOT NULL
          GROUP BY hour, td.to_agent_id",
    )
    .bind(&date)
    .fetch_all(pool)
    .await?;

    let mut buckets: Vec<serde_json::Map<String, Value>> =
        (0..24).map(|_| serde_json::Map::new()).collect();
    for row in rows {
        let hour = row.try_get::<i64, _>("hour").unwrap_or(-1);
        if !(0..24).contains(&hour) {
            continue;
        }
        let agent_id = match row.try_get::<String, _>("to_agent_id") {
            Ok(value) => value,
            Err(_) => continue,
        };
        let count = row.try_get::<i64, _>("cnt").unwrap_or(0);
        buckets[hour as usize].insert(agent_id, json!(count));
    }
    let hours = buckets
        .into_iter()
        .enumerate()
        .map(|(hour, agents)| json!({ "hour": hour, "agents": agents }))
        .collect();

    Ok(ActivityHeatmapResponse { hours, date })
}
