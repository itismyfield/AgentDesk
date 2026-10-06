//! Channel O home rows (`o_channel_homes`). Every transition is one conditional statement: a
//! condition that no longer holds is [`HomeWrite::Stale`] and leaves the row unchanged. Each new
//! epoch comes from one sequence, so no lifecycle of a channel repeats an earlier one's epoch.
#![cfg_attr(not(test), allow(dead_code))]

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use super::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL;
use crate::services::cluster::channel_home::FORCE_AFTER;

const COLUMNS: &str =
    "channel_id, provider, state, holder, target, epoch, renewed_at, updated_at, detail";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeState {
    Releasing,
    Released,
    Worker,
    Reclaiming,
    Reclaimed,
    Orphaned,
}

impl HomeState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Releasing => "releasing",
            Self::Released => "released",
            Self::Worker => "worker",
            Self::Reclaiming => "reclaiming",
            Self::Reclaimed => "reclaimed",
            Self::Orphaned => "orphaned",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        [
            Self::Releasing,
            Self::Released,
            Self::Worker,
            Self::Reclaiming,
            Self::Reclaimed,
            Self::Orphaned,
        ]
        .into_iter()
        .find(|state| state.as_str() == raw)
    }
}

/// A delegated channel's provider in the form the router stamps on intake rows, which claims
/// compare exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeProvider {
    Claude,
    Codex,
}

impl HomeProvider {
    /// Accepts `claude` or `codex` in any case and surrounding space; anything else is refused.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// How long a holder's lease must have been silent before `force`; never shorter than F.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ForceWindow(Duration);

impl ForceWindow {
    pub(crate) const MIN: Self = Self(FORCE_AFTER);

    pub(crate) fn at_least(after: Duration) -> Option<Self> {
        (after >= FORCE_AFTER).then_some(Self(after))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelHome {
    pub(crate) channel_id: String,
    pub(crate) provider: String,
    pub(crate) state: HomeState,
    pub(crate) holder: Option<String>,
    pub(crate) target: Option<String>,
    pub(crate) epoch: i64,
    pub(crate) renewed_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) detail: Option<String>,
}

/// A renewal write that matched `holder` at `epoch`. Only [`renew`] builds one, so a read can
/// never stand in for the conditional write that opens a home gate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HeldHome {
    channel_id: String,
    holder: String,
    epoch: i64,
    state: HomeState,
}

impl HeldHome {
    pub(crate) fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub(crate) fn holder(&self) -> &str {
        &self.holder
    }

    pub(crate) fn epoch(&self) -> i64 {
        self.epoch
    }

    pub(crate) fn state(&self) -> HomeState {
        self.state
    }

    #[cfg(test)]
    pub(crate) fn for_test(channel_id: &str, holder: &str, epoch: i64, state: HomeState) -> Self {
        Self {
            channel_id: channel_id.to_string(),
            holder: holder.to_string(),
            epoch,
            state,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HomeWrite<T> {
    Applied(T),
    /// The row no longer met the write's condition; nothing changed.
    Stale,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ForceOutcome {
    Orphaned(ChannelHome),
    /// The holder renewed within the force window; nothing changed.
    Fresh,
    /// No holder at the observed epoch; nothing changed.
    Stale,
}

/// A failed read or write: callers hold instead of deciding, never treating it as "no row".
#[derive(Debug)]
pub(crate) enum HomeError {
    Db(sqlx::Error),
    Undecodable { channel_id: String, detail: String },
}

impl std::fmt::Display for HomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(error) => write!(f, "o_channel_homes query failed: {error}"),
            Self::Undecodable { channel_id, detail } => {
                write!(f, "o_channel_homes row {channel_id} undecodable: {detail}")
            }
        }
    }
}

impl From<sqlx::Error> for HomeError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(error)
    }
}

fn decode(row: &PgRow) -> Result<ChannelHome, HomeError> {
    let channel_id: String = row.try_get("channel_id")?;
    let raw: String = row.try_get("state")?;
    let Some(state) = HomeState::parse(&raw) else {
        return Err(HomeError::Undecodable {
            channel_id,
            detail: format!("state {raw:?}"),
        });
    };
    Ok(ChannelHome {
        channel_id,
        provider: row.try_get("provider")?,
        state,
        holder: row.try_get("holder")?,
        target: row.try_get("target")?,
        epoch: row.try_get("epoch")?,
        renewed_at: row.try_get("renewed_at")?,
        updated_at: row.try_get("updated_at")?,
        detail: row.try_get("detail")?,
    })
}

fn written(row: Option<PgRow>) -> Result<HomeWrite<ChannelHome>, HomeError> {
    match row {
        Some(row) => decode(&row).map(HomeWrite::Applied),
        None => Ok(HomeWrite::Stale),
    }
}

/// The channel's home row; `None` means no row, so the gateway rules apply.
pub(crate) async fn read_home(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<ChannelHome>, HomeError> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM o_channel_homes WHERE channel_id = $1"
    ))
    .bind(channel_id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(decode).transpose()
}

/// Operator `delegate`: a gateway-owned channel starts releasing to `target` at a fresh epoch.
pub(crate) async fn delegate(
    pool: &PgPool,
    channel_id: &str,
    provider: HomeProvider,
    gateway: &str,
    target: &str,
) -> Result<HomeWrite<ChannelHome>, HomeError> {
    let row = sqlx::query(&format!(
        "INSERT INTO o_channel_homes (channel_id, provider, state, holder, target, epoch, renewed_at)
         SELECT $1, $2, 'releasing', $3, $4, nextval('o_channel_home_epochs'), NOW()
          WHERE NOT EXISTS (SELECT 1 FROM o_channel_homes WHERE channel_id = $1)
         ON CONFLICT (channel_id) DO NOTHING
         RETURNING {COLUMNS}"
    ))
    .bind(channel_id)
    .bind(provider.as_str())
    .bind(gateway)
    .bind(target)
    .fetch_optional(pool)
    .await?;
    written(row)
}

/// A drained holder leaves: `releasing` → `released` (or `reclaiming` → `reclaimed`), no
/// holder, a fresh epoch.
async fn leave(
    pool: &PgPool,
    channel_id: &str,
    from: HomeState,
    to: HomeState,
    holder: &str,
    epoch: i64,
) -> Result<HomeWrite<ChannelHome>, HomeError> {
    let row = sqlx::query(&format!(
        "UPDATE o_channel_homes
            SET state = $5, holder = NULL, epoch = nextval('o_channel_home_epochs'),
                updated_at = NOW()
          WHERE channel_id = $1 AND state = $2 AND holder = $3 AND epoch = $4
          RETURNING {COLUMNS}"
    ))
    .bind(channel_id)
    .bind(from.as_str())
    .bind(holder)
    .bind(epoch)
    .bind(to.as_str())
    .fetch_optional(pool)
    .await?;
    written(row)
}

pub(crate) async fn finish_release(
    pool: &PgPool,
    channel_id: &str,
    holder: &str,
    epoch: i64,
) -> Result<HomeWrite<ChannelHome>, HomeError> {
    let (from, to) = (HomeState::Releasing, HomeState::Released);
    leave(pool, channel_id, from, to, holder, epoch).await
}

pub(crate) async fn finish_reclaim(
    pool: &PgPool,
    channel_id: &str,
    holder: &str,
    epoch: i64,
) -> Result<HomeWrite<ChannelHome>, HomeError> {
    let (from, to) = (HomeState::Reclaiming, HomeState::Reclaimed);
    leave(pool, channel_id, from, to, holder, epoch).await
}

/// The named target takes a released channel: `released` → `worker` with itself as holder.
pub(crate) async fn adopt(
    pool: &PgPool,
    channel_id: &str,
    target: &str,
    epoch: i64,
) -> Result<HomeWrite<ChannelHome>, HomeError> {
    let row = sqlx::query(&format!(
        "UPDATE o_channel_homes
            SET state = 'worker', holder = $2, target = NULL, renewed_at = NOW(), updated_at = NOW()
          WHERE channel_id = $1 AND state = 'released' AND target = $2 AND epoch = $3
          RETURNING {COLUMNS}"
    ))
    .bind(channel_id)
    .bind(target)
    .bind(epoch)
    .fetch_optional(pool)
    .await?;
    written(row)
}

/// Operator `reclaim`: the worker keeps holding and starts draining back to `gateway`.
pub(crate) async fn begin_reclaim(
    pool: &PgPool,
    channel_id: &str,
    epoch: i64,
    gateway: &str,
) -> Result<HomeWrite<ChannelHome>, HomeError> {
    let row = sqlx::query(&format!(
        "UPDATE o_channel_homes
            SET state = 'reclaiming', target = $3, updated_at = NOW()
          WHERE channel_id = $1 AND state = 'worker' AND epoch = $2
          RETURNING {COLUMNS}"
    ))
    .bind(channel_id)
    .bind(epoch)
    .bind(gateway)
    .fetch_optional(pool)
    .await?;
    written(row)
}

/// The gateway drops a reclaimed row, returning the channel to the gateway rules.
pub(crate) async fn remove_reclaimed(
    pool: &PgPool,
    channel_id: &str,
    target: &str,
    epoch: i64,
) -> Result<HomeWrite<()>, HomeError> {
    let deleted = sqlx::query(
        "DELETE FROM o_channel_homes
          WHERE channel_id = $1 AND state = 'reclaimed' AND target = $2 AND epoch = $3",
    )
    .bind(channel_id)
    .bind(target)
    .bind(epoch)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(if deleted == 1 {
        HomeWrite::Applied(())
    } else {
        HomeWrite::Stale
    })
}

/// The holder's lease renewal. Only this write yields a [`HeldHome`].
pub(crate) async fn renew(
    pool: &PgPool,
    channel_id: &str,
    holder: &str,
    epoch: i64,
) -> Result<HomeWrite<HeldHome>, HomeError> {
    let row = sqlx::query(&format!(
        "UPDATE o_channel_homes SET renewed_at = NOW()
          WHERE channel_id = $1 AND holder = $2 AND epoch = $3
            AND state IN ('releasing', 'worker', 'reclaiming')
          RETURNING {COLUMNS}"
    ))
    .bind(channel_id)
    .bind(holder)
    .bind(epoch)
    .fetch_optional(pool)
    .await?;
    Ok(match written(row)? {
        HomeWrite::Applied(home) => HomeWrite::Applied(HeldHome {
            channel_id: home.channel_id,
            holder: holder.to_string(),
            epoch: home.epoch,
            state: home.state,
        }),
        HomeWrite::Stale => HomeWrite::Stale,
    })
}

/// Operator `force`: a holder silent longer than `window` drops to `orphaned`. The window is
/// checked on the locked row, so a renewal that commits first keeps the holder.
pub(crate) async fn force_orphan(
    pool: &PgPool,
    channel_id: &str,
    epoch: i64,
    window: ForceWindow,
    detail: &str,
) -> Result<ForceOutcome, HomeError> {
    let row = sqlx::query(&format!(
        "UPDATE o_channel_homes
            SET state = 'orphaned', holder = NULL, epoch = nextval('o_channel_home_epochs'),
                updated_at = NOW(), detail = $4
          WHERE channel_id = $1 AND epoch = $2 AND holder IS NOT NULL
            AND NOW() - renewed_at > make_interval(secs => $3)
          RETURNING {COLUMNS}"
    ))
    .bind(channel_id)
    .bind(epoch)
    .bind(window.0.as_secs_f64())
    .bind(detail)
    .fetch_optional(pool)
    .await?;
    if let Some(row) = row {
        return decode(&row).map(ForceOutcome::Orphaned);
    }
    // Labels the refusal only; neither label changes the row.
    let held = read_home(pool, channel_id)
        .await?
        .is_some_and(|home| home.epoch == epoch && home.holder.is_some());
    Ok(if held {
        ForceOutcome::Fresh
    } else {
        ForceOutcome::Stale
    })
}

/// Every home row, by channel.
pub(crate) async fn list_homes(pool: &PgPool) -> Result<Vec<ChannelHome>, HomeError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM o_channel_homes ORDER BY channel_id"
    ))
    .fetch_all(pool)
    .await?;
    rows.iter().map(decode).collect()
}

/// A channel's open intake rows by the home epoch they were routed at, against the row's
/// current `epoch`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenIntake {
    /// Claimable by the current holder.
    pub(crate) current: i64,
    /// Routed at another epoch; no holder claims them again.
    pub(crate) other_epoch: i64,
    /// Routed before the channel had a row; no holder claims them while one exists.
    pub(crate) unrouted: i64,
}

pub(crate) async fn open_intake_by_epoch(
    pool: &PgPool,
    channel_id: &str,
    epoch: i64,
) -> Result<OpenIntake, HomeError> {
    let row = sqlx::query(&format!(
        "SELECT COUNT(*) FILTER (WHERE home_epoch = $2) AS current,
                COUNT(*) FILTER (WHERE home_epoch <> $2) AS other_epoch,
                COUNT(*) FILTER (WHERE home_epoch IS NULL) AS unrouted
           FROM intake_outbox
          WHERE channel_id = $1 AND status IN ({INTAKE_OUTBOX_OPEN_STATUSES_SQL})"
    ))
    .bind(channel_id)
    .bind(epoch)
    .fetch_one(pool)
    .await?;
    Ok(OpenIntake {
        current: row.try_get("current")?,
        other_epoch: row.try_get("other_epoch")?,
        unrouted: row.try_get("unrouted")?,
    })
}

#[cfg(test)]
#[path = "o_channel_homes_tests.rs"]
mod tests;
