use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::{PgPool, Row};

use crate::db::session_status::{
    AWAITING_BG, AWAITING_USER, DISCONNECTED, IDLE, LEGACY_WORKING, TURN_ACTIVE,
};

pub const ACTIVE_TOOL_WINDOW_SECS: i64 = 5;
pub const STUCK_TOOL_WINDOW_SECS: i64 = 5 * 60;
pub const STALE_CHILD_WINDOW_SECS: i64 = 24 * 60 * 60;

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
    pub stale_alive: Vec<ChildInventoryItem>,
    pub closed_count: usize,
}

impl ChildInventorySummary {
    pub fn effective_active_children(&self, recorded_active_children: i32) -> i32 {
        if self.alive.is_empty() && self.stale_alive.is_empty() && self.closed_count == 0 {
            return recorded_active_children.max(0);
        }
        i32::try_from(self.alive.len()).unwrap_or(i32::MAX)
    }
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
        } else if child_item_is_stale_alive(&item, Utc::now()) {
            summary.stale_alive.push(item);
        } else {
            summary.alive.push(item);
        }
    }

    Ok(summary)
}

fn child_item_is_stale_alive(item: &ChildInventoryItem, now: DateTime<Utc>) -> bool {
    item.closed_at.is_none()
        && item
            .spawned_at
            .map(|spawned| {
                now.signed_duration_since(spawned).num_seconds() >= STALE_CHILD_WINDOW_SECS
            })
            .unwrap_or(false)
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
