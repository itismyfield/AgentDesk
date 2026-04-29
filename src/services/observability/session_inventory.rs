use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::db::session_status::{
    AWAITING_BG, AWAITING_USER, DISCONNECTED, IDLE, LEGACY_WORKING, TURN_ACTIVE,
};

pub const ACTIVE_TOOL_WINDOW_SECS: i64 = 5;
pub const STUCK_TOOL_WINDOW_SECS: i64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChildInventoryItem {
    pub id: i64,
    pub session_key: String,
    pub purpose: Option<String>,
    pub spawned_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ChildInventorySummary {
    pub alive: Vec<ChildInventoryItem>,
    pub closed_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VisualStatus {
    Active,
    IdleBgWait,
    StuckSuspect,
    IdleNoPending,
}

impl VisualStatus {
    pub fn code(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::IdleBgWait => "idle-bg-wait",
            Self::StuckSuspect => "stuck-suspect",
            Self::IdleNoPending => "idle-no-pending",
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Self::Active => "🟢",
            Self::IdleBgWait => "💤",
            Self::StuckSuspect => "⚠️",
            Self::IdleNoPending => "⚪",
        }
    }

    pub fn display(self) -> String {
        format!("{} {}", self.emoji(), self.code())
    }
}

pub fn derive_visual_status(
    raw_status: Option<&str>,
    last_tool_at: Option<DateTime<Utc>>,
    active_children: i32,
    now: DateTime<Utc>,
) -> VisualStatus {
    let normalized = raw_status
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(IDLE)
        .to_ascii_lowercase();

    if normalized == TURN_ACTIVE
        && last_tool_at
            .map(|last| now.signed_duration_since(last).num_seconds() > STUCK_TOOL_WINDOW_SECS)
            .unwrap_or(false)
        && active_children == 0
    {
        return VisualStatus::StuckSuspect;
    }

    if normalized == TURN_ACTIVE
        || last_tool_at
            .map(|last| now.signed_duration_since(last).num_seconds() < ACTIVE_TOOL_WINDOW_SECS)
            .unwrap_or(false)
    {
        return VisualStatus::Active;
    }

    if normalized == AWAITING_BG || active_children > 0 {
        return VisualStatus::IdleBgWait;
    }

    if normalized == LEGACY_WORKING
        && last_tool_at
            .map(|last| now.signed_duration_since(last).num_seconds() > STUCK_TOOL_WINDOW_SECS)
            .unwrap_or(false)
    {
        return VisualStatus::StuckSuspect;
    }

    if matches!(
        normalized.as_str(),
        AWAITING_USER | IDLE | DISCONNECTED | "error" | "aborted"
    ) {
        return VisualStatus::IdleNoPending;
    }

    VisualStatus::IdleNoPending
}

pub async fn load_child_inventory_by_parent_key_pg(
    pool: &PgPool,
    parent_session_key: &str,
) -> Result<ChildInventorySummary, sqlx::Error> {
    let parent_session_key = parent_session_key.trim();
    if parent_session_key.is_empty() {
        return Ok(ChildInventorySummary::default());
    }

    let rows = sqlx::query(
        "SELECT c.id,
                COALESCE(c.session_key, '') AS session_key,
                c.purpose,
                c.spawned_at,
                c.closed_at
           FROM sessions p
           JOIN sessions c ON c.parent_session_id = p.id
          WHERE p.session_key = $1
          ORDER BY c.spawned_at ASC NULLS LAST, c.id ASC",
    )
    .bind(parent_session_key)
    .fetch_all(pool)
    .await?;

    let mut summary = ChildInventorySummary::default();
    for row in rows {
        let item = ChildInventoryItem {
            id: row.try_get("id")?,
            session_key: row.try_get("session_key")?,
            purpose: row.try_get("purpose").ok().flatten(),
            spawned_at: row.try_get("spawned_at").ok().flatten(),
            closed_at: row.try_get("closed_at").ok().flatten(),
        };
        if item.closed_at.is_some() {
            summary.closed_count += 1;
        } else {
            summary.alive.push(item);
        }
    }

    Ok(summary)
}

pub fn format_child_inventory_progress(
    summary: &ChildInventorySummary,
    now: DateTime<Utc>,
) -> Option<String> {
    if summary.alive.is_empty() {
        return None;
    }

    let alive = summary
        .alive
        .iter()
        .take(4)
        .enumerate()
        .map(|(index, child)| {
            let label = child_label(index);
            let age = child
                .spawned_at
                .map(|spawned| now.signed_duration_since(spawned))
                .unwrap_or_else(Duration::zero);
            format!("{label} {}", format_compact_duration(age))
        })
        .collect::<Vec<_>>()
        .join(", ");

    let extra = summary.alive.len().saturating_sub(4);
    let alive = if extra > 0 {
        format!("{alive}, +{extra} more")
    } else {
        alive
    };

    Some(format!(
        "{} alive ({alive}) / {} closed",
        summary.alive.len(),
        summary.closed_count
    ))
}

pub fn format_compact_duration(duration: Duration) -> String {
    let total = duration.num_seconds().max(0);
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn child_label(index: usize) -> String {
    if index < 26 {
        let letter = char::from(b'A' + u8::try_from(index).unwrap_or(0));
        format!("#{letter}")
    } else {
        format!("#{}", index + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_status_distinguishes_four_states() {
        let now = Utc::now();
        assert_eq!(
            derive_visual_status(
                Some("turn_active"),
                Some(now - Duration::seconds(4)),
                0,
                now
            ),
            VisualStatus::Active
        );
        assert_eq!(
            derive_visual_status(
                Some("awaiting_bg"),
                Some(now - Duration::seconds(30)),
                2,
                now
            ),
            VisualStatus::IdleBgWait
        );
        assert_eq!(
            derive_visual_status(
                Some("turn_active"),
                Some(now - Duration::minutes(6)),
                0,
                now
            ),
            VisualStatus::StuckSuspect
        );
        assert_eq!(
            derive_visual_status(Some("awaiting_user"), None, 0, now),
            VisualStatus::IdleNoPending
        );
        assert_eq!(
            derive_visual_status(Some("idle"), None, 0, now),
            VisualStatus::IdleNoPending
        );
        assert_eq!(
            derive_visual_status(Some("working"), Some(now - Duration::seconds(4)), 0, now),
            VisualStatus::Active
        );
        assert_eq!(
            derive_visual_status(Some("working"), Some(now - Duration::minutes(6)), 0, now),
            VisualStatus::StuckSuspect
        );
        assert_eq!(
            derive_visual_status(Some("aborted"), None, 0, now),
            VisualStatus::IdleNoPending
        );
    }

    #[test]
    fn child_inventory_progress_omits_empty_alive_slot() {
        let now = Utc::now();
        let summary = ChildInventorySummary {
            alive: Vec::new(),
            closed_count: 3,
        };
        assert_eq!(format_child_inventory_progress(&summary, now), None);
    }

    #[test]
    fn child_inventory_progress_formats_alive_and_closed_counts() {
        let now = Utc::now();
        let summary = ChildInventorySummary {
            alive: vec![
                ChildInventoryItem {
                    id: 1,
                    session_key: "child-a".to_string(),
                    purpose: None,
                    spawned_at: Some(now - Duration::seconds(252)),
                    closed_at: None,
                },
                ChildInventoryItem {
                    id: 2,
                    session_key: "child-b".to_string(),
                    purpose: None,
                    spawned_at: Some(now - Duration::seconds(65)),
                    closed_at: None,
                },
            ],
            closed_count: 1,
        };
        assert_eq!(
            format_child_inventory_progress(&summary, now).as_deref(),
            Some("2 alive (#A 4m12s, #B 1m05s) / 1 closed")
        );
    }
}
