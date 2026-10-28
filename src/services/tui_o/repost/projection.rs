//! Shared receipts as a read-only view over a channel's local ledger: a rejection or approval of
//! a piece PostgreSQL holds a receipt for reads as settled; ledger and shared rows stay as written.

use std::collections::BTreeMap;

use sqlx::Row;
use sqlx::postgres::PgPool;

use super::identity::{IDENTITY_VERSION, SPLIT_VERSION, payload_sha256, piece_of};
use super::o_piece_delivery::{LedgerError, kind_name, provider_name, to_pg};
use crate::services::tui_o::shadow::UnitKey;
use crate::services::tui_o::store::ledger::{LedgerState, PieceDisposition, PieceRecord};

/// A piece as stored: provider, native key, kind and index.
type Stored = (String, String, String, i64);

#[derive(Default)]
pub(crate) enum ReceiptOverlay {
    /// No re-post hook is installed; the local ledger decides alone.
    #[default]
    Disabled,
    /// The channel's receipted pieces, each with the payload hash it was admitted under.
    Known {
        channel: u64,
        receipted: BTreeMap<Stored, String>,
    },
    /// The read failed or held a row this build cannot read; nothing reads as settled.
    Unknown(String),
}

impl ReceiptOverlay {
    /// One read of the channel's receipted pieces by `sender` under this build's identity rules.
    pub(crate) async fn load(pool: &PgPool, channel: u64, sender: u64) -> Self {
        match receipted(pool, channel, sender).await {
            Ok(receipted) => Self::Known { channel, receipted },
            Err(error) => Self::Unknown(error.to_string()),
        }
    }

    /// Whether a shared receipt resolves this local attempt with the same key and payload.
    fn settles(&self, record: &PieceRecord) -> bool {
        let Self::Known { channel, receipted } = self else {
            return false;
        };
        let Some(key) = piece_of(record) else {
            return false;
        };
        let unit = key.unit();
        let stored = (
            provider_name(unit.provider).to_owned(),
            unit.native_key.clone(),
            kind_name(unit.kind).to_owned(),
            i64::from(key.piece_index()),
        );
        unit.channel_id == *channel
            && receipted
                .get(&stored)
                .is_some_and(|sha| *sha == payload_sha256(&record.payload))
    }

    /// The ledger's disposition, with a receipted rejection or approval read as settled.
    pub(crate) fn disposition(
        &self,
        ledger: &LedgerState,
        unit: &UnitKey,
        index: u32,
    ) -> PieceDisposition {
        match ledger.disposition(unit, index) {
            PieceDisposition::Blocked { .. } | PieceDisposition::Authorized { .. }
                if ledger
                    .latest_piece(unit, index)
                    .is_some_and(|(_, record)| self.settles(record)) =>
            {
                PieceDisposition::Settled
            }
            local => local,
        }
    }

    /// As [`LedgerState::blocked`], less the rejections a receipt resolved.
    pub(crate) fn blocked(&self, ledger: &LedgerState) -> Option<(u64, u16)> {
        (0..ledger.next_serial())
            .filter_map(|serial| ledger.piece(serial))
            .filter(|piece| ledger.excluded(&piece.unit_key).is_none())
            .find_map(
                |piece| match self.disposition(ledger, &piece.unit_key, piece.piece_index) {
                    PieceDisposition::Blocked { serial, status } => Some((serial, status)),
                    _ => None,
                },
            )
    }
}

async fn receipted(
    pool: &PgPool,
    channel: u64,
    sender: u64,
) -> Result<BTreeMap<Stored, String>, LedgerError> {
    let sql = "SELECT d.provider, d.native_key, d.kind, d.piece_index, d.payload_sha256 \
               FROM o_piece_delivery d WHERE d.channel_id = $1 AND d.identity_version = $2 \
               AND d.split_version = $3 AND d.sender_id = $4 AND EXISTS (SELECT 1 \
               FROM o_piece_receipts r WHERE r.channel_id = d.channel_id \
               AND r.provider = d.provider AND r.native_key = d.native_key AND r.kind = d.kind \
               AND r.piece_index = d.piece_index AND r.author_id = d.sender_id)";
    let rows = sqlx::query(sql)
        .bind(channel.to_string())
        .bind(IDENTITY_VERSION)
        .bind(SPLIT_VERSION)
        .bind(to_pg(sender, "sender")?)
        .fetch_all(pool)
        .await?;
    let mut receipted = BTreeMap::new();
    for row in rows {
        let stored = (
            row.try_get("provider")?,
            row.try_get("native_key")?,
            row.try_get("kind")?,
            row.try_get("piece_index")?,
        );
        receipted.insert(stored, row.try_get("payload_sha256")?);
    }
    Ok(receipted)
}

/// Receipt ids are candidates, including receipts found only by a later shared probe.
#[cfg(unix)]
pub(crate) async fn delivery_candidates(
    pool: &PgPool,
    key: &super::o_piece_delivery::PieceKey,
    sender: u64,
    payload: &str,
) -> Result<Vec<u64>, LedgerError> {
    let sql = "SELECT r.message_id FROM o_piece_delivery d JOIN o_piece_receipts r \
        USING (channel_id, provider, native_key, kind, piece_index) \
        WHERE d.channel_id=$1 AND d.provider=$2 AND d.native_key=$3 AND d.kind=$4 \
        AND d.piece_index=$5 AND d.identity_version=$6 AND d.split_version=$7 \
        AND d.sender_id=$8 AND r.author_id=d.sender_id AND d.payload_sha256=$9 \
        AND d.payload=$10 ORDER BY r.message_id LIMIT 8";
    let rows = key
        .bind(sqlx::query(sql))
        .bind(IDENTITY_VERSION)
        .bind(SPLIT_VERSION)
        .bind(to_pg(sender, "sender")?)
        .bind(payload_sha256(payload))
        .bind(payload.to_owned())
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|row| {
            let id: i64 = row.try_get("message_id")?;
            u64::try_from(id).map_err(|_| LedgerError::Invalid("receipt message id".into()))
        })
        .collect()
}
