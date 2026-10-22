//! Consent releases refer to existing custody; they never copy or enqueue a payload.
use super::super::super::{ChannelId, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::session_host::policy::{
    consent::{
        IntentRef,
        settlement::{CustodySink, SettlementKey},
    },
    durable::{self, Directory},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Released {
    nonce: String,
    intent: IntentRef,
    woken: bool,
}

pub(super) struct LiveSink {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    directory: Directory,
}

impl LiveSink {
    pub(super) fn new(shared: Arc<SharedData>, provider: ProviderKind) -> Self {
        Self {
            shared,
            provider,
            directory: Directory(
                crate::config::runtime_root()
                    .map(|root| root.join("runtime/herdr_policy/consents/custody")),
            ),
        }
    }

    fn identity(&self, key: &SettlementKey, intent: &IntentRef) -> Result<String, String> {
        if key.store != intent.store
            || key.id != intent.id
            || key.nonce.is_empty()
            || intent.provider != self.provider.as_str()
            || intent.provider != self.shared.provider.as_str()
            || intent.channel == 0
            || intent.source_message == 0
        {
            return Err("consent custody provider or source identity changed; preserve the original reference".into());
        }
        durable::key(&(key.store.as_str(), key.id.as_str()))
    }

    async fn read(&self, name: String) -> Result<Option<Released>, String> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            directory
                .read_stable(&name)?
                .map(|bytes| {
                    serde_json::from_slice(&bytes)
                        .map_err(|e| format!("custody marker corrupt: {e}"))
                })
                .transpose()
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn commit(
        &self,
        name: String,
        nonce: String,
        intent: IntentRef,
        woken: bool,
    ) -> Result<(), String> {
        let directory = self.directory.clone();
        tokio::task::spawn_blocking(move || {
            directory
                .change(&name, |previous| {
                    let mut released = match previous {
                        Some(bytes) => {
                            serde_json::from_slice::<Released>(&bytes).map_err(|e| e.to_string())?
                        }
                        None => Released {
                            nonce,
                            intent: intent.clone(),
                            woken: false,
                        },
                    };
                    if released.intent != intent {
                        return Err("custody reference changed; preserve its marker".into());
                    }
                    released.woken |= woken;
                    serde_json::to_vec(&released).map_err(|e| e.to_string())
                })
                .map(|_| ())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn mailbox_present(&self, intent: &IntentRef) -> Result<(), String> {
        if intent.id != format!("{}:{}", intent.source_message, intent.fifo) {
            return Err(
                "mailbox custody reference must bind its source and queued generation".into(),
            );
        }
        let provider = self.provider.clone();
        let token_hash = self.shared.token_hash.clone();
        let channel = ChannelId::new(intent.channel);
        let (disk, _) = tokio::task::spawn_blocking(move || {
            crate::services::turn_orchestrator::load_channel_pending_queue_checked(
                &provider,
                &token_hash,
                channel,
            )
        })
        .await
        .map_err(|e| e.to_string())??;
        let matches = |item: &crate::services::turn_orchestrator::Intervention| {
            item.source_message_queued_generations()
                .iter()
                .any(|source| {
                    source.message_id.get() == intent.source_message
                        && source.queued_generation == intent.fifo
                })
        };
        if !disk.iter().any(matches) {
            return Err("durable mailbox custody reference missing; preserve consent hold".into());
        }
        let live = self
            .shared
            .mailbox(channel)
            .try_snapshot()
            .await
            .map_err(|e| format!("mailbox unavailable: {e:?}"))?;
        if !live.intervention_queue.iter().any(matches) {
            return Err("mailbox custody has not been restored; retry recovery".into());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl CustodySink for LiveSink {
    async fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        let name = self.identity(key, intent)?;
        if let Some(released) = self.read(name.clone()).await? {
            return if released.intent == *intent {
                Ok(())
            } else {
                Err("custody source changed".into())
            };
        }
        match intent.store.as_str() {
            "mailbox" => self.mailbox_present(intent).await?,
            "intake_outbox" => {
                let id = intent
                    .id
                    .parse()
                    .map_err(|_| "invalid original outbox row id")?;
                let pool = self
                    .shared
                    .pg_pool
                    .as_ref()
                    .ok_or("consent outbox store unavailable")?;
                crate::db::intake_outbox::consent_handoff::release(
                    pool,
                    id,
                    &intent.provider,
                    intent.channel,
                    intent.source_message,
                    intent.fifo,
                    &key.nonce,
                )
                .await?;
            }
            _ => {
                return Err(
                    "unknown consent custody store; preserve the original reference".into(),
                );
            }
        }
        // Commit before waking: a lost response can be replayed after the original payload ran.
        self.commit(name, key.nonce.clone(), intent.clone(), false)
            .await
    }

    async fn wake(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        let name = self.identity(key, intent)?;
        let released = self
            .read(name.clone())
            .await?
            .ok_or("custody wake requires durable release")?;
        if released.intent != *intent {
            return Err("custody source changed".into());
        }
        if released.woken {
            return Ok(());
        }
        if intent.store == "mailbox" {
            let channel = ChannelId::new(intent.channel);
            let outcome =
                crate::services::discord::queue_io::kick_idle_queue_channel_if_context_available(
                    &self.shared,
                    &self.provider,
                    channel,
                    "herdr_consent_release",
                )
                .await;
            crate::services::discord::queue_io::arm_event_backstop_after_no_start_if_queue_nonempty(
                &self.shared, &self.provider, channel, outcome, "herdr_consent_release").await;
        }
        // Outbox pending rows are already watched by the existing worker polling loop.
        self.commit(name, released.nonce, intent.clone(), true)
            .await
    }
}

#[cfg(test)]
#[path = "custody_tests.rs"]
mod tests;
