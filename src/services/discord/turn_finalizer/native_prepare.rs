//! Pre-key custody belongs to the finalizer actor; a leaf carries only its acknowledged handle.
#[cfg(test)]
pub(in crate::services::discord) use super::terminal_handler::native_supply::source_binding::BindingRoot;
pub(in crate::services::discord) use super::terminal_handler::native_supply::source_binding::{
    pin_source, release_unkeyed_source,
};
use super::*;
#[path = "native_restore.rs"]
mod native_restore;
use crate::services::tui_input::human_frame::history::v2::{Registry, native::Ticket};
pub(in crate::services::discord) use native_restore::{BootInputs, boot_ready};
pub(in crate::services::discord) use native_restore::{RestoreStep, RestoredInput};

// Producer, bridge and O host share this gate after custody and control consumers are installed.
pub(in crate::services::discord) fn enabled() -> bool {
    true
}

/// Installs the canonical physical identity before a primary's first key, under the row CAS.
pub(in crate::services::discord) async fn bind_hosted_row(
    shared: &Arc<SharedData>,
    key: TurnKey,
    actor: Option<&Arc<CancelToken>>,
    ticket: &Ticket,
) -> Result<(), String> {
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
    };
    let target = &ticket.key["attempt"]["target"];
    if actor.is_none() || target["host"] != "herdr" {
        return Ok(());
    }
    let Some(row) = super::super::inflight::load_inflight_state_read_only_result(
        &ProviderKind::Claude,
        key.channel_id.get(),
    )?
    else {
        return Ok(());
    };
    if let Some(nonce) = &row.hosted_execution_nonce {
        return (target["execution_nonce"] == *nonce)
            .then_some(())
            .ok_or_else(|| "native hosted row nonce changed".into());
    }
    if key.generation != shared.restart.current_generation
        || row.provider != "claude"
        || row.channel_id != key.channel_id.get()
        || row.effective_finalizer_turn_id() != key.user_msg_id
        || row.born_generation != key.generation
        || !key.matches_episode_nonce(row.turn_nonce.as_deref())
        || row.hosted_record_id.is_some()
        || row.host_locator.is_some()
    {
        return Err("native hosted row episode changed".into());
    }
    let current = shared.mailbox(key.channel_id).snapshot().await;
    if current.active_user_message_id.map(|id| id.get()) != Some(key.user_msg_id)
        || !key.matches_episode_nonce(current.active_turn_nonce.as_deref())
        || !current
            .cancel_token
            .as_ref()
            .zip(actor)
            .is_some_and(|(a, b)| Arc::ptr_eq(a, b))
    {
        return Err("native hosted actor changed".into());
    }
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or("native hosted row pool unavailable")?;
    let HostedLookup::Found(observed) = load_hosted_execution_pg(
        pool,
        HostedLookupKey::SessionKey(row.session_key.as_deref().ok_or("native row key missing")?),
    )
    .await
    else {
        return Err("native hosted row unreadable".into());
    };
    let HostedRecord::Known(record) = &observed.record else {
        return Err("native hosted row unknown".into());
    };
    let at = record
        .location
        .as_ref()
        .ok_or("native hosted location missing")?;
    if !matches!(record.state, HostedState::Pending | HostedState::Bound)
        || record.expected.is_none()
        || record.owner.provider != "claude"
        || record.owner.discord_token_hash != shared.token_hash
        || record.owner.channel_id != key.channel_id.get().to_string()
        || target["owner"] != record.owner.logical_key
        || target["execution_nonce"] != record.execution_nonce
        || target["root"] != at.socket_addr
        || at.host != "herdr"
    {
        return Err("native hosted target changed".into());
    }
    let source = &ticket.key["attempt"]["source"];
    let session = source["session_id"]
        .as_str()
        .or_else(|| source["expected_session_id"].as_str())
        .ok_or("native hosted session missing")?;
    let path = source["transcript"]
        .as_str()
        .or_else(|| source["expected_transcript"].as_str())
        .ok_or("native hosted transcript missing")?;
    let mut next = row.clone();
    next.hosted_record_id = Some(observed.session_id());
    next.hosted_execution_nonce = Some(record.execution_nonce.clone());
    next.host_locator = Some(
        serde_json::from_value(serde_json::json!({
            "host_kind":"herdr", "host_session_id":at.named_session,
            "pane":at.pane_id, "execution_node":at.execution_node,
        }))
        .map_err(|e| e.to_string())?,
    );
    next.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    next.tmux_session_name = Some(record.owner.logical_key.clone());
    next.session_id = Some(session.into());
    next.output_path = Some(path.into());
    // The PG lock pins the canonical record while the file CAS preserves any successor row.
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let raw: Option<Option<serde_json::Value>> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE id = $1 FOR SHARE")
            .bind(observed.session_id())
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    if raw.flatten().as_ref() != Some(&serde_json::to_value(record).map_err(|e| e.to_string())?) {
        return Err("native hosted record superseded".into());
    }
    let expected = super::super::inflight::InflightTurnIdentity::from_state(&row);
    let outcome = super::super::inflight::stamp_runtime_handoff_if_matches_identity(
        (&row, &mut next),
        &expected,
        "native_prepare::bind_hosted_row",
    );
    tx.commit().await.map_err(|e| e.to_string())?;
    (outcome == super::super::inflight::GuardedSaveOutcome::Saved)
        .then_some(())
        .ok_or_else(|| format!("native hosted row CAS refused: {outcome:?}"))
}

const START_MISSING: &str = "native primary Start missing";

pub(in crate::services::discord) struct Handle {
    finalizer: Arc<TurnFinalizer>,
    key: TurnKey,
    input: String,
    history: Registry,
    shared: Arc<SharedData>,
    finished: bool,
}

impl Handle {
    pub(in crate::services::discord) async fn prepare(
        shared: &Arc<SharedData>,
        key: TurnKey,
        actor: Option<Arc<CancelToken>>,
        history: Registry,
        namespace: String,
        ticket: Ticket,
    ) -> Result<Self, String> {
        if let Err(error) = bind_hosted_row(shared, key, actor.as_ref(), &ticket).await {
            history.native_not_sent(&namespace, &ticket).await?;
            return Err(error);
        }
        // The bridge registers the primary's Start concurrently; a bounded wait covers that race.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let input = loop {
            let (ack, rx) = oneshot::channel();
            shared
                .turn_finalizer
                .tx
                .send(FinalizeMsg::NativeRegister {
                    key,
                    actor: actor.clone(),
                    history: history.clone(),
                    namespace: namespace.clone(),
                    ticket: Box::new(ticket.clone()),
                    shared: shared.clone(),
                    ack,
                })
                .map_err(|_| "native registration actor unavailable")?;
            match rx
                .await
                .map_err(|_| "native registration acknowledgement lost")?
            {
                Err(e) if e == START_MISSING && tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                result => break result?,
            }
        };
        Ok(Self {
            finalizer: shared.turn_finalizer.clone(),
            key,
            input,
            history,
            shared: shared.clone(),
            finished: false,
        })
    }

    fn witness(&self) -> Result<(), String> {
        let keyed = crate::services::claude_tui::owner_input::key_attempted()
            .ok_or("native key scope unavailable")?;
        let (ack, rx) = oneshot::channel();
        self.finalizer
            .tx
            .send(FinalizeMsg::NativeKeyed {
                key: self.key,
                input: self.input.clone(),
                keyed,
                history: self.history.clone(),
                shared: self.shared.clone(),
                ack,
            })
            .map_err(|_| "native key actor unavailable")?;
        tokio::runtime::Handle::current()
            .block_on(rx)
            .map_err(|_| "native key acknowledgement lost")?
    }

    pub(in crate::services::discord) fn finish(mut self) -> Result<(), String> {
        let result = self.witness();
        self.finished = true;
        result
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.witness()
        {
            tracing::error!(%error, "native key witness remains unconfirmed");
        }
    }
}

impl TurnFinalizer {
    pub(in crate::services::discord) async fn native_settled(
        &self,
        key: TurnKey,
        actor: &CancelToken,
    ) -> Result<bool, String> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(FinalizeMsg::NativeSettled {
                key,
                actor: actor as *const CancelToken as usize,
                ack,
            })
            .map_err(|_| "native settlement actor unavailable")?;
        rx.await
            .map_err(|_| "native settlement acknowledgement lost".into())
    }

    pub(in crate::services::discord) async fn native_scope(
        &self,
        key: TurnKey,
        actor: &CancelToken,
    ) -> Result<bool, String> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(FinalizeMsg::NativeScope {
                key,
                actor: Some(actor as *const CancelToken as usize),
                ack,
            })
            .map_err(|_| "native scope actor unavailable")?;
        rx.await
            .map_err(|_| "native scope acknowledgement lost".into())
    }

    /// Whether native custody holds this exact turn, whichever actor drives it.
    async fn native_holds(&self, key: TurnKey) -> Result<bool, String> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(FinalizeMsg::NativeScope {
                key,
                actor: None,
                ack,
            })
            .map_err(|_| "native scope actor unavailable")?;
        rx.await
            .map_err(|_| "native scope acknowledgement lost".into())
    }
}

/// A row native custody holds is recorded by the native completion alone; an unanswerable
/// query counts as held so no other writer borrows its unscoped body.
pub(in crate::services::discord) async fn holds_row(
    shared: &SharedData,
    row: &super::super::inflight::InflightTurnState,
) -> bool {
    if row.provider != ProviderKind::Claude.as_str() {
        return false;
    }
    let key = TurnKey::new(
        ChannelId::new(row.channel_id),
        row.effective_finalizer_turn_id(),
        row.born_generation,
    )
    .with_episode_nonce(row.turn_nonce.as_deref());
    shared
        .turn_finalizer
        .native_holds(key)
        .await
        .unwrap_or(true)
}

pub(in crate::services::discord) async fn handle(
    msg: FinalizeMsg,
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
) {
    match msg {
        FinalizeMsg::NativeAbstain {
            input,
            finish,
            shared,
            ack,
        } => {
            let _ = ack.send(
                super::terminal_handler::native_supply::native_abstain::handle(
                    *input, finish, &shared, ledger,
                ),
            );
        }
        FinalizeMsg::NativeRemember { work, ack } => {
            let _ = ack.send(super::terminal_handler::native_supply::remember(
                ledger, &work,
            ));
        }
        FinalizeMsg::NativeInputs { channel, ack } => {
            let inputs = ledger
                .values()
                .filter(|e| e.turn_key.channel_id == channel)
                .flat_map(|e| e.native_inputs.values().cloned())
                .collect();
            let restored = ledger
                .values()
                .filter(|e| e.turn_key.channel_id == channel)
                .flat_map(|e| e.native_boot_inputs.values().cloned())
                .collect();
            let _ = ack.send((inputs, restored));
        }
        FinalizeMsg::NativeBoot {
            inputs,
            step,
            shared,
            ack,
        } => {
            let result = native_restore::restore(inputs, step, &shared, ledger).await;
            let _ = ack.send(result);
        }
        FinalizeMsg::NativeBootReady { channel, ack } => {
            let ready = ledger
                .values()
                .filter(|e| e.turn_key.channel_id == channel)
                .all(|entry| entry.native_boot_inputs.values().all(|input| input.settled));
            let _ = ack.send(ready);
        }
        FinalizeMsg::NativeRegister {
            key,
            actor,
            history,
            namespace,
            ticket,
            shared,
            ack,
        } => {
            let result = async {
                let id =
                    serde_json::to_string(&(&namespace, &ticket.key)).map_err(|e| e.to_string())?;
                let primary = ticket.owning_episode.is_some();
                let prepared = super::terminal_handler::NativePrepared::capture(
                    &history, &namespace, *ticket, key, actor, &shared,
                )
                .await?;
                if primary && !ledger.contains_key(&key.exact_key()) {
                    return Err(START_MISSING.into());
                }
                let entry = ledger.entry(key.exact_key()).or_insert_with(|| {
                    LedgerEntry::registered(
                        key,
                        ProviderKind::Claude,
                        RelayOwnerKind::None,
                        None,
                        CompletionAdmissionPlan::Immediate,
                        None,
                    )
                });
                if entry.phase == Phase::Finalized {
                    return Err("native episode already finalized".into());
                }
                if entry
                    .native_inputs
                    .get(&id)
                    .is_some_and(|existing| !existing.same_control_scope(&prepared))
                {
                    return Err("native control scope replaced".into());
                }
                prepared.install_control(&history, &shared).await?;
                entry.native_inputs.entry(id.clone()).or_insert(prepared);
                if primary {
                    entry.completion_admission.native_input = Some(false);
                }
                Ok(id)
            }
            .await;
            let _ = ack.send(result);
        }
        FinalizeMsg::NativeKeyed {
            key,
            input,
            keyed,
            history,
            shared,
            ack,
        } => {
            let result = async {
                let entry = ledger
                    .get_mut(&key.exact_key())
                    .ok_or("native entry missing")?;
                let prepared = entry
                    .native_inputs
                    .get(&input)
                    .ok_or("native registration missing")?;
                if keyed {
                    #[cfg(test)]
                    super::terminal_handler::native_supply::source_binding::bind_before_key_witness()
                        .await;
                    return prepared.any_key(&history);
                }
                let removed = prepared.remove_control(&history, &shared).await;
                entry.native_inputs.remove(&input);
                if entry.native_inputs.is_empty() {
                    entry.completion_admission.native_input = None;
                }
                removed
            }
            .await;
            let _ = ack.send(result);
        }
        FinalizeMsg::NativeSettled { key, actor, ack } => {
            let settled = ledger.get(&key.exact_key()).is_some_and(|entry| {
                entry.phase == Phase::Finalized
                    && entry.completion_admission.native_input == Some(true)
                    && !entry.native_debt_pending()
                    && entry
                        .native_inputs
                        .values()
                        .any(|input| input.owns_bridge(key, Some(actor)))
            });
            let _ = ack.send(settled);
        }
        FinalizeMsg::NativeScope { key, actor, ack } => {
            let owned = ledger.get(&key.exact_key()).is_some_and(|entry| {
                entry
                    .native_inputs
                    .values()
                    .any(|input| input.owns_bridge(key, actor))
                    || (actor.is_none()
                        && entry
                            .native_boot_inputs
                            .values()
                            .any(|input| input.identity().1.owning_episode.is_some()))
            });
            let _ = ack.send(owned);
        }
        _ => unreachable!("native custody dispatcher received another message"),
    }
}

pub(super) fn boot_pending(entry: &LedgerEntry) -> bool {
    entry
        .native_boot_inputs
        .values()
        .any(|input| !input.settled)
}

pub(in crate::services::discord) use super::terminal_handler::native_supply::port as supply_port;

pub(in crate::services::discord) use native_restore::validate_record as validate_saved_record;
