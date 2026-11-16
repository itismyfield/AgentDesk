//! One shared effect/attempt survives every owner and writer incarnation.
use super::*;
use serde_json::json;
pub(super) async fn structural(
    tx: &mut Transaction<'_, Postgres>,
    r: &Reference,
) -> Result<(), AcquireError> {
    let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM o_herdr_debt_grants g JOIN o_herdr_debt_effects e ON e.id=g.effect_id WHERE g.id=$1 AND g.claim_core=$2 AND g.episode=$3 AND e.id=$4 AND e.episode=g.episode AND e.provider=$5 AND e.channel_id=$6 AND e.input_key=$7 AND e.coverage=$8 AND e.plan_sha=$9 AND e.snapshots ? $10 AND e.payload_sha=$11 AND e.unit_key=$12 AND e.piece_index=$13 AND e.bot_id=$14)")
        .bind(r.grant_id).bind(r.core()?).bind(&r.episode).bind(r.effect_id).bind(&r.provider).bind(&r.channel).bind(&r.input_key).bind(&r.coverage).bind(&r.plan_sha).bind(&r.snapshot_sha).bind(&r.payload_sha).bind(serde_json::to_value(&r.unit_key).map_err(|_|denied("debt unit encoding"))?).bind(i64::from(r.piece_index)).bind(i64::try_from(r.authority.bot_id).map_err(|_|denied("debt bot id out of range"))?).fetch_one(&mut **tx).await?;
    if !valid {
        return Err(denied("debt grant or original identity changed"));
    }
    Ok(())
}
pub(super) async fn check(
    tx: &mut Transaction<'_, Postgres>,
    r: &Reference,
    grant_phase: &str,
    effect_phase: &str,
) -> Result<(), AcquireError> {
    structural(tx, r).await?;
    let binding =
        serde_json::to_value(&r.authority).map_err(|_| denied("debt authority encoding"))?;
    let current:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM o_herdr_debt_runs s WHERE s.provider=$1 AND s.channel_id=$2 AND s.binding=$3 AND s.active AND ((s.binding->>'domain'='execution' AND EXISTS(SELECT 1 FROM intake_session_owners o WHERE o.provider=s.provider AND o.raw_channel_id=s.channel_id AND o.owner_instance_id=s.binding->>'custodian' AND o.generation=(s.binding->>'revision')::bigint AND o.status='active' AND o.routing_cleanup IS NULL)) OR (s.binding->>'domain'='gateway' AND NOT EXISTS(SELECT 1 FROM intake_session_owners o WHERE o.provider=s.provider AND o.raw_channel_id=s.channel_id AND o.status='active') AND NOT EXISTS(SELECT 1 FROM intake_outbox d WHERE d.provider=s.provider AND d.channel_id=s.channel_id AND d.routing_receipt_id IS NOT NULL))))")
        .bind(&r.provider).bind(&r.channel).bind(binding).fetch_one(&mut **tx).await?;
    if !current {
        return Err(denied("debt custodian or writer run changed"));
    }
    let (grant,state,proof):(String,String,Option<Value>)=sqlx::query_as("SELECT g.state,e.state,e.receipt FROM o_herdr_debt_grants g JOIN o_herdr_debt_effects e ON e.id=g.effect_id WHERE g.id=$1")
        .bind(r.grant_id).fetch_one(&mut **tx).await?;
    if grant != grant_phase {
        return Err(denied("debt capability is not issued for this phase"));
    }
    receipt::admit(&state, effect_phase, proof.as_ref(), r)?;
    Ok(())
}
pub(super) async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    r: &Reference,
) -> Result<Uuid, AcquireError> {
    check(tx, r, "issued", "ready").await?;
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO o_herdr_debt_attempts(id,effect_id,grant_id,payload_sha,phase) VALUES($1,$2,$3,$4,'reserved')")
        .bind(attempt).bind(r.effect_id).bind(r.grant_id).bind(&r.payload_sha).execute(&mut **tx).await?;
    sqlx::query("UPDATE o_herdr_debt_effects SET state='reserved',attempt_id=$2 WHERE id=$1")
        .bind(r.effect_id)
        .bind(attempt)
        .execute(&mut **tx)
        .await?;
    sqlx::query("UPDATE o_herdr_debt_grants SET state='reserved' WHERE id=$1")
        .bind(r.grant_id)
        .execute(&mut **tx)
        .await?;
    Ok(attempt)
}

impl Scope {
    /// Registers immutable coverage only; issuance still requires a live custodian and store.
    pub(crate) async fn register_herdr_body_debt(
        &self,
        plan: &impl Registration,
        bot: u64,
    ) -> Result<usize, String> {
        let document = plan.registration(bot)?;
        if document["provider"] != self.provider {
            return Err("body debt provider mismatch".into());
        }
        let channel = document["channel"].as_str().ok_or("body debt channel")?;
        let episode = document["episode"].as_str().ok_or("body debt episode")?;
        let input = document["input_key"].as_str().ok_or("body debt input")?;
        let plan_sha = document["plan_sha"].as_str().ok_or("body debt plan")?;
        let bot = i64::try_from(bot).map_err(|e| e.to_string())?;
        let mut tx = begin(&self.pool).await.map_err(|e| e.to_string())?;
        let id = OwnerIdentity::new(&self.provider, channel);
        crate::db::channel_custody_lock::lock(&mut tx, &id)
            .await
            .map_err(|e| e.to_string())?;
        let incompatible: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND (input_key<>$4 OR plan_sha<>$5 OR bot_id<>$6 OR provenance->>'kind' IS DISTINCT FROM 'c1_accepted_snapshot'))")
            .bind(&self.provider).bind(channel).bind(episode).bind(input).bind(plan_sha).bind(bot)
            .fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
        if incompatible {
            return Err("body debt has a conflicting shared plan".into());
        }
        for alias in document["native"]
            .as_array()
            .ok_or("body debt native coverage")?
        {
            let claimed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM o_herdr_debt_effects e CROSS JOIN LATERAL jsonb_array_elements(e.provenance->'native') a WHERE e.provider=$1 AND e.channel_id=$2 AND e.episode<>$3 AND a->'unit_key'=$4 AND a->'piece_index'=$5)")
                .bind(&self.provider).bind(channel).bind(episode).bind(&alias["unit_key"]).bind(&alias["piece_index"])
                .fetch_one(&mut *tx).await.map_err(|e| e.to_string())?;
            if claimed {
                return Err("native coverage belongs to another original input".into());
            }
        }
        let manifest = document["effects"][0]["coverage"]
            .as_str()
            .ok_or("body debt manifest coverage")?;
        let mut inserted = 0;
        for piece in document["effects"].as_array().ok_or("body debt effects")? {
            let coverage = piece["coverage"].as_str().ok_or("body debt coverage")?;
            let payload = piece["payload"].as_str().ok_or("body debt payload")?;
            let payload_sha = piece["payload_sha"]
                .as_str()
                .ok_or("body debt payload hash")?;
            let index = i32::try_from(piece["piece_index"].as_u64().ok_or("body debt piece")?)
                .map_err(|e| e.to_string())?;
            let snapshots = document["snapshots"]
                .as_object()
                .ok_or("body debt snapshots")?;
            let digests = json!(snapshots.keys().collect::<Vec<_>>());
            // One raw manifest per plan, rather than one full body copy per split piece.
            let provenance = if index == 0 {
                json!({"kind":"c1_accepted_snapshot", "version":1, "manifest_coverage":manifest,
                    "snapshots":document["snapshots"], "native":document["native"]})
            } else {
                json!({"kind":"c1_accepted_snapshot", "version":1, "manifest_coverage":manifest})
            };
            let result = sqlx::query("INSERT INTO o_herdr_debt_effects(id,provider,channel_id,episode,input_key,coverage,plan_sha,snapshots,provenance,payload,payload_sha,unit_key,piece_index,bot_id,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'ready') ON CONFLICT(provider,channel_id,episode,input_key,coverage) DO NOTHING")
                .bind(Uuid::new_v4()).bind(&self.provider).bind(channel).bind(episode).bind(input).bind(coverage).bind(plan_sha).bind(&digests)
                .bind(&provenance).bind(payload).bind(payload_sha).bind(&piece["unit_key"]).bind(index).bind(bot)
                .execute(&mut *tx).await.map_err(|e|e.to_string())?;
            inserted += result.rows_affected() as usize;
            let (effect, old_payload, old_hash, old_unit, old_index, old_provenance, old_snapshots): (Uuid,String,String,Value,i32,Value,Value) = sqlx::query_as("SELECT id,payload,payload_sha,unit_key,piece_index,provenance,snapshots FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND input_key=$4 AND coverage=$5")
                .bind(&self.provider).bind(channel).bind(episode).bind(input).bind(coverage).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
            if old_payload != payload
                || old_hash != payload_sha
                || old_unit != piece["unit_key"]
                || old_index != index
            {
                return Err("body debt coverage cannot change its payload or identity".into());
            }
            if old_provenance["version"] != 1 || old_provenance["manifest_coverage"] != manifest {
                return Err("body debt manifest identity changed".into());
            }
            if index != 0 {
                if old_provenance != provenance {
                    return Err("unreadable body debt manifest reference".into());
                }
                let mut hashes = std::collections::BTreeSet::new();
                for value in old_snapshots.as_array().ok_or("original snapshot hashes")? {
                    hashes.insert(value.as_str().ok_or("original snapshot hash")?.to_owned());
                }
                hashes.extend(snapshots.keys().cloned());
                sqlx::query("UPDATE o_herdr_debt_effects SET snapshots=$2 WHERE id=$1")
                    .bind(effect)
                    .bind(json!(hashes))
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| e.to_string())?;
                continue;
            }
            let mut merged = old_provenance;
            let kept = merged["snapshots"]
                .as_object_mut()
                .ok_or("unreadable original snapshots")?;
            for (digest, snapshot) in snapshots {
                if kept.get(digest).is_some_and(|old| old != snapshot) {
                    return Err("immutable snapshot changed".into());
                }
                kept.insert(digest.clone(), snapshot.clone());
            }
            let mut original_ids = Vec::new();
            for snapshot in merged["snapshots"]
                .as_object()
                .ok_or("original snapshots")?
                .values()
            {
                let row: Value = serde_json::from_str(
                    snapshot["row_json"].as_str().ok_or("original row bytes")?,
                )
                .map_err(|e| e.to_string())?;
                if let Some(id) = row.get("intake_outbox_id").filter(|id| !id.is_null()) {
                    original_ids.push(id.clone());
                }
            }
            if original_ids.windows(2).any(|ids| ids[0] != ids[1]) {
                return Err("contradictory original intake evidence".into());
            }
            let aliases = merged["native"]
                .as_array_mut()
                .ok_or("unreadable original native coverage")?;
            for alias in document["native"].as_array().ok_or("native aliases")? {
                if !aliases.contains(alias) {
                    aliases.push(alias.clone());
                }
            }
            let digests = json!(
                merged["snapshots"]
                    .as_object()
                    .ok_or("original snapshots")?
                    .keys()
                    .collect::<Vec<_>>()
            );
            sqlx::query("UPDATE o_herdr_debt_effects SET snapshots=$2,provenance=$3 WHERE id=$1")
                .bind(effect)
                .bind(digests)
                .bind(merged)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(inserted)
    }
}
