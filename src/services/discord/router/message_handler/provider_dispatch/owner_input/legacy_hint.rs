use super::{ChannelId, Front, FrontClaim, Shared};
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    self as db, HostedLookup, HostedLookupKey, HostedOwner, HostedRecord,
};
use crate::services::codex::legacy_hint::{
    self as marker, ACarrier, AStartResult, evidence::AbsenceStamp,
};
use crate::services::provider::ProviderKind;
use crate::services::tmux_common::runtime_hint_fence::RuntimeHintFence;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, MarkerControl, MarkerRowExpectation, MarkerStage, OwnedRecoveryTarget,
    QueueIdentity, QueueOwner, RecoveryGate, RecoveryPass,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::{Arc, atomic::Ordering};

async fn change<T: Send + 'static>(
    mailbox: &ChannelMailboxHandle,
    f: impl FnOnce(&mut QueueOwner) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    mailbox
        .v2_request(move |slot| f(slot.as_mut().ok_or("input owner missing")?))
        .await
}
fn identity(front: &Front) -> QueueIdentity {
    let item = crate::services::turn_orchestrator::pending_queue_item_to_intervention(
        front.1.clone(),
        std::time::SystemTime::now(),
        std::time::Instant::now(),
    );
    QueueIdentity::of(&item)
}
fn epoch(owner: &HostedOwner) -> Option<String> {
    let channel = owner.channel_id.parse().ok()?;
    let endpoint = crate::config::session_hosts::herdr_endpoint(channel)?;
    (crate::config::session_hosts::local_node().as_deref()
        == Some(endpoint.execution_node.as_str())
        && crate::config::runtime_root()?.display().to_string() == owner.runtime_root)
        .then(|| format!("{}:{endpoint:?}", owner.owner_node))
}
async fn owner(
    shared: &Shared,
    channel: ChannelId,
    context: Option<&Value>,
) -> Result<Option<HostedOwner>, String> {
    if shared.provider != ProviderKind::Codex
        || !crate::services::turn_host::herdr_turn_switched_on_for(&shared.provider)
    {
        return Ok(None);
    }
    let name = match context {
        Some(context) => {
            match super::super::super::headless_turn::owner_start_pane(shared, channel, context)
                .await
                .map_err(|_| "marker pane is unnamed")?
            {
                Some((name, true)) => Some(name),
                Some((_, false)) => return Ok(None),
                None => None,
            }
        }
        None => None,
    };
    let named = shared
        .core
        .lock()
        .await
        .sessions
        .get(&channel)
        .and_then(|session| session.channel_name.clone())
        .or_else(|| {
            crate::services::discord::adk_session::registered_channel_fallback_name(
                channel,
                &shared.provider,
            )
        });
    let Some(logical_key) =
        name.or_else(|| named.map(|name| shared.provider.build_tmux_session_name(&name)))
    else {
        return Ok(None);
    };
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: shared.token_hash.clone(),
        channel_id: channel.to_string(),
        logical_key,
        owner_node: crate::config::session_hosts::local_node().ok_or("marker node missing")?,
        runtime_root: crate::config::runtime_root()
            .ok_or("marker root missing")?
            .display()
            .to_string(),
    };
    Ok(epoch(&owner).map(|_| owner))
}
async fn observe(
    pool: &sqlx::PgPool,
    owner: &HostedOwner,
    expected: Option<&MarkerRowExpectation>,
) -> Result<MarkerRowExpectation, String> {
    let key = crate::services::discord::adk_session::build_namespaced_session_key(
        &owner.discord_token_hash,
        &ProviderKind::Codex,
        &owner.logical_key,
    );
    let exact = db::load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await;
    let canonical = db::load_hosted_execution_pg(
        pool,
        HostedLookupKey::Canonical {
            provider: &owner.provider,
            identity: CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: &owner.discord_token_hash,
                channel_id: &owner.channel_id,
            },
        },
    )
    .await;
    if exact != canonical {
        return Err("marker exact/canonical differ".into());
    }
    let candidates = db::load_marker_owner_candidates_pg(pool, &owner.logical_key).await?;
    let current = match exact {
        HostedLookup::Missing if candidates.is_empty() => MarkerRowExpectation::Missing {
            owner: owner.into(),
            reserved_row_id: match expected {
                Some(MarkerRowExpectation::Missing {
                    reserved_row_id, ..
                }) => *reserved_row_id,
                _ => None,
            },
        },
        HostedLookup::Found(row)
            if row.record == HostedRecord::Legacy
                && candidates.len() == 1
                && candidates[0].session_id == row.session_id()
                && candidates[0].raw.is_none()
                && candidates[0].identity
                    == [
                        Some(owner.provider.clone()),
                        Some("discord_channel".into()),
                        Some(owner.discord_token_hash.clone()),
                        Some(owner.channel_id.clone()),
                    ]
                && (candidates[0].session_key.as_deref() == Some(key.as_str())
                    || candidates[0].aliases.contains(&key)) =>
        {
            MarkerRowExpectation::Legacy {
                owner: owner.into(),
                row_id: row.session_id(),
                exact_session_key: key,
            }
        }
        _ => return Err("marker owner is not uniquely absent Legacy".into()),
    };
    if expected.is_some_and(|old| old != &current) {
        return Err("marker row changed".into());
    }
    Ok(current)
}

pub(super) struct Prepared {
    control: MarkerControl,
    stamp: AbsenceStamp,
}
impl Prepared {
    pub(super) async fn carrier(
        &self,
        mailbox: &ChannelMailboxHandle,
        claim: &FrontClaim,
    ) -> Result<ACarrier, String> {
        let (front, number) = (claim.front().clone(), claim.number());
        let live = change(mailbox, move |owner| owner.a_claim_liveness(&front, number)).await?;
        let (owner, expected) = (self.stamp.owner.clone(), self.stamp.epoch.clone());
        Ok(ACarrier::new(
            self.stamp.clone(),
            Arc::new(move || {
                live.load(Ordering::SeqCst) && epoch(&owner).as_deref() == Some(expected.as_str())
            }),
        ))
    }
    pub(super) async fn wait(
        &self,
        shared: &Shared,
        channel: ChannelId,
        context: &Value,
    ) -> super::Wait {
        match super::start_target(shared, channel, context).await {
            Ok(Some(target)) if target.tmux == self.stamp.owner.logical_key => {
                super::native_wait(&shared.provider, target)
            }
            _ => super::Wait::Event,
        }
    }
}

pub(super) async fn prepare(
    shared: &Shared,
    channel: ChannelId,
    front: &Front,
    context: Option<&Value>,
) -> Result<Option<Prepared>, String> {
    let Some(owner) = owner(shared, channel, context).await? else {
        return Ok(None);
    };
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or("marker database unavailable")?;
    let mailbox = shared.mailbox(channel);
    let current_epoch = epoch(&owner).ok_or("marker mapping changed")?;
    let namespace = format!("{}/{}", owner.provider, owner.discord_token_hash);
    let cleanup_id = hex::encode(Sha256::digest(
        serde_json::to_vec(&(&namespace, identity(front), &current_epoch))
            .map_err(|e| e.to_string())?,
    ));
    let mut failures = 0;
    loop {
        let step = advance(shared, channel, front, &owner, &current_epoch, &cleanup_id).await;
        match step {
            Ok(ready) => return Ok(ready),
            Err(error) => {
                failures += 1;
                if failures < 3 {
                    continue;
                }
                let (original, id, detail) = (front.clone(), cleanup_id.clone(), error.clone());
                let retained = change(&mailbox, move |owner| {
                    owner.a_refuse(&original, &id, &detail)
                })
                .await;
                if retained.is_ok() {
                    notify(shared, channel).await?;
                } else {
                    super::hold_start(&mailbox, channel, front).await;
                    super::storage_failure::cleanup(
                        Some(pool),
                        ("codex", channel.get(), &namespace),
                        (&cleanup_id, "storage_hold"),
                    )
                    .await?;
                }
                return Err(format!("NotSent(\"marker_admission_held: {error}\")"));
            }
        }
    }
}
async fn advance(
    shared: &Shared,
    channel: ChannelId,
    front: &Front,
    owner: &HostedOwner,
    current_epoch: &str,
    id: &str,
) -> Result<Option<Prepared>, String> {
    let mailbox = shared.mailbox(channel);
    let original = front.clone();
    let mut control = change(&mailbox, move |owner| {
        owner.recover()?;
        Ok(owner.a_control_for(&original))
    })
    .await?;
    let reading = crate::services::discord::turn_presence::activity::reading_now(
        shared,
        &shared.provider,
        channel,
    )
    .await;
    let fence = RuntimeHintFence::acquire_async(&owner.logical_key).await?;
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or("marker database unavailable")?;
    if control.is_none() {
        let mut expected = observe(pool, owner, None).await?;
        let initial = AbsenceStamp::capture(
            owner,
            expected.clone(),
            identity(front),
            current_epoch.into(),
        )?;
        let eligible = match &expected {
            MarkerRowExpectation::Legacy { .. } => {
                reading.marker_only_legacy(&initial, &initial).is_some()
            }
            MarkerRowExpectation::Missing { .. } => {
                reading.observed.activity
                    == crate::services::discord::turn_presence::activity::Activity::Idle
            }
        };
        if !eligible || reading.publish_if_current(|| ()).await.is_none() {
            return Ok(None);
        }
        if let MarkerRowExpectation::Missing {
            reserved_row_id, ..
        } = &mut expected
        {
            *reserved_row_id = Some(db::reserve_marker_row_id_pg(pool).await?);
        }
        let stamp = AbsenceStamp::capture(
            owner,
            expected.clone(),
            identity(front),
            current_epoch.into(),
        )?;
        let original = front.clone();
        let receipt = change(&mailbox, move |owner| owner.a_front_receipt(&original)).await?;
        let candidate = MarkerControl {
            cleanup_id: id.into(),
            identity: identity(front),
            front_receipt: receipt,
            epoch: current_epoch.into(),
            expected,
            paths: stamp.path_snapshots(),
            witness: stamp.witness()?,
            stage: MarkerStage::Intent,
            attempt: None,
        };
        let (original, epoch) = (front.clone(), current_epoch.to_string());
        control = Some(
            change(&mailbox, move |owner| {
                owner.a_intent(&original, candidate, &epoch)
            })
            .await?,
        );
    }
    let mut control = control.ok_or("marker intent missing")?;
    if control.epoch != current_epoch || epoch(owner).as_deref() != Some(current_epoch) {
        return Err("marker epoch changed".into());
    }
    if control.attempt.is_some() {
        return Err("marker execution already has custody".into());
    }
    if control.stage == MarkerStage::Intent {
        observe(pool, owner, Some(&control.expected)).await?;
        marker::cleanup(&AbsenceStamp::from_control(&control)?, &fence)?;
        let id = control.cleanup_id.clone();
        change(&mailbox, move |owner| {
            owner.a_stage(&id, &MarkerStage::Intent, MarkerStage::Receipt)
        })
        .await?;
        control.stage = MarkerStage::Receipt;
    }
    if let MarkerRowExpectation::Missing {
        reserved_row_id: Some(reserved),
        ..
    } = &control.expected
    {
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &owner.discord_token_hash,
            &ProviderKind::Codex,
            &owner.logical_key,
        );
        db::prepare_marker_missing_pg(pool, owner, &key, *reserved)
            .await?
            .ok_or("reserved marker row conflicted")?;
        let (id, reserved) = (control.cleanup_id.clone(), *reserved);
        control = change(&mailbox, move |owner| {
            owner.a_bind_created_row(&id, reserved, key)
        })
        .await?;
    }
    observe(pool, owner, Some(&control.expected)).await?;
    let current = AbsenceStamp::capture(
        owner,
        control.expected.clone(),
        control.identity.clone(),
        control.epoch.clone(),
    )?;
    if !AbsenceStamp::from_control(&control)?.permits_partial(&current)
        || current.paths.iter().any(|path| path.file.is_some())
    {
        return Err("marker receipt changed".into());
    }
    if control.stage == MarkerStage::Receipt {
        let (id, next) = (
            control.cleanup_id.clone(),
            MarkerStage::Outbox {
                event_id: format!(
                    "{}/{}/{}",
                    owner.discord_token_hash, control.cleanup_id, control.epoch
                ),
            },
        );
        let stage = next.clone();
        change(&mailbox, move |owner| {
            owner.a_stage(&id, &MarkerStage::Receipt, next)
        })
        .await?;
        control.stage = stage;
    }
    drop(fence);
    Ok(Some(Prepared {
        control,
        stamp: current,
    }))
}
async fn notify(shared: &Shared, channel: ChannelId) -> Result<(), String> {
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or("marker notice database unavailable")?;
    crate::services::discord::queue_io::input_owner::notice::drain_confirmed(
        pool,
        &shared.mailbox(channel),
        shared.provider.as_str(),
        channel.get(),
    )
    .await
    .map(drop)
}
pub(super) async fn finish(
    shared: &Shared,
    channel: ChannelId,
    front: &Front,
    prepared: &Prepared,
    carrier: &ACarrier,
) {
    let result = carrier.take_result();
    let validated = matches!(result, Some(AStartResult::Validated));
    let (unproved, refusal) = match result {
        Some(AStartResult::Refused {
            stage,
            observed_identity,
            detail,
        }) => {
            tracing::warn!(%stage, ?observed_identity, %detail, "marker admission refused");
            (false, Some(detail))
        }
        Some(AStartResult::Validated) => (false, None),
        _ => (true, Some("executor outcome is unproved".into())),
    };
    let mailbox = shared.mailbox(channel);
    for _ in 0..3 {
        let (id, original, refusal) = (
            prepared.control.cleanup_id.clone(),
            front.clone(),
            refusal.clone(),
        );
        let settled = change(&mailbox, move |owner| {
            if let Some(reason) = refusal {
                return if unproved {
                    owner.a_unproved(&original, &id, &reason)
                } else {
                    owner.a_refuse(&original, &id, &reason)
                };
            }
            let state = owner.json()?;
            if state["head_injection"]["identity"]
                == serde_json::to_value(&identity(&original)).map_err(|e| e.to_string())?
                && !state["head_injection"]["attempt"].is_null()
            {
                let attempt = serde_json::from_value(state["head_injection"]["attempt"].clone())
                    .map_err(|e| e.to_string())?;
                owner.a_attempt(&id, attempt)?;
            }
            owner.a_ack_if_settled(&id)
        })
        .await;
        if settled.is_ok() && (validated || notify(shared, channel).await.is_ok()) {
            return;
        }
    }
    tracing::warn!(channel_id=channel.get(), cleanup_id=%prepared.control.cleanup_id, "marker settlement remains in custody");
}

pub(super) async fn recover(
    shared: &Shared,
    target: OwnedRecoveryTarget,
    pass: &RecoveryPass,
) -> RecoveryGate {
    let channel = match &target {
        OwnedRecoveryTarget::Ready(channel) => *channel,
        OwnedRecoveryTarget::Failed(failure) => failure.channel,
    };
    let mailbox = shared.mailbox(channel);
    let persistence = crate::services::discord::queue_dispatch::persistence_context(
        shared,
        &shared.provider,
        channel,
    );
    let queue = crate::services::turn_orchestrator::v2_queue(&persistence, channel);
    for attempt in 0..3 {
        let restored = async {
            if let OwnedRecoveryTarget::Failed(_) = &target {
                if owner(shared, channel, None).await?.is_none() {
                    return Err("catalog candidate is not an eligible local owner".into());
                }
            }
            let (path, namespace) = queue.clone()?;
            mailbox.v2_ensure(path, namespace).await?;
            let (controls, state) = change(&mailbox, |owner| {
                owner.recover()?;
                Ok((owner.a_controls(), owner.json()?))
            })
            .await?;
            for control in controls {
                let id = control.cleanup_id.clone();
                if change(&mailbox, move |owner| owner.a_ack_if_settled(&id)).await? {
                    continue;
                }
                if state["head_injection"]["identity"]
                    == serde_json::to_value(&control.identity).map_err(|e| e.to_string())?
                {
                    continue;
                }
                let front = change(&mailbox, |owner| Ok(owner.front()))
                    .await?
                    .ok_or("control debt lacks an original or result")?;
                if identity(&front) != control.identity {
                    return Err("marker debt original differs".into());
                }
                let context = match front.0.clone() {
                    Some(ticket) => {
                        mailbox
                            .v2_admission_context(front.1.clone(), ticket)
                            .await?
                    }
                    None => None,
                };
                prepare(shared, channel, &front, context.as_ref())
                    .await?
                    .ok_or("marker debt is not eligible")?;
            }
            let history =
                crate::services::tui_input::human_frame::history::v2::Registry::runtime()?;
            let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
            crate::services::discord::queue_io::input_owner::recovery::recover(
                &mailbox,
                queue.clone()?,
                history,
                shared.pg_pool.as_ref(),
                (shared.provider.as_str(), channel.get(), now),
            )
            .await?;
            Ok::<_, String>(())
        }
        .await;
        match restored {
            Ok(()) => {
                super::wake(shared, channel);
                return RecoveryGate::Complete;
            }
            Err(error) => {
                if attempt < 2 {
                    continue;
                }
                tracing::warn!(channel_id=channel.get(), %error, "marker recovery held");
                if let OwnedRecoveryTarget::Failed(failure) = &target {
                    let _notice =
                        super::storage_failure::catalog(shared.pg_pool.as_ref(), pass, failure)
                            .await;
                }
            }
        }
    }
    RecoveryGate::Held
}
