use super::dto::{AchievementsResponse, ActivityHeatmapResponse, StreaksResponse};
use chrono::{Datelike, NaiveDate};
use serde_json::{Value, json};
use sqlx::{PgPool, QueryBuilder, Row};
use std::collections::HashMap;

pub async fn streaks_pg(pool: &PgPool) -> Result<StreaksResponse, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT a.id, a.name, a.avatar_emoji,
                STRING_AGG(DISTINCT td.updated_at::date::text, ',') AS active_dates,
                MAX(td.updated_at)::text AS last_active
         FROM agents a
         INNER JOIN task_dispatches td ON td.to_agent_id = a.id
         WHERE td.status = 'completed'
         GROUP BY a.id
         ORDER BY last_active DESC",
    )
    .fetch_all(pool)
    .await?;

    let streaks = rows
        .into_iter()
        .map(|row| {
            let agent_id = row.try_get::<String, _>("id").unwrap_or_default();
            let name = row.try_get::<Option<String>, _>("name").ok().flatten();
            let avatar_emoji = row
                .try_get::<Option<String>, _>("avatar_emoji")
                .ok()
                .flatten();
            let active_dates_str = row
                .try_get::<Option<String>, _>("active_dates")
                .ok()
                .flatten();
            let last_active = row
                .try_get::<Option<String>, _>("last_active")
                .ok()
                .flatten();
            let streak = if let Some(ref dates_str) = active_dates_str {
                let mut dates: Vec<&str> = dates_str.split(',').collect();
                dates.sort();
                dates.reverse();
                compute_streak(&dates)
            } else {
                0
            };

            json!({
                "agent_id": agent_id,
                "name": name,
                "avatar_emoji": avatar_emoji,
                "streak": streak,
                "last_active": last_active,
            })
        })
        .collect::<Vec<_>>();

    Ok(StreaksResponse { streaks })
}

fn compute_streak(sorted_dates_desc: &[&str]) -> i64 {
    compute_streak_from_today(sorted_dates_desc, chrono_today())
}

fn compute_streak_from_today(sorted_dates_desc: &[&str], today: i64) -> i64 {
    if sorted_dates_desc.is_empty() {
        return 0;
    }

    let mut streak = 0i64;
    let mut expected_date = today;

    for date_str in sorted_dates_desc {
        if let Some(d) = parse_date(date_str) {
            if d == expected_date {
                streak += 1;
                expected_date = d - 1;
            } else if d < expected_date {
                break;
            }
        }
    }

    streak
}

fn parse_date(s: &str) -> Option<i64> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .ok()
        .map(|date| i64::from(date.num_days_from_ce()))
}

fn chrono_today() -> i64 {
    i64::from(chrono::Utc::now().date_naive().num_days_from_ce())
}

pub async fn achievements_pg(
    pool: &PgPool,
    agent_id: Option<&str>,
) -> Result<AchievementsResponse, sqlx::Error> {
    let mut query = QueryBuilder::new(
        "SELECT id, COALESCE(name, id), COALESCE(name_ko, name, id), xp, avatar_emoji FROM agents WHERE xp > 0",
    );
    if let Some(agent_id) = agent_id {
        query.push(" AND id = ").push_bind(agent_id);
    }

    let agents: Vec<(String, String, String, i64, String)> = query
        .build()
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| {
            (
                row.try_get::<String, _>(0).unwrap_or_default(),
                row.try_get::<String, _>(1).unwrap_or_default(),
                row.try_get::<String, _>(2).unwrap_or_default(),
                row.try_get::<i64, _>(3).unwrap_or(0),
                row.try_get::<Option<String>, _>(4)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "🤖".to_string()),
            )
        })
        .collect();

    let mut agent_completed_times: HashMap<String, Vec<i64>> = HashMap::new();
    for (agent_id, _, _, _, _) in &agents {
        let times: Vec<i64> = sqlx::query_scalar(
            "SELECT (EXTRACT(EPOCH FROM updated_at)::BIGINT * 1000) AS completed_at_ms
             FROM task_dispatches WHERE to_agent_id = $1 AND status = 'completed'
             ORDER BY updated_at ASC",
        )
        .bind(agent_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        agent_completed_times.insert(agent_id.clone(), times);
    }

    Ok(AchievementsResponse {
        achievements: build_achievements(&agents, &agent_completed_times),
        daily_missions: daily_missions_pg(pool).await?,
    })
}

fn build_achievements(
    agents: &[(String, String, String, i64, String)],
    agent_completed_times: &HashMap<String, Vec<i64>>,
) -> Vec<Value> {
    let milestones: &[(i64, &str, &str, &str)] = &[
        (10, "first_task", "첫 번째 작업 완료", "common"),
        (50, "getting_started", "본격적인 시작", "uncommon"),
        (100, "centurion", "100 XP 달성", "rare"),
        (250, "veteran", "베테랑", "epic"),
        (500, "expert", "전문가", "legendary"),
        (1000, "master", "마스터", "mythic"),
    ];

    let mut achievements = Vec::new();
    for (agent_id, name, name_ko, xp, avatar_emoji) in agents {
        let completion_times = agent_completed_times.get(agent_id.as_str());
        for (index, (threshold, achievement_type, description, rarity)) in
            milestones.iter().enumerate()
        {
            if xp >= threshold {
                let next_threshold = milestones.get(index + 1).map(|milestone| milestone.0);
                let percent = next_threshold
                    .map(|next| ((*xp as f64 / next as f64) * 100.0).round() as i64)
                    .unwrap_or(100);
                let approx_index = (*threshold as usize / 10).saturating_sub(1);
                let earned_at = completion_times
                    .and_then(|times| times.get(approx_index))
                    .copied()
                    .unwrap_or(0);

                achievements.push(json!({
                    "id": format!("{agent_id}:{achievement_type}"),
                    "agent_id": agent_id,
                    "type": achievement_type,
                    "name": format!("{description} ({threshold} XP)"),
                    "description": description,
                    "earned_at": earned_at,
                    "agent_name": name,
                    "agent_name_ko": name_ko,
                    "avatar_emoji": avatar_emoji.as_str(),
                    "rarity": rarity,
                    "progress": {
                        "current_xp": xp,
                        "threshold": threshold,
                        "next_threshold": next_threshold,
                        "percent": percent,
                    },
                }));
            }
        }
    }

    achievements
}

async fn daily_missions_pg(pool: &PgPool) -> Result<Vec<Value>, sqlx::Error> {
    let (completed_today, active_agents_today, review_queue): (i64, i64, i64) = sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM task_dispatches
             WHERE status = 'completed' AND updated_at >= date_trunc('day', NOW())),
            (SELECT COUNT(DISTINCT to_agent_id) FROM task_dispatches
             WHERE status = 'completed' AND updated_at >= date_trunc('day', NOW())
               AND to_agent_id IS NOT NULL),
            (SELECT COUNT(*) FROM kanban_cards WHERE status = 'review')",
    )
    .fetch_one(pool)
    .await?;
    Ok(vec![
        json!({
            "id": "dispatches_today",
            "label": "Complete 5 dispatches today",
            "current": completed_today,
            "target": 5,
            "completed": completed_today >= 5,
        }),
        json!({
            "id": "active_agents_today",
            "label": "Get 3 agents shipping today",
            "current": active_agents_today,
            "target": 3,
            "completed": active_agents_today >= 3,
        }),
        json!({
            "id": "review_queue_zero",
            "label": "Drain the review queue",
            "current": i64::from(review_queue == 0),
            "target": 1,
            "completed": review_queue == 0,
        }),
    ])
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn achievement_date_stays_unknown_when_xp_outpaces_completion_history() {
        let mut agent = (
            "agent-a".to_string(),
            "Agent A".to_string(),
            "에이전트 A".to_string(),
            60,
            "🤖".to_string(),
        );
        let mut times = HashMap::from([(agent.0.clone(), vec![1_000, 2_000])]);
        let before = build_achievements(std::slice::from_ref(&agent), &times);
        agent.3 = 90;
        times.get_mut(&agent.0).unwrap().push(3_000);
        let after = build_achievements(std::slice::from_ref(&agent), &times);
        let milestone = |items: Vec<Value>| {
            items
                .into_iter()
                .find(|item| item["type"] == "getting_started")
                .unwrap()
        };
        let before = milestone(before);
        let after = milestone(after);

        assert_eq!(before["id"], after["id"]);
        assert_eq!(before["earned_at"], after["earned_at"]);
        assert_eq!(after["earned_at"], 0);
        assert_eq!(before["progress"]["current_xp"], 60);
        assert_eq!(after["progress"]["current_xp"], 90);
    }

    #[test]
    fn compute_streak_counts_consecutive_days_from_today() {
        let today = parse_date("2026-05-06").expect("valid today");

        let streak = compute_streak_from_today(&["2026-05-06", "2026-05-05", "2026-05-03"], today);

        assert_eq!(streak, 2);
    }

    #[test]
    fn compute_streak_counts_adjacent_days_across_month_boundary() {
        let today = parse_date("2026-06-01").expect("valid today");

        let streak = compute_streak_from_today(&["2026-06-01", "2026-05-31"], today);

        assert_eq!(streak, 2);
    }

    #[test]
    fn compute_streak_counts_adjacent_days_across_year_boundary() {
        let today = parse_date("2027-01-01").expect("valid today");

        let streak = compute_streak_from_today(&["2027-01-01", "2026-12-31"], today);

        assert_eq!(streak, 2);
    }

    #[test]
    fn compute_streak_ignores_invalid_dates_before_a_gap() {
        let today = parse_date("2026-05-06").expect("valid today");

        let streak = compute_streak_from_today(&["not-a-date", "2026-05-06"], today);

        assert_eq!(streak, 1);
    }
}
