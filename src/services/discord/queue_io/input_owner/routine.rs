//! A caller keeps its owner address, original and ticket together until an exact observer binds.
use super::admission::{self, Admission, AdmissionState, AdmissionTicket, TurnBind};
use crate::services::discord::health::HealthRegistry;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct OwnerInput {
    pub(crate) ticket: String,
    pub(crate) provider: String,
    pub(crate) namespace: String,
    pub(crate) channel: u64,
    pub(crate) original: Value,
    pub(crate) context: Value,
    #[serde(default)]
    pub(crate) bind: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) admission_committed: Option<bool>,
}

impl OwnerInput {
    pub(crate) fn new(
        original: &PendingQueueItem,
        namespace: &str,
        provider: &str,
        context: Value,
    ) -> Self {
        Self {
            ticket: AdmissionTicket::new().as_str().to_owned(),
            provider: provider.into(),
            namespace: namespace.into(),
            channel: original.channel_id.unwrap_or_default(),
            original: serde_json::to_value(original).expect("queue original serializes"),
            context,
            bind: None,
            admission_committed: None,
        }
    }

    async fn check_address(&self, handle: &ChannelMailboxHandle) -> Result<(), String> {
        let namespace = self.namespace.clone();
        handle
            .v2_request(move |slot| {
                let owner = slot.as_ref().ok_or("owner not adopted")?;
                if owner.snapshot.namespace.as_deref() != Some(namespace.as_str()) {
                    return Err("owner namespace mismatch".into());
                }
                Ok(())
            })
            .await?;
        if self.original["channel_id"].as_u64() != Some(self.channel) {
            return Err("owner channel mismatch".into());
        }
        Ok(())
    }

    pub(crate) async fn admit(&self, handle: &ChannelMailboxHandle) -> Result<Admission, String> {
        self.check_address(handle).await?;
        let original: PendingQueueItem = serde_json::from_value(self.original.clone())
            .map_err(|e| format!("stored original: {e}"))?;
        if original.source_message_ids.len() > 1
            || original.source_message_queued_generations.len() > 1
        {
            return Err("synthetic owner input must keep one original source".into());
        }
        Ok(admission::admit_with_context(
            handle,
            &original,
            AdmissionTicket::stored(&self.ticket),
            self.context.clone(),
        )
        .await)
    }

    /// NotArmed only means no arm occurred; it does not guarantee automatic progress.
    pub(crate) async fn may_observe(
        &self,
        handle: &ChannelMailboxHandle,
        allow_admit: bool,
    ) -> Result<bool, String> {
        self.check_address(handle).await?;
        let original: PendingQueueItem = serde_json::from_value(self.original.clone())
            .map_err(|e| format!("stored original: {e}"))?;
        let ticket = AdmissionTicket::stored(&self.ticket);
        let found = admission::exact_turn(handle, &ticket, &original).await?;
        if !matches!(found, TurnBind::Unknown | TurnBind::Absent) {
            let context = handle
                .v2_admission_context(original.clone(), self.ticket.clone())
                .await?;
            if context.as_ref() != Some(&self.context) {
                return Err("owner execution context mismatch".into());
            }
        }
        match found {
            TurnBind::Unknown | TurnBind::NotArmed => Ok(false),
            TurnBind::Absent => {
                if !allow_admit || self.admission_committed == Some(true) {
                    return Ok(false);
                }
                match admission::status(handle, &ticket).await? {
                    AdmissionState::Committed => {}
                    AdmissionState::Pending | AdmissionState::Absent => {
                        if let Admission::Rejected(reason) = self.admit(handle).await? {
                            return Err(reason);
                        }
                    }
                }
                Ok(false)
            }
            TurnBind::Armed(turn) | TurnBind::Settled(turn) => {
                Ok(self.bind.as_ref().is_some_and(|bind| {
                    bind["turn_id"] == self.context["turn_id"]
                        && bind["attempt"].as_str().is_some_and(|attempt| {
                            turn.is(attempt, bind.get("source").filter(|v| !v.is_null()))
                        })
                }))
            }
        }
    }

    pub(crate) async fn may_retry(&self, handle: &ChannelMailboxHandle) -> Result<bool, String> {
        if !self.may_observe(handle, false).await? {
            return Ok(false);
        }
        let original = serde_json::from_value(self.original.clone())
            .map_err(|e| format!("stored original: {e}"))?;
        Ok(matches!(
            admission::exact_turn(handle, &AdmissionTicket::stored(&self.ticket), &original)
                .await?,
            TurnBind::Settled(_)
        ))
    }

    /// The existing observer supplies its logical turn and exact attempt before completion is read.
    pub(crate) async fn bind_observer(
        &mut self,
        handle: &ChannelMailboxHandle,
        attempt: &str,
        source: Option<Value>,
        turn_id: &str,
    ) -> Result<(), String> {
        let expected = json!({"attempt":attempt,"source":source,"turn_id":turn_id});
        if self.bind.as_ref().is_some_and(|old| old != &expected)
            || self.context["turn_id"] != turn_id
        {
            return Err("stale observer bind".into());
        }
        self.check_address(handle).await?;
        let original = serde_json::from_value(self.original.clone())
            .map_err(|e| format!("stored original: {e}"))?;
        let ticket = AdmissionTicket::stored(&self.ticket);
        let found = admission::exact_turn(handle, &ticket, &original).await?;
        match found {
            TurnBind::Armed(turn) | TurnBind::Settled(turn)
                if turn.is(attempt, source.as_ref()) =>
            {
                let context = handle
                    .v2_admission_context(original, self.ticket.clone())
                    .await?;
                if context.as_ref() != Some(&self.context) {
                    return Err("owner execution context mismatch".into());
                }
                self.bind = Some(expected);
                Ok(())
            }
            _ => Err("observer does not name the owner's exact turn".into()),
        }
    }
}

pub(crate) async fn mailbox(
    registry: &HealthRegistry,
    provider: &str,
    channel: u64,
) -> Result<ChannelMailboxHandle, String> {
    let provider = ProviderKind::from_str(provider).ok_or("owner provider invalid")?;
    if channel == 0 {
        return Err("owner channel invalid".into());
    }
    let channel = poise::serenity_prelude::ChannelId::new(channel);
    let shared = registry
        .shared_for_provider_on_channel(&provider, channel)
        .await
        .ok_or("owner runtime unavailable")?;
    Ok(shared.mailbox(channel))
}

impl ChannelMailboxHandle {
    pub(crate) fn new_owner_input(
        original: &PendingQueueItem,
        namespace: &str,
        provider: &str,
        context: Value,
    ) -> OwnerInput {
        OwnerInput::new(original, namespace, provider, context)
    }

    pub(crate) fn restore_owner_input(value: Value) -> Result<OwnerInput, serde_json::Error> {
        serde_json::from_value(value)
    }

    pub(crate) async fn owner_input_mailbox(
        registry: &HealthRegistry,
        provider: &str,
        channel: u64,
    ) -> Result<Self, String> {
        mailbox(registry, provider, channel).await
    }
}

impl OwnerInput {
    pub(crate) async fn confirm_admission(
        &self,
        handle: &ChannelMailboxHandle,
    ) -> Result<bool, String> {
        match self.admit(handle).await? {
            Admission::Accepted(_) => Ok(true),
            Admission::Pending(_) => Ok(false),
            Admission::Rejected(reason) => Err(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TestRuntimeRootGuard;
    use crate::services::discord::runtime_store::discord_pending_queue_root;
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    #[tokio::test]
    async fn committed_unknown_never_readmits_or_observes_a_stale_completion() {
        let _root = TestRuntimeRootGuard::new();
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(poise::serenity_prelude::ChannelId::new(7272991));
        let path = discord_pending_queue_root().unwrap().join("unknown.json");
        handle
            .v2_adopt(path.clone(), "unknown".into())
            .await
            .unwrap();
        let original =
            serde_json::from_value(json!({"author_id":1,"message_id":8000000000000001991u64,
            "channel_id":7272991,"queued_generation":9,"text":"raw"}))
            .unwrap();
        let mut input =
            OwnerInput::new(&original, "unknown", "claude", json!({"turn_id":"logical"}));
        let before_admission = std::fs::read(&path).unwrap();
        assert!(
            input
                .bind_observer(&handle, "nonce", None, "logical")
                .await
                .is_err()
        );
        assert_eq!(
            before_admission,
            std::fs::read(&path).unwrap(),
            "a stale observer never admits an absent ticket"
        );
        assert!(matches!(
            input.admit(&handle).await.unwrap(),
            Admission::Accepted(_)
        ));
        handle.v2_arm_head_for_tests("nonce").await.unwrap();
        assert!(
            input
                .bind_observer(&handle, "wrong", None, "logical")
                .await
                .is_err()
        );
        input
            .bind_observer(&handle, "nonce", None, "logical")
            .await
            .unwrap();
        handle
            .v2_request(|slot| {
                let owner = slot.as_mut().ok_or("owner absent")?;
                let key = owner.active_attempt()?;
                owner.settle(&key, true, 1)?;
                owner.tick(86_400_001)
            })
            .await
            .unwrap();
        assert!(matches!(
            admission::exact_turn(&handle, &AdmissionTicket::stored(&input.ticket), &original)
                .await
                .unwrap(),
            TurnBind::Unknown
        ));
        let before = std::fs::read(&path).unwrap();
        assert!(!input.may_observe(&handle, false).await.unwrap());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
    #[tokio::test]
    async fn grouped_synthetic_sources_are_refused_before_split_admission() {
        let _root = TestRuntimeRootGuard::new();
        let registry = ChannelMailboxRegistry::default();
        let handle = registry.handle(poise::serenity_prelude::ChannelId::new(7272992));
        let path = discord_pending_queue_root().unwrap().join("grouped.json");
        handle
            .v2_adopt(path.clone(), "grouped".into())
            .await
            .unwrap();
        let original:PendingQueueItem=serde_json::from_value(json!({"author_id":1,"message_id":8000000000000001992u64,
            "channel_id":7272992,"queued_generation":9,"source_message_ids":[8000000000000001992u64,8000000000000001993u64],"text":"A+B"})).unwrap();
        let input = OwnerInput::new(&original, "grouped", "claude", json!({"turn_id":"logical"}));
        let before = std::fs::read(&path).unwrap();
        assert!(input.admit(&handle).await.is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}
