//! Shared re-post rows (`o_piece_delivery`, `o_piece_receipts`), one per admitted piece. A failed
//! read is an error, never "no row": nothing here falls back to a local budget.

use std::fmt;

use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{PgExecutor, PgPool, Postgres, Row};

use super::o_piece_attempts::{self, AttemptResult};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};

type PgQuery<'q> = sqlx::query::Query<'q, Postgres, PgArguments>;

/// Placeholders `$1..$5` of every statement that names one piece.
pub(super) const KEY_MATCH: &str = "channel_id = $1 AND provider = $2 AND native_key = $3 \
                                    AND kind = $4 AND piece_index = $5";
pub(super) const KEY_COLUMNS: &str = "channel_id, provider, native_key, kind, piece_index";
const ROW_COLUMNS: &str = "channel_id, provider, native_key, kind, piece_index, payload, \
                           payload_sha256, identity_version, split_version, original_anchor, \
                           sender_id, admitted_by, origin_serial, origin_node, failure, \
                           conflict_sha256, revision";

#[derive(Debug)]
pub(crate) enum LedgerError {
    /// PostgreSQL failed or was unreachable; the caller learned nothing about the row.
    Pg(sqlx::Error),
    /// A value the schema cannot hold, or a stored row this build cannot read.
    Invalid(String),
}

impl From<sqlx::Error> for LedgerError {
    fn from(error: sqlx::Error) -> Self {
        Self::Pg(error)
    }
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pg(error) => write!(f, "re-post ledger unavailable: {error}"),
            Self::Invalid(detail) => write!(f, "re-post ledger value refused: {detail}"),
        }
    }
}

pub(super) fn to_pg(value: u64, what: &str) -> Result<i64, LedgerError> {
    i64::try_from(value).map_err(|_| LedgerError::Invalid(format!("{what} {value} exceeds BIGINT")))
}

fn from_pg(value: i64, what: &str) -> Result<u64, LedgerError> {
    u64::try_from(value).map_err(|_| LedgerError::Invalid(format!("stored {what} {value}")))
}

/// A piece's budget identity. Session, node, epoch and local serial are deliberately absent, so a
/// fork, a new holder or a renumbered ledger reaches the same row.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct PieceKey {
    unit: UnitKey,
    piece_index: u32,
}

impl PieceKey {
    /// `None` for what is never posted as its own message: tool calls and Codex tool results.
    pub(crate) fn new(unit: UnitKey, piece_index: u32) -> Option<Self> {
        let postable = match (unit.provider, unit.kind) {
            (_, UnitKind::Body) | (ShadowProvider::Claude, UnitKind::ToolResult) => true,
            (_, UnitKind::Tool) | (ShadowProvider::Codex, UnitKind::ToolResult) => false,
        };
        let named = unit.channel_id > 0 && !unit.native_key.trim().is_empty();
        (postable && named).then_some(Self { unit, piece_index })
    }

    pub(crate) fn unit(&self) -> &UnitKey {
        &self.unit
    }

    pub(crate) fn piece_index(&self) -> u32 {
        self.piece_index
    }

    pub(super) fn bind<'q>(&self, query: PgQuery<'q>) -> PgQuery<'q> {
        query
            .bind(self.unit.channel_id.to_string())
            .bind(provider_name(self.unit.provider))
            .bind(self.unit.native_key.clone())
            .bind(kind_name(self.unit.kind))
            .bind(i64::from(self.piece_index))
    }

    fn read(row: &PgRow) -> Result<Self, LedgerError> {
        let invalid = |what: &str| LedgerError::Invalid(format!("stored piece {what}"));
        let channel: String = row.try_get("channel_id")?;
        let provider = match row.try_get::<String, _>("provider")?.as_str() {
            "claude" => ShadowProvider::Claude,
            "codex" => ShadowProvider::Codex,
            _ => return Err(invalid("provider")),
        };
        let kind = match row.try_get::<String, _>("kind")?.as_str() {
            "body" => UnitKind::Body,
            "tool_result" => UnitKind::ToolResult,
            _ => return Err(invalid("kind")),
        };
        let unit = UnitKey {
            channel_id: channel.parse().map_err(|_| invalid("channel"))?,
            provider,
            native_key: row.try_get("native_key")?,
            kind,
        };
        let index = u32::try_from(row.try_get::<i64, _>("piece_index")?);
        Self::new(unit, index.map_err(|_| invalid("index"))?).ok_or_else(|| invalid("key"))
    }
}

pub(crate) fn provider_name(provider: ShadowProvider) -> &'static str {
    match provider {
        ShadowProvider::Claude => "claude",
        ShadowProvider::Codex => "codex",
    }
}

pub(crate) fn kind_name(kind: UnitKind) -> &'static str {
    match kind {
        UnitKind::Body => "body",
        UnitKind::Tool => "tool",
        UnitKind::ToolResult => "tool_result",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdmittedBy {
    /// An on original whose result went uncertain.
    Uncertain,
    /// An operator approved re-sending a rejected piece.
    Operator,
}

impl AdmittedBy {
    fn as_str(self) -> &'static str {
        match self {
            Self::Uncertain => "uncertain",
            Self::Operator => "operator",
        }
    }
}

/// Why a piece ended without a confirmed message; payload and history stay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    NotFound,
    UnknownAtCap,
    Rejected,
    Cap,
    CapUnknown,
}

impl Failure {
    const ALL: [Self; 5] = [
        Self::NotFound,
        Self::UnknownAtCap,
        Self::Rejected,
        Self::Cap,
        Self::CapUnknown,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::UnknownAtCap => "unknown_at_cap",
            Self::Rejected => "rejected",
            Self::Cap => "cap",
            Self::CapUnknown => "cap_unknown",
        }
    }

    pub(super) fn parse(raw: &str) -> Result<Self, LedgerError> {
        let found = Self::ALL
            .into_iter()
            .find(|failure| failure.as_str() == raw);
        found.ok_or_else(|| LedgerError::Invalid(format!("stored failure {raw}")))
    }
}

/// What admission records: the stored piece and the slots it already spent.
#[derive(Clone, Debug)]
pub(crate) struct NewDelivery {
    pub(crate) key: PieceKey,
    pub(crate) payload: String,
    pub(crate) payload_sha256: String,
    pub(crate) identity_version: i32,
    pub(crate) split_version: i32,
    pub(crate) original_anchor: u64,
    pub(crate) sender_id: u64,
    pub(crate) admitted_by: AdmittedBy,
    pub(crate) origin_serial: u64,
    pub(crate) origin_node: String,
    /// What the original POST came to; it takes slot 0.
    pub(crate) original: AttemptResult,
    /// Counted sends made before admission; they take slots 1 and 2, and any beyond are already over the cap.
    pub(crate) prior_retries: u32,
    pub(crate) failure: Option<Failure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeliveryRow {
    pub(crate) key: PieceKey,
    pub(crate) payload: String,
    pub(crate) payload_sha256: String,
    pub(crate) identity_version: i32,
    pub(crate) split_version: i32,
    pub(crate) original_anchor: u64,
    pub(crate) sender_id: u64,
    pub(crate) admitted_by: AdmittedBy,
    pub(crate) origin_serial: u64,
    pub(crate) origin_node: String,
    pub(crate) failure: Option<Failure>,
    pub(crate) conflict_sha256: Option<String>,
    pub(crate) revision: i64,
}

impl DeliveryRow {
    fn read(row: &PgRow) -> Result<Self, LedgerError> {
        let admitted_by = match row.try_get::<String, _>("admitted_by")?.as_str() {
            "uncertain" => AdmittedBy::Uncertain,
            "operator" => AdmittedBy::Operator,
            other => return Err(LedgerError::Invalid(format!("stored admission {other}"))),
        };
        let failure: Option<String> = row.try_get("failure")?;
        Ok(Self {
            key: PieceKey::read(row)?,
            payload: row.try_get("payload")?,
            payload_sha256: row.try_get("payload_sha256")?,
            identity_version: row.try_get("identity_version")?,
            split_version: row.try_get("split_version")?,
            original_anchor: from_pg(row.try_get("original_anchor")?, "anchor")?,
            sender_id: from_pg(row.try_get("sender_id")?, "sender")?,
            admitted_by,
            origin_serial: from_pg(row.try_get("origin_serial")?, "serial")?,
            origin_node: row.try_get("origin_node")?,
            failure: failure.as_deref().map(Failure::parse).transpose()?,
            conflict_sha256: row.try_get("conflict_sha256")?,
            revision: row.try_get("revision")?,
        })
    }

    /// The same content from the same sender under the same identity and split rules.
    fn same_piece(&self, new: &NewDelivery) -> bool {
        self.payload_sha256 == new.payload_sha256
            && self.identity_version == new.identity_version
            && self.split_version == new.split_version
            && self.sender_id == new.sender_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AdmitOutcome {
    /// This call created the row and its spent slots.
    Admitted(DeliveryRow),
    /// The same piece was already admitted; its budget stands.
    Existing(DeliveryRow),
    /// Another payload or identity holds the key. No new budget: the stored row and its slots
    /// stay, and the offered hash is kept for the operator notice.
    IdentityConflict(DeliveryRow),
}

/// Admits a piece once. Concurrent and repeated calls for a key end on one row and one slot 0.
pub(crate) async fn admit(pool: &PgPool, new: &NewDelivery) -> Result<AdmitOutcome, LedgerError> {
    let mut tx = pool.begin().await?;
    let insert = format!(
        "INSERT INTO o_piece_delivery ({KEY_COLUMNS}, payload, payload_sha256, identity_version, \
         split_version, original_anchor, sender_id, admitted_by, origin_serial, origin_node, \
         failure) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) \
         ON CONFLICT DO NOTHING"
    );
    let inserted = new
        .key
        .bind(sqlx::query(&insert))
        .bind(&new.payload)
        .bind(&new.payload_sha256)
        .bind(new.identity_version)
        .bind(new.split_version)
        .bind(to_pg(new.original_anchor, "anchor")?)
        .bind(to_pg(new.sender_id, "sender")?)
        .bind(new.admitted_by.as_str())
        .bind(to_pg(new.origin_serial, "serial")?)
        .bind(&new.origin_node)
        .bind(new.failure.map(Failure::as_str))
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if inserted {
        o_piece_attempts::record_spent(&mut tx, new).await?;
    }
    let row = select(&mut *tx, &new.key, " FOR UPDATE")
        .await?
        .ok_or_else(|| LedgerError::Invalid("admitted row vanished".into()))?;
    let outcome = if inserted {
        AdmitOutcome::Admitted(row)
    } else if row.same_piece(new) {
        AdmitOutcome::Existing(row)
    } else {
        let note = format!(
            "UPDATE o_piece_delivery SET conflict_sha256 = $6, conflict_at = NOW() \
             WHERE {KEY_MATCH} RETURNING {ROW_COLUMNS}"
        );
        let noted = new.key.bind(sqlx::query(&note)).bind(&new.payload_sha256);
        AdmitOutcome::IdentityConflict(DeliveryRow::read(&noted.fetch_one(&mut *tx).await?)?)
    };
    tx.commit().await?;
    Ok(outcome)
}

async fn select<'c>(
    executor: impl PgExecutor<'c>,
    key: &PieceKey,
    lock: &str,
) -> Result<Option<DeliveryRow>, LedgerError> {
    let sql = format!("SELECT {ROW_COLUMNS} FROM o_piece_delivery WHERE {KEY_MATCH}{lock}");
    let row = key.bind(sqlx::query(&sql)).fetch_optional(executor).await?;
    row.as_ref().map(DeliveryRow::read).transpose()
}

/// The admitted row, `None` only when PostgreSQL answered that there is none.
pub(crate) async fn load(
    pool: &PgPool,
    key: &PieceKey,
) -> Result<Option<DeliveryRow>, LedgerError> {
    select(pool, key, "").await
}

/// Every key admitted in `channel_id`, for a holder to refuse their originals before it is ready.
pub(crate) async fn admitted_keys(
    pool: &PgPool,
    channel_id: u64,
) -> Result<Vec<PieceKey>, LedgerError> {
    let sql = format!("SELECT {KEY_COLUMNS} FROM o_piece_delivery WHERE channel_id = $1");
    let rows = sqlx::query(&sql)
        .bind(channel_id.to_string())
        .fetch_all(pool)
        .await?;
    rows.iter().map(PieceKey::read).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptMethod {
    /// The POST response of a send of this piece.
    PostResponse,
    /// A durable local `Posted`, whatever Discord did to the content.
    LocalPosted,
    Marker,
    Nonce,
    ExactMatch,
}

impl ReceiptMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::PostResponse => "post_response",
            Self::LocalPosted => "local_posted",
            Self::Marker => "marker",
            Self::Nonce => "nonce",
            Self::ExactMatch => "exact_match",
        }
    }
}

/// A validated message of one piece, by the expected sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Receipt {
    pub(crate) key: PieceKey,
    pub(crate) message_id: u64,
    pub(crate) author_id: u64,
    pub(crate) slot: Option<u8>,
    pub(crate) method: ReceiptMethod,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptOutcome {
    Recorded {
        revision: i64,
    },
    /// This message was already this piece's receipt.
    Known,
    /// The message already counts for another piece; it is not moved.
    AttributedElsewhere(PieceKey),
    NotAdmitted,
    /// Not the sender the piece was admitted for; a response id alone is no success.
    WrongAuthor,
}

/// Records a message as the piece's success. The first receipt resolves the piece; later ones
/// stay as observed duplicates.
pub(crate) async fn record_receipt(
    pool: &PgPool,
    receipt: &Receipt,
) -> Result<ReceiptOutcome, LedgerError> {
    let mut tx = pool.begin().await?;
    let Some(row) = select(&mut *tx, &receipt.key, " FOR UPDATE").await? else {
        return Ok(ReceiptOutcome::NotAdmitted);
    };
    if row.sender_id != receipt.author_id {
        return Ok(ReceiptOutcome::WrongAuthor);
    }
    let insert = format!(
        "INSERT INTO o_piece_receipts ({KEY_COLUMNS}, message_id, slot, author_id, method) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING"
    );
    let inserted = receipt
        .key
        .bind(sqlx::query(&insert))
        .bind(to_pg(receipt.message_id, "message")?)
        .bind(receipt.slot.map(i16::from))
        .bind(to_pg(receipt.author_id, "author")?)
        .bind(receipt.method.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    let outcome = if inserted {
        ReceiptOutcome::Recorded {
            revision: advance(&mut tx, &receipt.key).await?,
        }
    } else {
        let sql = format!(
            "SELECT {KEY_COLUMNS} FROM o_piece_receipts WHERE channel_id = $1 AND message_id = $2"
        );
        let holder = sqlx::query(&sql)
            .bind(receipt.key.unit().channel_id.to_string())
            .bind(to_pg(receipt.message_id, "message")?)
            .fetch_one(&mut *tx)
            .await?;
        match PieceKey::read(&holder)? {
            key if key == receipt.key => ReceiptOutcome::Known,
            key => ReceiptOutcome::AttributedElsewhere(key),
        }
    };
    tx.commit().await?;
    Ok(outcome)
}

/// Bumps the row's revision inside `tx`, so evidence read before the change no longer grants.
pub(super) async fn advance(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    key: &PieceKey,
) -> Result<i64, LedgerError> {
    let sql = format!(
        "UPDATE o_piece_delivery SET revision = revision + 1, updated_at = NOW() \
         WHERE {KEY_MATCH} RETURNING revision"
    );
    Ok(key
        .bind(sqlx::query(&sql))
        .fetch_one(&mut **tx)
        .await?
        .try_get(0)?)
}
