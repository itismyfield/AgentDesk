//! Consumed POST slots (`o_piece_attempts`): slot 0 is the original and only [`grant`] consumes
//! slot 1 or 2. A slot is never deleted or reused, so a crash or refusal before its POST spends it.

use std::time::Duration;

use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use super::o_piece_delivery::{
    Failure, KEY_COLUMNS, KEY_MATCH, LedgerError, NewDelivery, PieceKey, advance,
};

/// Slots 0, 1 and 2: the original and at most two further sends.
pub(crate) const SLOTS: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptResult {
    Created,
    Rejected,
    Uncertain,
    /// Refused after the slot was consumed and before any request; the slot stays spent.
    NotSent,
    /// Its holder is gone past the deadline; whether it posted is unknown.
    Abandoned,
}

impl AttemptResult {
    const ALL: [Self; 5] = [
        Self::Created,
        Self::Rejected,
        Self::Uncertain,
        Self::NotSent,
        Self::Abandoned,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Rejected => "rejected",
            Self::Uncertain => "uncertain",
            Self::NotSent => "not_sent",
            Self::Abandoned => "abandoned",
        }
    }
}

/// Why a further send is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Intent {
    AutoReconfirm,
    OperatorResume { approval_id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttemptKind {
    Original,
    /// A counted send made before the piece was admitted.
    PriorRetry,
    AutoReconfirm,
    OperatorResume,
}

impl AttemptKind {
    const ALL: [Self; 4] = [
        Self::Original,
        Self::PriorRetry,
        Self::AutoReconfirm,
        Self::OperatorResume,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::PriorRetry => "prior_retry",
            Self::AutoReconfirm => "auto_reconfirm",
            Self::OperatorResume => "operator_resume",
        }
    }
}

fn parse<T: Copy>(all: &[T], name: fn(T) -> &'static str, raw: &str) -> Result<T, LedgerError> {
    let found = all.iter().copied().find(|value| name(*value) == raw);
    found.ok_or_else(|| LedgerError::Invalid(format!("stored attempt value {raw}")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptRow {
    pub(crate) slot: u8,
    pub(crate) kind: AttemptKind,
    pub(crate) result: Option<AttemptResult>,
}

pub(crate) struct GrantRequest<'a> {
    pub(crate) key: &'a PieceKey,
    /// The row revision the caller's evidence was read at; any later change refuses the grant.
    pub(crate) expected_revision: i64,
    pub(crate) intent: Intent,
    pub(crate) owner: &'a str,
    pub(crate) run_id: &'a str,
    /// How long the send may run; past it another holder may settle the slot as abandoned.
    pub(crate) ttl: Duration,
}

/// Proof that one grant call consumed `slot`. Only [`grant`] builds it, never from a read, and it
/// is not `Clone`, so a lost commit reply or a re-read cannot mint a second one.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SlotGrant {
    key: PieceKey,
    slot: u8,
    grant_id: Uuid,
    revision: i64,
}

impl SlotGrant {
    pub(crate) fn key(&self) -> &PieceKey {
        &self.key
    }

    pub(crate) fn slot(&self) -> u8 {
        self.slot
    }

    pub(crate) fn revision(&self) -> i64 {
        self.revision
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GrantOutcome {
    Granted(SlotGrant),
    NotAdmitted,
    /// A receipt exists; nothing more is sent.
    Resolved,
    Failed(Failure),
    /// The row moved past the caller's revision; its evidence is stale.
    Stale {
        revision: i64,
    },
    /// An earlier slot is still open; it must be settled before another is consumed.
    Open {
        slot: u8,
    },
    CapReached,
}

/// Consumes the next slot under the piece's row lock, or says why not. PostgreSQL failure is an
/// error, never a grant.
pub(crate) async fn grant(
    pool: &PgPool,
    request: GrantRequest<'_>,
) -> Result<GrantOutcome, LedgerError> {
    let key = request.key;
    let mut tx = pool.begin().await?;
    let lock =
        format!("SELECT revision, failure FROM o_piece_delivery WHERE {KEY_MATCH} FOR UPDATE");
    let Some(row) = key
        .bind(sqlx::query(&lock))
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(GrantOutcome::NotAdmitted);
    };
    let revision: i64 = row.try_get("revision")?;
    let failure: Option<String> = row.try_get("failure")?;
    let resolved = format!("SELECT EXISTS (SELECT 1 FROM o_piece_receipts WHERE {KEY_MATCH})");
    if key
        .bind(sqlx::query(&resolved))
        .fetch_one(&mut *tx)
        .await?
        .try_get(0)?
    {
        return Ok(GrantOutcome::Resolved);
    }
    if let Some(failure) = failure {
        return Ok(GrantOutcome::Failed(Failure::parse(&failure)?));
    }
    if revision != request.expected_revision {
        return Ok(GrantOutcome::Stale { revision });
    }
    let spent = attempts_in(&mut tx, key).await?;
    if let Some(open) = spent.iter().find(|attempt| attempt.result.is_none()) {
        return Ok(GrantOutcome::Open { slot: open.slot });
    }
    let slot = spent
        .iter()
        .map(|attempt| attempt.slot + 1)
        .max()
        .unwrap_or(0);
    if slot >= SLOTS {
        return Ok(GrantOutcome::CapReached);
    }
    let (kind, approval_id) = match request.intent {
        Intent::AutoReconfirm => (AttemptKind::AutoReconfirm, None),
        Intent::OperatorResume { approval_id } => (AttemptKind::OperatorResume, Some(approval_id)),
    };
    let grant_id = Uuid::new_v4();
    let insert = format!(
        "INSERT INTO o_piece_attempts ({KEY_COLUMNS}, slot, grant_id, intent, approval_id, owner, \
         run_id, deadline) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, \
         NOW() + make_interval(secs => $12))"
    );
    key.bind(sqlx::query(&insert))
        .bind(i16::from(slot))
        .bind(grant_id)
        .bind(kind.as_str())
        .bind(approval_id)
        .bind(request.owner)
        .bind(request.run_id)
        .bind(request.ttl.as_secs_f64())
        .execute(&mut *tx)
        .await?;
    let revision = advance(&mut tx, key).await?;
    tx.commit().await?;
    Ok(GrantOutcome::Granted(SlotGrant {
        key: key.clone(),
        slot,
        grant_id,
        revision,
    }))
}

/// Records how the granted send ended. `None` when the slot was already settled, by this grant or
/// by another holder after its deadline; the slot stays spent either way.
pub(crate) async fn settle(
    pool: &PgPool,
    grant: &SlotGrant,
    result: AttemptResult,
) -> Result<Option<i64>, LedgerError> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "UPDATE o_piece_attempts SET result = $6, settled_at = NOW() \
         WHERE {KEY_MATCH} AND slot = $7 AND grant_id = $8 AND result IS NULL"
    );
    let settled = grant
        .key
        .bind(sqlx::query(&sql))
        .bind(result.as_str())
        .bind(i16::from(grant.slot))
        .bind(grant.grant_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let revision = if settled == 1 {
        Some(advance(&mut tx, &grant.key).await?)
    } else {
        None
    };
    tx.commit().await?;
    Ok(revision)
}

/// Settles as abandoned every open slot of the piece whose deadline has passed on the database
/// clock, so one node's clock never decides another's lease. Returns the slots it settled.
pub(crate) async fn settle_expired(pool: &PgPool, key: &PieceKey) -> Result<Vec<u8>, LedgerError> {
    let mut tx = pool.begin().await?;
    let sql = format!(
        "UPDATE o_piece_attempts SET result = 'abandoned', settled_at = NOW() \
         WHERE {KEY_MATCH} AND result IS NULL AND deadline <= NOW() RETURNING slot"
    );
    let rows = key.bind(sqlx::query(&sql)).fetch_all(&mut *tx).await?;
    let slots = rows
        .iter()
        .map(|row| slot_of(row.try_get("slot")?))
        .collect::<Result<Vec<_>, _>>()?;
    if !slots.is_empty() {
        advance(&mut tx, key).await?;
    }
    tx.commit().await?;
    Ok(slots)
}

fn slot_of(raw: i16) -> Result<u8, LedgerError> {
    u8::try_from(raw)
        .ok()
        .filter(|slot| *slot < SLOTS)
        .ok_or_else(|| LedgerError::Invalid(format!("stored slot {raw}")))
}

/// Writes the slots a piece spent before admission, inside the admitting transaction.
pub(super) async fn record_spent(
    tx: &mut Transaction<'_, Postgres>,
    new: &NewDelivery,
) -> Result<(), LedgerError> {
    let prior = u8::try_from(new.prior_retries.min(u32::from(SLOTS - 1))).unwrap_or(SLOTS - 1);
    let run_id = format!("serial-{}", new.origin_serial);
    let insert = format!(
        "INSERT INTO o_piece_attempts ({KEY_COLUMNS}, slot, grant_id, intent, owner, run_id, \
         result, settled_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, NOW())"
    );
    for slot in 0..=prior {
        let (kind, result) = match slot {
            0 => (AttemptKind::Original, new.original),
            _ => (AttemptKind::PriorRetry, AttemptResult::Uncertain),
        };
        new.key
            .bind(sqlx::query(&insert))
            .bind(i16::from(slot))
            .bind(Uuid::new_v4())
            .bind(kind.as_str())
            .bind(&new.origin_node)
            .bind(&run_id)
            .bind(result.as_str())
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn attempts_in(
    tx: &mut Transaction<'_, Postgres>,
    key: &PieceKey,
) -> Result<Vec<AttemptRow>, LedgerError> {
    let sql = format!(
        "SELECT slot, intent, result FROM o_piece_attempts WHERE {KEY_MATCH} ORDER BY slot"
    );
    let rows = key.bind(sqlx::query(&sql)).fetch_all(&mut **tx).await?;
    rows.iter()
        .map(|row| -> Result<AttemptRow, LedgerError> {
            let kind: String = row.try_get("intent")?;
            let result: Option<String> = row.try_get("result")?;
            Ok(AttemptRow {
                slot: slot_of(row.try_get("slot")?)?,
                kind: parse(&AttemptKind::ALL, AttemptKind::as_str, &kind)?,
                result: result
                    .map(|raw| parse(&AttemptResult::ALL, AttemptResult::as_str, &raw))
                    .transpose()?,
            })
        })
        .collect()
}

/// The piece's consumed slots in order.
pub(crate) async fn attempts(
    pool: &PgPool,
    key: &PieceKey,
) -> Result<Vec<AttemptRow>, LedgerError> {
    let mut tx = pool.begin().await?;
    let rows = attempts_in(&mut tx, key).await?;
    tx.commit().await?;
    Ok(rows)
}
