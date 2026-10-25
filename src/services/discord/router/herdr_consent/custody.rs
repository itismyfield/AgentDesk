//! Consent releases refer to existing custody; they never copy or enqueue a payload.
use super::super::super::{ChannelId, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::session_host::policy::{
    consent::{
        ConsentKey, IntentRef,
        settlement::{CustodySink, SettlementKey},
    },
    durable::{self, Directory},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Released {
    #[serde(default)]
    consent: String,
    nonce: String,
    intent: IntentRef,
    woken: bool,
}

static WAKES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) struct LiveSink {
    consent: String,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    directory: Directory,
}

impl LiveSink {
    pub(super) fn new(
        shared: Arc<SharedData>,
        provider: ProviderKind,
        key: &ConsentKey,
    ) -> Result<Self, String> {
        Ok(Self {
            consent: durable::key(key)?,
            shared,
            provider,
            directory: Directory(
                crate::config::runtime_root()
                    .map(|root| root.join("runtime/herdr_policy/consents/custody")),
            ),
        })
    }

    /// The exact original payload may pass admission after release, before consent acknowledgement.
    pub(super) async fn released(&self, intent: &IntentRef) -> Result<bool, String> {
        let key = SettlementKey {
            nonce: "eligibility".into(),
            store: intent.store.clone(),
            id: intent.id.clone(),
        };
        let Some(released) = self.read(self.identity(&key, intent)?).await? else {
            return Ok(false);
        };
        if released.intent != *intent || released.consent != self.consent {
            return Err("custody release belongs to a different consent or source".into());
        }
        Ok(true)
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
        let consent = self.consent.clone();
        tokio::task::spawn_blocking(move || {
            directory
                .change(&name, |previous| {
                    let mut released = match previous {
                        Some(bytes) => {
                            serde_json::from_slice::<Released>(&bytes).map_err(|e| e.to_string())?
                        }
                        None => Released {
                            consent: consent.clone(),
                            nonce,
                            intent: intent.clone(),
                            woken: false,
                        },
                    };
                    if released.intent != intent || released.consent != consent {
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

    async fn mailbox_pending(&self, intent: &IntentRef) -> Result<bool, String> {
        if intent.id != format!("{}:{}", intent.source_message, intent.fifo) {
            return Err(
                "mailbox custody reference must bind its source and queued generation".into(),
            );
        }
        use crate::services::turn_orchestrator as queue;
        let provider = self.provider.clone();
        let token_hash = self.shared.token_hash.clone();
        let channel = ChannelId::new(intent.channel);
        // Drain writes the marker before trimming the queue and requeue writes the queue before
        // deleting the marker, so queue reads around the marker read see an item in transit.
        let disk = tokio::task::spawn_blocking(move || {
            let read =
                || queue::load_channel_pending_queue_checked(&provider, &token_hash, channel);
            let mut items = read()?.0;
            let marker = queue::load_channel_pending_dispatch_marker_checked(
                &provider,
                &token_hash,
                channel,
            )?;
            items.extend(marker.map(|(item, _)| item));
            items.extend(read()?.0);
            Ok::<_, String>(items)
        })
        .await
        .map_err(|e| e.to_string())??;
        let matches = |item: &queue::Intervention| {
            item.source_message_queued_generations()
                .iter()
                .any(|source| {
                    source.message_id.get() == intent.source_message
                        && source.queued_generation == intent.fifo
                })
        };
        let live = self
            .shared
            .mailbox(channel)
            .try_snapshot()
            .await
            .map_err(|e| format!("mailbox unavailable: {e:?}"))?;
        let persisted = disk.iter().any(matches);
        // A drained head is live as the dispatch reservation or the active turn it became.
        let source = poise::serenity_prelude::MessageId::new(intent.source_message);
        let restored = live.intervention_queue.iter().any(matches)
            || [live.pending_user_dispatch, live.active_user_message_id].contains(&Some(source))
            || live.pending_user_dispatch_source_ids.contains(&source)
            || live.active_absorbed_source_ids.contains(&source);
        if persisted != restored {
            return Err("mailbox custody is not durably restored; retry recovery".into());
        }
        Ok(persisted)
    }
}

#[async_trait::async_trait]
impl CustodySink for LiveSink {
    async fn release(&mut self, key: &SettlementKey, intent: &IntentRef) -> Result<(), String> {
        let name = self.identity(key, intent)?;
        if let Some(released) = self.read(name.clone()).await? {
            return if released.intent == *intent && released.consent == self.consent {
                Ok(())
            } else {
                Err("custody source changed".into())
            };
        }
        match intent.store.as_str() {
            "mailbox" => {
                if !self.mailbox_pending(intent).await? {
                    return Err(
                        "durable mailbox custody reference missing; preserve consent hold".into(),
                    );
                }
            }
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
                    (&self.consent, &key.nonce),
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
        let _wake = WAKES.lock().await;
        let name = self.identity(key, intent)?;
        let released = self
            .read(name.clone())
            .await?
            .ok_or("custody wake requires durable release")?;
        if released.intent != *intent || released.consent != self.consent {
            return Err("custody source changed".into());
        }
        if released.woken {
            return Ok(());
        }
        if intent.store == "mailbox" && self.mailbox_pending(intent).await? {
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
