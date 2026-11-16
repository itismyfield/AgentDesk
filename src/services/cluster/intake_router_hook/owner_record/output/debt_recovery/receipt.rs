//! Unknown outcomes keep the effect pending; only exact Posted proof consumes it.
use super::*;
#[cfg(test)]
use crate::services::tui_o::store::ledger::{PieceOutcome, PieceRecord, PostConfirmation};
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    version: u8,
    effect: Uuid,
    unit: Unit,
    piece: u32,
    payload_sha: String,
    bot: u64,
    message: u64,
}
impl Proof {
    fn matches(&self, r: &Reference) -> bool {
        self.version == 1
            && self.effect == r.effect_id
            && self.unit == r.unit_key
            && self.piece == r.piece_index
            && self.payload_sha == r.payload_sha
            && self.bot == r.authority.bot_id
            && self.message > 0
    }
}
pub(super) fn admit(
    state: &str,
    expected: &str,
    value: Option<&Value>,
    r: &Reference,
) -> Result<(), AcquireError> {
    if let Some(value) = value {
        let proof: Proof =
            serde_json::from_value(value.clone()).map_err(|_| denied("debt receipt unreadable"))?;
        if !proof.matches(r) {
            return Err(denied("debt receipt identity changed"));
        }
    }
    if state != expected || value.is_some() {
        return Err(denied("debt effect is pending or already confirmed"));
    }
    Ok(())
}
#[cfg(test)]
pub(super) async fn confirm(
    pool: &sqlx::PgPool,
    r: &Reference,
    serial: u64,
    piece: &PieceRecord,
    confirmation: &PostConfirmation,
) -> Result<(), String> {
    if !confirmation.matches(serial, piece, r.authority.bot_id)
        || serde_json::to_value(&piece.unit_key).map_err(|e| e.to_string())?
            != serde_json::to_value(&r.unit_key).map_err(|e| e.to_string())?
        || piece.piece_index != r.piece_index
        || hash(&piece.payload) != r.payload_sha
    {
        return Err("debt Posted proof does not match effect".into());
    }
    let Some(PieceOutcome::Posted(message)) = piece.outcome else {
        return Err("debt body not posted".into());
    };
    let proof = Proof {
        version: 1,
        effect: r.effect_id,
        unit: r.unit_key.clone(),
        piece: r.piece_index,
        payload_sha: r.payload_sha.clone(),
        bot: r.authority.bot_id,
        message,
    };
    let mut tx = begin(pool).await.map_err(|e| e.to_string())?;
    crate::db::channel_custody_lock::lock(&mut tx, &OwnerIdentity::new(&r.provider, &r.channel))
        .await
        .map_err(|e| e.to_string())?;
    registry::structural(&mut tx, r)
        .await
        .map_err(|e| e.to_string())?;
    let changed=sqlx::query("UPDATE o_herdr_debt_effects e SET state='confirmed',receipt=$2 WHERE e.id=$1 AND e.state='reserved' AND EXISTS(SELECT 1 FROM o_herdr_debt_attempts a WHERE a.id=e.attempt_id AND a.grant_id=$3 AND a.phase='reserved')")
        .bind(r.effect_id).bind(serde_json::to_value(&proof).map_err(|e|e.to_string())?).bind(r.grant_id).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    if changed.rows_affected() != 1 {
        return Err("debt receipt does not own current attempt".into());
    }
    sqlx::query(
        "UPDATE o_herdr_debt_attempts SET phase='confirmed' WHERE grant_id=$1 AND phase='reserved'",
    )
    .bind(r.grant_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE o_herdr_debt_grants SET state='consumed' WHERE id=$1")
        .bind(r.grant_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}
