//! Pre-key custody belongs to the finalizer actor; a leaf carries only its acknowledged handle.
use super::*;
use crate::services::tui_input::human_frame::history::v2::{Registry, native::Ticket};

// Completion evidence production is outside this rollout, so default inputs keep their old path.
pub(in crate::services::discord) fn enabled() -> bool {
    false
}

pub(in crate::services::discord) struct Handle {
    finalizer: Arc<TurnFinalizer>,
    key: TurnKey,
    input: String,
    history: Registry,
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
        let (ack, rx) = oneshot::channel();
        shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeRegister {
                key,
                actor,
                history: history.clone(),
                namespace,
                ticket: Box::new(ticket),
                shared: shared.clone(),
                ack,
            })
            .map_err(|_| "native registration actor unavailable")?;
        let input = rx
            .await
            .map_err(|_| "native registration acknowledgement lost")??;
        Ok(Self {
            finalizer: shared.turn_finalizer.clone(),
            key,
            input,
            history,
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
    pub(in crate::services::discord) async fn native_scope(
        &self,
        key: TurnKey,
        actor: &CancelToken,
    ) -> Result<bool, String> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(FinalizeMsg::NativeScope {
                key,
                actor: actor as *const CancelToken as usize,
                ack,
            })
            .map_err(|_| "native scope actor unavailable")?;
        rx.await
            .map_err(|_| "native scope acknowledgement lost".into())
    }
}

pub(super) async fn handle(msg: FinalizeMsg, ledger: &mut HashMap<LedgerKey, LedgerEntry>) {
    match msg {
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
                    return Err("native primary Start missing".into());
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
            ack,
        } => {
            let result = (|| {
                let entry = ledger
                    .get_mut(&key.exact_key())
                    .ok_or("native entry missing")?;
                let prepared = entry
                    .native_inputs
                    .get(&input)
                    .ok_or("native registration missing")?;
                if keyed {
                    return prepared.any_key(&history);
                }
                entry.native_inputs.remove(&input);
                if entry.native_inputs.is_empty() {
                    entry.completion_admission.native_input = None;
                }
                Ok(())
            })();
            let _ = ack.send(result);
        }
        FinalizeMsg::NativeScope { key, actor, ack } => {
            let owned = ledger.get(&key.exact_key()).is_some_and(|entry| {
                entry
                    .native_inputs
                    .values()
                    .any(|input| input.owns_bridge(key, actor))
            });
            let _ = ack.send(owned);
        }
        _ => unreachable!("native custody dispatcher received another message"),
    }
}
