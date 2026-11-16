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
        let kept: Option<Value> = sqlx::query_scalar(
            "SELECT receipt FROM o_herdr_debt_effects WHERE id=$1 AND state='confirmed'",
        )
        .bind(r.effect_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .flatten();
        if kept.as_ref() != Some(&serde_json::to_value(&proof).map_err(|e| e.to_string())?) {
            return Err("debt receipt does not own current attempt".into());
        }
    }
    sqlx::query(
        "UPDATE o_herdr_debt_attempts SET phase='confirmed' WHERE grant_id=$1 AND phase='reserved'",
    )
    .bind(r.grant_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("UPDATE o_herdr_debt_grants SET state='consumed' WHERE effect_id=$1 AND state IN ('issued','reserved')")
        .bind(r.effect_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

#[cfg(test)]
pub(crate) struct Effect {
    pub(crate) index: u32,
    pub(crate) state: String,
    pub(crate) receipt: Option<PostedReceipt>,
}
#[cfg(test)]
pub(crate) struct Coverage(
    Vec<(
        crate::services::tui_o::shadow::UnitKey,
        u32,
        String,
        super::super::Artifact,
    )>,
);
#[cfg(test)]
impl Coverage {
    pub(crate) fn aliases(
        &self,
    ) -> impl Iterator<
        Item = &(
            crate::services::tui_o::shadow::UnitKey,
            u32,
            String,
            super::super::Artifact,
        ),
    > {
        self.0.iter()
    }
}
#[cfg(test)]
impl authority::Session {
    pub(crate) async fn inspect(&self, plan: &impl Registration) -> Result<Vec<Effect>, String> {
        let key = plan.lookup();
        if key["provider"] != self.binding.provider || key["channel"] != self.binding.channel {
            return Err("debt lookup destination changed".into());
        }
        let rows:Vec<(Uuid,i32,String,Option<Value>,Value,String,i64,Value)>=sqlx::query_as("SELECT id,piece_index,state,receipt,unit_key,payload_sha,bot_id,snapshots FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND input_key=$4 AND plan_sha=$5 ORDER BY piece_index")
            .bind(&self.binding.provider).bind(&self.binding.channel).bind(key["episode"].as_str().ok_or("debt episode")?).bind(key["input_key"].as_str().ok_or("debt input")?).bind(key["plan_sha"].as_str().ok_or("debt plan")?).fetch_all(&self.scope.pool).await.map_err(|e|e.to_string())?;
        rows.into_iter()
            .map(
                |(id, index, state, receipt, unit, payload, bot, snapshots)| {
                    if key["snapshots"]
                        .as_array()
                        .ok_or("debt snapshots")?
                        .iter()
                        .any(|s| !snapshots.as_array().is_some_and(|known| known.contains(s)))
                    {
                        return Err("unregistered immutable debt snapshot".into());
                    }
                    let receipt = match receipt {
                        Some(value) => {
                            let proof: Proof =
                                serde_json::from_value(value).map_err(|e| e.to_string())?;
                            if state != "confirmed"
                                || proof.version != 1
                                || proof.effect != id
                                || serde_json::to_value(&proof.unit).map_err(|e| e.to_string())?
                                    != unit
                                || i64::from(proof.piece) != i64::from(index)
                                || proof.payload_sha != payload
                                || i64::try_from(proof.bot) != Ok(bot)
                                || proof.bot != self.binding.bot_id
                                || proof.message == 0
                            {
                                return Err("debt receipt is not exact coverage".into());
                            }
                            Some(PostedReceipt {
                                version: 1,
                                effect: id,
                                provider: self.binding.provider.clone(),
                                channel: self
                                    .binding
                                    .channel
                                    .parse()
                                    .map_err(|_| "debt channel")?,
                                episode: key["episode"].as_str().ok_or("debt episode")?.into(),
                                input_key: key["input_key"].as_str().ok_or("debt input")?.into(),
                                plan_sha: key["plan_sha"].as_str().ok_or("debt plan")?.into(),
                                unit: proof.unit,
                                index: proof.piece,
                                payload_sha: proof.payload_sha,
                                bot: proof.bot,
                                message: proof.message,
                            })
                        }
                        None if state == "confirmed" => {
                            return Err("confirmed debt has no Posted proof".into());
                        }
                        None => None,
                    };
                    Ok(Effect {
                        index: u32::try_from(index).map_err(|e| e.to_string())?,
                        state,
                        receipt,
                    })
                },
            )
            .collect()
    }
    pub(crate) async fn confirm(
        &self,
        artifact: &super::super::Artifact,
        serial: u64,
        piece: &PieceRecord,
        proof: &PostConfirmation,
    ) -> Result<(), String> {
        let super::super::Artifact::Debt(debt) = artifact else {
            return Err("debt receipt lacks its original capability".into());
        };
        if debt.debt_recovery.provider != self.binding.provider
            || debt.debt_recovery.channel != self.binding.channel
        {
            return Err("receipt destination mismatch".into());
        }
        confirm(&self.scope.pool, &debt.debt_recovery, serial, piece, proof).await
    }
    pub(crate) async fn coverage(
        &self,
        plan: &impl Registration,
    ) -> Result<Option<Coverage>, String> {
        let effects = self.inspect(plan).await?;
        let key = plan.lookup();
        if effects.is_empty()
            || effects.len() != key["pieces"].as_u64().ok_or("debt fragment count")? as usize
            || effects.iter().any(|e| e.receipt.is_none())
        {
            return Ok(None);
        }
        let manifest:Value=sqlx::query_scalar("SELECT provenance FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND input_key=$4 AND plan_sha=$5 AND coverage=provenance->>'manifest_coverage'")
            .bind(&self.binding.provider).bind(&self.binding.channel).bind(key["episode"].as_str().ok_or("debt episode")?).bind(key["input_key"].as_str().ok_or("debt input")?).bind(key["plan_sha"].as_str().ok_or("debt plan")?).fetch_one(&self.scope.pool).await.map_err(|e|e.to_string())?;
        let mut aliases = Vec::new();
        for alias in manifest["native"]
            .as_array()
            .ok_or("debt native coverage")?
        {
            let origin: super::super::Artifact =
                serde_json::from_value(alias["output_origin"].clone())
                    .map_err(|e| e.to_string())?;
            if !matches!(origin, super::super::Artifact::Original(_)) {
                return Err("coverage lost original provenance".into());
            }
            aliases.push((
                serde_json::from_value(alias["unit_key"].clone()).map_err(|e| e.to_string())?,
                u32::try_from(alias["piece_index"].as_u64().ok_or("native piece")?)
                    .map_err(|e| e.to_string())?,
                alias["payload"].as_str().ok_or("native payload")?.into(),
                origin,
            ));
        }
        Ok(Some(Coverage(aliases)))
    }
    pub(crate) async fn confirm_native(
        &self,
        plan: &impl Registration,
        origin: &super::super::Artifact,
        serial: u64,
        piece: &PieceRecord,
        confirmation: &PostConfirmation,
    ) -> Result<(), String> {
        if !matches!(origin, super::super::Artifact::Original(_))
            || !confirmation.matches(serial, piece, self.binding.bot_id)
        {
            return Err("native coverage lacks exact original Posted proof".into());
        }
        let Some(PieceOutcome::Posted(message)) = piece.outcome else {
            return Err("native body not posted".into());
        };
        let key = plan.lookup();
        if key["provider"] != self.binding.provider || key["channel"] != self.binding.channel {
            return Err("native receipt destination changed".into());
        }
        let mut tx = begin(&self.scope.pool).await.map_err(|e| e.to_string())?;
        crate::db::channel_custody_lock::lock(
            &mut tx,
            &OwnerIdentity::new(&self.binding.provider, &self.binding.channel),
        )
        .await
        .map_err(|e| e.to_string())?;
        let rows:Vec<(Uuid,Value,i32,String,String,Option<Value>)>=sqlx::query_as("SELECT id,unit_key,piece_index,payload_sha,state,receipt FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND input_key=$4 AND plan_sha=$5 AND payload=$6")
            .bind(&self.binding.provider).bind(&self.binding.channel).bind(key["episode"].as_str().ok_or("debt episode")?).bind(key["input_key"].as_str().ok_or("debt input")?).bind(key["plan_sha"].as_str().ok_or("debt plan")?).bind(&piece.payload).fetch_all(&mut *tx).await.map_err(|e|e.to_string())?;
        let [(id, unit, index, payload, state, kept)] = rows.as_slice() else {
            return Err("native fragment coverage is ambiguous".into());
        };
        let manifest:Value=sqlx::query_scalar("SELECT provenance FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND input_key=$4 AND plan_sha=$5 AND coverage=provenance->>'manifest_coverage'")
            .bind(&self.binding.provider).bind(&self.binding.channel).bind(key["episode"].as_str().ok_or("debt episode")?).bind(key["input_key"].as_str().ok_or("debt input")?).bind(key["plan_sha"].as_str().ok_or("debt plan")?).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
        let exact = manifest["native"].as_array().is_some_and(|aliases| {
            aliases.iter().any(|a| {
                a["unit_key"] == serde_json::to_value(&piece.unit_key).unwrap_or(Value::Null)
                    && a["piece_index"] == piece.piece_index
                    && a["payload"] == piece.payload
                    && a["output_origin"] == serde_json::to_value(origin).unwrap_or(Value::Null)
            })
        });
        if !exact || hash(&piece.payload) != *payload {
            return Err("native proof does not own an unpaid original fragment".into());
        }
        let proof = Proof {
            version: 1,
            effect: *id,
            unit: serde_json::from_value(unit.clone()).map_err(|e| e.to_string())?,
            piece: u32::try_from(*index).map_err(|e| e.to_string())?,
            payload_sha: payload.clone(),
            bot: self.binding.bot_id,
            message,
        };
        let proof = serde_json::to_value(proof).map_err(|e| e.to_string())?;
        if state == "confirmed" && kept.as_ref() == Some(&proof) {
            // Reconcile the same exact effect after either local/shared receipt crash cut.
        } else if state == "ready" && kept.is_none() {
            let updated=sqlx::query("UPDATE o_herdr_debt_effects SET state='confirmed',receipt=$2 WHERE id=$1 AND state='ready' AND receipt IS NULL").bind(id).bind(&proof).execute(&mut *tx).await.map_err(|e|e.to_string())?;
            if updated.rows_affected() != 1 {
                return Err("native receipt lost its effect".into());
            }
        } else {
            return Err("native receipt is pending or conflicts with exact coverage".into());
        }
        sqlx::query(
            "UPDATE o_herdr_debt_grants SET state='consumed' WHERE effect_id=$1 AND state IN ('issued','reserved')",
        )
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostedReceipt {
    version: u8,
    effect: Uuid,
    provider: String,
    channel: u64,
    episode: String,
    input_key: String,
    plan_sha: String,
    unit: Unit,
    index: u32,
    payload_sha: String,
    bot: u64,
    message: u64,
}
impl PostedReceipt {
    pub(crate) fn effect(&self) -> Uuid {
        self.effect
    }
    pub(crate) fn message(&self) -> u64 {
        self.message
    }
    pub(crate) fn valid(&self) -> bool {
        self.version == 1
            && !self.effect.is_nil()
            && self.channel > 0
            && self.bot > 0
            && self.message > 0
            && [
                self.episode.as_str(),
                self.input_key.as_str(),
                self.plan_sha.as_str(),
                self.payload_sha.as_str(),
            ]
            .into_iter()
            .all(digest)
            && matches!(self.provider.as_str(), "claude" | "codex")
            && self.unit.channel_id == self.channel
            && self.unit.provider == self.provider
            && self.unit.kind == "body"
            && self.index <= i32::MAX as u32
            && self.unit.native_key == format!("herdr:{}:saved", self.episode)
    }
    #[cfg(test)]
    pub(crate) fn context(&self, channel: u64, provider: &str) -> bool {
        self.valid() && self.channel == channel && self.provider == provider
    }
}
