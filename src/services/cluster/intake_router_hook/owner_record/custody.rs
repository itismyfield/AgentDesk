//! Shared custody is acknowledged only after the full regular delivery commits.
use super::*;
use crate::db::replay_disposition::write;
use crate::services::cluster::attachment_transfer::{store, uploads::BundleRef};

fn invalid(message: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::Protocol(message.to_string())
}

fn canonical(
    p: &InsertPendingPayload,
    id: &OwnerIdentity,
    sources: Vec<String>,
) -> write::CanonicalInput {
    write::CanonicalInput {
        request_key: format!(
            "discord:{}:{}:{}",
            id.provider(),
            id.raw_channel_id(),
            p.user_msg_id
        ),
        provider: id.provider().to_owned(),
        channel: id.raw_channel_id().to_owned(),
        sources,
        original_text: p.user_text.clone(),
        owner_id: p.request_owner_id.clone(),
        agent_id: p.agent_id.clone(),
        attachments: p.attachment_refs.clone(),
        reply_context: p.reply_context.clone(),
        provenance: "new_input",
    }
}

pub(crate) struct StageRequest<'a> {
    pub payload: &'a InsertPendingPayload,
    pub home_revision: i64,
    pub wait_generation: Option<i64>,
    pub grace_seconds: i32,
}

pub(crate) async fn stage(pool: &PgPool, request: StageRequest<'_>) -> Result<i64, sqlx::Error> {
    let p = request.payload;
    let id = OwnerIdentity::new(&p.provider, &p.channel_id);
    if request.grace_seconds < 0 {
        return Err(invalid("negative grace"));
    }
    let refs: Vec<BundleRef> =
        serde_json::from_value(p.attachment_refs.clone()).map_err(invalid)?;
    store::validate_refs(pool, &refs, id.provider(), id.raw_channel_id())
        .await
        .map_err(invalid)?;
    let mut sources = p.replay_source_message_ids.clone().unwrap_or_default();
    sources.push(p.user_msg_id.clone());
    sources.sort_unstable();
    sources.dedup();
    if refs
        .iter()
        .any(|r| !sources.contains(&r.identity.user_msg_id))
    {
        return Err(invalid("bundle is not an input source"));
    }
    let canonical = canonical(p, &id, sources);
    let receipt = write::register_or_reuse(pool, &canonical, &p.forwarded_by_instance_id)
        .await
        .map_err(invalid)?;
    let mut tx = pool.begin().await?;
    lock(&mut tx, &id).await?;
    if let Some(row) =
        sqlx::query_scalar::<_, i64>("SELECT id FROM intake_outbox WHERE routing_receipt_id=$1")
            .bind(receipt.id)
            .fetch_optional(&mut *tx)
            .await?
    {
        tx.commit().await?;
        return Ok(row);
    }
    if receipt.disposition
        != crate::db::replay_disposition::receipt::Disposition::RegisteredNotStarted
    {
        return Err(invalid("source is already protected"));
    }
    let revision: i64 = sqlx::query_scalar("SELECT COALESCE((SELECT revision FROM channel_home_preferences WHERE provider=$1 AND raw_channel_id=$2),0)")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_one(&mut *tx).await?;
    let owner = read_latest_owner_in_tx(&mut tx, &id).await?;
    if revision != request.home_revision
        || owner.as_ref().map(|o| o.generation) != request.wait_generation
    {
        return Err(invalid("home or owner changed before staging"));
    }
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtext('agentdesk.attachment-bundles.v1')::BIGINT)",
    )
    .execute(&mut *tx)
    .await?;
    for reference in &refs {
        let present: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_attachment_bundles WHERE id=$1 AND provider=$2 AND channel_id=$3 AND user_msg_id=$4 AND (expires_at>NOW() OR intake_routing_bundle_pinned(id)))")
            .bind(reference.bundle_id).bind(id.provider()).bind(id.raw_channel_id()).bind(&reference.identity.user_msg_id).fetch_one(&mut *tx).await?;
        if !present {
            return Err(invalid("bundle expired before custody commit"));
        }
    }
    let key = format!("routing:{}", receipt.id);
    let AdmissionOutcome::Inserted { outbox_id } = insert_admission_savepoint(
        &mut tx,
        p,
        AdmissionKind::Held,
        "",
        0,
        &p.target_instance_id,
        &key,
        1,
    )
    .await?
    else {
        return Err(invalid("input already has another admission"));
    };
    sqlx::query("UPDATE intake_outbox SET routing_receipt_id=$2,routing_state='held',routing_home_revision=$3,routing_wait_owner_generation=$4,routing_not_before=NOW()+make_interval(secs=>$5::INT::DOUBLE PRECISION) WHERE id=$1")
        .bind(outbox_id).bind(receipt.id).bind(revision).bind(request.wait_generation).bind(request.grace_seconds).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(outbox_id)
}

pub(crate) async fn lock(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(id.advisory_key())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchReadiness {
    Wait,
    NormalEnd,
    Unavailable,
}

/// Readiness comes from the control plane; unavailable requires the stored deadline and old stamp.
pub(crate) async fn promote(
    pool: &PgPool,
    delivery: i64,
    target: &str,
    expected: Option<(&str, i64)>,
    readiness: SwitchReadiness,
) -> Result<bool, sqlx::Error> {
    crate::services::cluster::agent_execution_node::AgentExecutionNode {
        default_node_id: Some(target.to_owned()),
    }
    .validate()
    .map_err(invalid)?;
    let row = sqlx::query_as::<_, IntakeOutboxRow>(
        "SELECT * FROM intake_outbox WHERE id=$1 AND routing_state='held'",
    )
    .bind(delivery)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(false) };
    let id = OwnerIdentity::new(&row.provider, &row.channel_id);
    let mut tx = pool.begin().await?;
    lock(&mut tx, &id).await?;
    let ready: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox d WHERE d.id=$1 AND d.routing_state='held' AND EXISTS(SELECT 1 FROM intake_outbox receipt WHERE receipt.id=d.routing_receipt_id AND receipt.replay_disposition='registered_not_started') AND (d.routing_not_before<=NOW() OR $3::BOOLEAN) AND d.routing_wait_owner_generation IS NOT DISTINCT FROM $2::BIGINT AND d.routing_home_revision=COALESCE((SELECT revision FROM channel_home_preferences WHERE provider=d.provider AND raw_channel_id=d.channel_id),0) AND NOT EXISTS(SELECT 1 FROM intake_outbox h WHERE h.provider=d.provider AND h.channel_id=d.channel_id AND h.routing_state='held' AND h.id<d.id))")
        .bind(delivery).bind(expected.map(|(_, generation)| generation)).bind(readiness == SwitchReadiness::NormalEnd).fetch_one(&mut *tx).await?;
    if !ready {
        return Ok(false);
    }
    let owner = read_latest_owner_in_tx(&mut tx, &id).await?;
    if owner
        .as_ref()
        .map(|o| (o.owner_instance_id.as_str(), o.generation))
        != expected
    {
        return Ok(false);
    }
    if let Some((_, generation)) = expected
        && readiness != SwitchReadiness::Wait
        && !super::cleanup::allows_on_tx(&mut tx, &id, generation, delivery, target, readiness)
            .await?
    {
        return Ok(false);
    }
    let closing: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3 AND routing_cleanup IS NOT NULL)")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(expected.map(|(_,g)|g)).fetch_one(&mut *tx).await?;
    #[cfg(test)]
    let closing = closing && !super::cleanup::tests::mutant("wait-control");
    if readiness == SwitchReadiness::Wait && closing {
        return Ok(false);
    }
    let prior_generation = owner.as_ref().map(|owner| owner.generation);
    if readiness != SwitchReadiness::Wait && owner.is_some() {
        retire_open_on_tx(&mut tx, &id, target).await?;
    }
    let generation = match owner {
        Some(old)
            if old.status == "active"
                && (old.owner_instance_id != target || readiness != SwitchReadiness::Wait) =>
        {
            if readiness == SwitchReadiness::Wait {
                return Ok(false);
            }
            let TransferOutcome::Transferred { new_generation } =
                transfer_owner_in_tx(&mut tx, &id, &old.owner_instance_id, old.generation, target)
                    .await?
            else {
                return Ok(false);
            };
            new_generation
        }
        Some(old) if old.status == "active" && old.owner_instance_id == target => old.generation,
        _ => match acquire_owner_in_tx(&mut tx, &id, target, |_| true).await? {
            AcquireOutcome::AcquiredLocal { generation } => generation,
            _ => return Ok(false),
        },
    };
    if let Some(prior) = prior_generation
        && readiness == SwitchReadiness::Unavailable
    {
        sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deferred=TRUE WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3")
            .bind(id.provider()).bind(id.raw_channel_id()).bind(prior).execute(&mut *tx).await?;
    }
    let rebase = "UPDATE intake_outbox SET routing_wait_owner_generation=$3 WHERE provider=$1 AND channel_id=$2 AND routing_state='held'";
    #[cfg(test)]
    let rebase_storage = if super::cleanup::tests::mutant("rebase-clear-attachments") {
        rebase.replace(
            "SET routing_wait_owner_generation=$3",
            "SET routing_wait_owner_generation=$3,attachment_refs='[]'",
        )
    } else if super::cleanup::tests::mutant("rebase-clear-reply") {
        rebase.replace(
            "SET routing_wait_owner_generation=$3",
            "SET routing_wait_owner_generation=$3,reply_context=NULL",
        )
    } else {
        rebase.to_owned()
    };
    #[cfg(test)]
    let rebase = sqlx::AssertSqlSafe(rebase_storage);
    sqlx::query(rebase)
        .bind(id.provider())
        .bind(id.raw_channel_id())
        .bind(generation)
        .execute(&mut *tx)
        .await?;
    let head:Option<i64> = sqlx::query_scalar("SELECT id FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_state='held' ORDER BY id LIMIT 1")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_optional(&mut *tx).await?;
    if head != Some(delivery) {
        tx.commit().await?;
        return Ok(false);
    }
    if existing_open_route_in_tx(&mut tx, &row.channel_id)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    let admitted = sqlx::query("UPDATE intake_outbox SET status='pending',routing_state='admitted',owner_instance_id=$2,owner_generation=$3,target_instance_id=$2,admission_kind=CASE WHEN forwarded_by_instance_id=$2 THEN 'local' ELSE 'forwarded' END WHERE id=$1 AND routing_state='held'")
        .bind(delivery).bind(target).bind(generation).execute(&mut *tx).await?;
    if admitted.rows_affected() != 1 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

#[derive(sqlx::FromRow)]
pub(crate) struct StoredDelivery {
    pub id: i64,
    pub routing_state: String,
    pub target_instance_id: String,
    matches_input: bool,
}

/// Read prior custody even while new routing producers are disabled.
pub(crate) async fn lookup_source(
    pool: &PgPool,
    input: &write::CanonicalInput,
) -> Result<Option<StoredDelivery>, sqlx::Error> {
    let stored: Option<StoredDelivery> = sqlx::query_as("SELECT d.id,d.routing_state,d.target_instance_id,(r.replay_source_message_ids=$2 AND r.user_text=$3 AND r.provider=$4 AND r.channel_id=$5) AS matches_input FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE r.replay_request_key=$1")
        .bind(&input.request_key).bind(&input.sources).bind(&input.original_text)
        .bind(&input.provider).bind(&input.channel).fetch_optional(pool).await?;
    if stored.as_ref().is_some_and(|row| !row.matches_input) {
        return Err(invalid("routed input source or body changed"));
    }
    Ok(stored)
}

async fn retire_open_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    target: &str,
) -> Result<(), sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Retiring {
        #[sqlx(flatten)]
        row: IntakeOutboxRow,
        routing_receipt_id: Option<i64>,
        never_started: bool,
        unreceipted: bool,
    }
    let old_rows: Vec<Retiring> = sqlx::query_as("SELECT *, accepted_at IS NULL AND spawned_at IS NULL AS never_started, replay_disposition IS NULL AS unreceipted FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND NOT replay_only AND status IN ('pending','claimed','accepted','spawned') ORDER BY id FOR UPDATE")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_all(&mut **tx).await?;
    for retiring in old_rows {
        let old = retiring.row;
        let unstarted = old.status == IntakeOutboxStatus::Pending
            && old.claim_owner.is_none()
            && retiring.never_started;
        #[cfg(test)]
        let unstarted = unstarted && !super::cleanup::tests::mutant("pending");
        if unstarted {
            let receipt = match retiring.routing_receipt_id {
                Some(receipt) => Some(receipt),
                None if retiring.unreceipted => {
                    let mut sources = old.replay_source_message_ids.clone().unwrap_or_default();
                    sources.push(old.user_msg_id.clone());
                    sources.sort_unstable();
                    sources.dedup();
                    let payload = crate::db::intake_outbox::InsertPendingPayload {
                        target_instance_id: old.target_instance_id.clone(),
                        forwarded_by_instance_id: old.forwarded_by_instance_id.clone(),
                        required_labels: old.required_labels.clone(),
                        execution_requirements: old.execution_requirements.clone(),
                        attachment_refs: old.attachment_refs.clone(),
                        channel_id: old.channel_id.clone(),
                        user_msg_id: old.user_msg_id.clone(),
                        replay_source_message_ids: Some(sources.clone()),
                        request_owner_id: old.request_owner_id.clone(),
                        request_owner_name: old.request_owner_name.clone(),
                        user_text: old.user_text.clone(),
                        reply_context: old.reply_context.clone(),
                        has_reply_boundary: old.has_reply_boundary,
                        dm_hint: old.dm_hint,
                        turn_kind: old.turn_kind.clone(),
                        merge_consecutive: old.merge_consecutive,
                        reply_to_user_message: old.reply_to_user_message,
                        defer_watcher_resume: old.defer_watcher_resume,
                        wait_for_completion: old.wait_for_completion,
                        preserve_on_cancel: old.preserve_on_cancel.unwrap_or(false),
                        agent_id: old.agent_id.clone(),
                        provider: old.provider.clone(),
                    };
                    Some(
                        write::register_or_reuse_on_tx(
                            tx,
                            &canonical(&payload, id, sources),
                            &old.forwarded_by_instance_id,
                        )
                        .await
                        .map_err(invalid)?
                        .id,
                    )
                }
                None => None,
            };
            if let Some(receipt) = receipt {
                let changed=sqlx::query("UPDATE intake_outbox SET status='unknown',routing_state='held',routing_receipt_id=$2,routing_home_revision=COALESCE((SELECT revision FROM channel_home_preferences WHERE provider=$3 AND raw_channel_id=$4),0),routing_not_before=NOW(),owner_instance_id=NULL,owner_generation=NULL,claimed_at=NULL,target_instance_id=$5 WHERE id=$1 AND EXISTS(SELECT 1 FROM intake_outbox r WHERE r.id=$2 AND r.replay_disposition='registered_not_started')")
                    .bind(old.id).bind(receipt).bind(id.provider()).bind(id.raw_channel_id()).bind(target).execute(&mut **tx).await?.rows_affected();
                if changed == 1 {
                    continue;
                }
            }
        }
        if let Some(receipt) = retiring.routing_receipt_id {
            write::withhold_owner_loss_on_tx(tx, receipt).await?;
            sqlx::query(
                "UPDATE intake_outbox SET status='unknown',routing_state='superseded' WHERE id=$1",
            )
            .bind(old.id)
            .execute(&mut **tx)
            .await?;
        } else {
            write::withhold_legacy_owner_loss_on_tx(tx, old.id).await?;
        }
    }
    Ok(())
}

/// Refreshes only unstarted held custody. The caller commits; repeat observations never extend a deadline.
pub(crate) async fn refresh_stage_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    delivery: i64,
    target: &str,
    home_revision: i64,
    expected: Option<(&str, i64)>,
    grace_seconds: i32,
) -> Result<bool, sqlx::Error> {
    if grace_seconds < 0 {
        return Err(invalid("negative grace"));
    }
    crate::services::cluster::agent_execution_node::AgentExecutionNode {
        default_node_id: Some(target.to_owned()),
    }
    .validate()
    .map_err(invalid)?;
    let identity: Option<(String, String)> = sqlx::query_as(
        "SELECT provider,channel_id FROM intake_outbox WHERE id=$1 AND routing_state='held'",
    )
    .bind(delivery)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((provider, channel)) = identity else {
        return Ok(false);
    };
    let id = OwnerIdentity::new(&provider, &channel);
    lock(tx, &id).await?;
    let current_home:i64=sqlx::query_scalar("SELECT COALESCE((SELECT revision FROM channel_home_preferences WHERE provider=$1 AND raw_channel_id=$2),0)").bind(id.provider()).bind(id.raw_channel_id()).fetch_one(&mut **tx).await?;
    if current_home != home_revision {
        return Ok(false);
    }
    let owner = read_latest_owner_in_tx(tx, &id).await?;
    if owner
        .as_ref()
        .map(|o| (o.owner_instance_id.as_str(), o.generation))
        != expected
    {
        return Ok(false);
    }
    let updated=sqlx::query("UPDATE intake_outbox d SET routing_home_revision=$2,target_instance_id=$3,routing_wait_owner_generation=$4,routing_not_before=CASE WHEN routing_wait_owner_generation IS DISTINCT FROM $4::BIGINT THEN NOW()+make_interval(secs=>$5::INT::DOUBLE PRECISION) ELSE routing_not_before END WHERE d.id=$1 AND d.routing_state='held' AND EXISTS(SELECT 1 FROM intake_outbox r WHERE r.id=d.routing_receipt_id AND r.replay_disposition='registered_not_started')")
        .bind(delivery).bind(home_revision).bind(target).bind(expected.map(|(_,g)|g)).bind(grace_seconds).execute(&mut **tx).await?;
    Ok(updated.rows_affected() == 1)
}
