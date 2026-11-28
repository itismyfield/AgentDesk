//! A caller keeps its owner address, original and ticket together until an exact observer binds.
use super::admission::{self, Admission, AdmissionState, AdmissionTicket, TurnBind};
use super::origin::Execution;
use crate::services::engine_runtime::health_port::owner_input::{OwnerInput, OwnerInputActor};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::queued_head::v2::source_admission::ExactTurn;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, PendingQueueItem, RoutineInputSettlement,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};

pub(crate) static ACTOR: RoutineInputActor = RoutineInputActor;

pub(crate) struct RoutineInputActor;

impl OwnerInputActor for RoutineInputActor {
    fn owned_start<'a>(
        &self,
        handle: &'a ChannelMailboxHandle,
        provider: &'a str,
        process: bool,
        start: (&'a str, u64, &'a str),
    ) -> BoxFuture<'a, Result<Option<(PendingQueueItem, String)>, String>> {
        Box::pin(owned_start(handle, provider, process, start))
    }

    fn confirm_admission<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(confirm_admission(input, handle))
    }

    fn start_rejection<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(start_rejection(input, handle))
    }

    fn may_observe<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
        allow_admit: bool,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(may_observe(input, handle, allow_admit))
    }

    fn may_retry<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(may_retry(input, handle))
    }

    fn unbound_attempt<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<Option<ExactTurn>, String>> {
        Box::pin(unbound_attempt(input, handle))
    }

    fn bind_observer<'a>(
        &self,
        input: &'a mut OwnerInput,
        handle: &'a ChannelMailboxHandle,
        attempt: &'a str,
        source: Option<Value>,
        turn_id: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(bind_observer(input, handle, attempt, source, turn_id))
    }

    fn settle_delivery<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<RoutineInputSettlement, String>> {
        Box::pin(settle_delivery(input, handle))
    }
}

async fn settle_delivery(
    input: &OwnerInput,
    handle: &ChannelMailboxHandle,
) -> Result<RoutineInputSettlement, String> {
    if input.original["channel_id"].as_u64() != Some(input.channel) {
        return Err("owner channel mismatch".into());
    }
    let original = serde_json::from_value(input.original.clone())
        .map_err(|error| format!("stored original: {error}"))?;
    handle
        .settle_routine_input(
            (input.namespace.clone(), input.ticket.clone()),
            original,
            input.context.clone(),
            input.admission_committed == Some(true),
        )
        .await
}

async fn check_address(input: &OwnerInput, handle: &ChannelMailboxHandle) -> Result<(), String> {
    let namespace = input.namespace.clone();
    handle
        .v2_request(move |slot| {
            let owner = slot.as_ref().ok_or("owner not adopted")?;
            if owner.snapshot.namespace.as_deref() != Some(namespace.as_str()) {
                return Err("owner namespace mismatch".into());
            }
            Ok(())
        })
        .await?;
    if input.original["channel_id"].as_u64() != Some(input.channel) {
        return Err("owner channel mismatch".into());
    }
    Ok(())
}

async fn admit(input: &OwnerInput, handle: &ChannelMailboxHandle) -> Result<Admission, String> {
    check_address(input, handle).await?;
    let original: PendingQueueItem = serde_json::from_value(input.original.clone())
        .map_err(|e| format!("stored original: {e}"))?;
    if original.source_message_ids.len() > 1 || original.source_message_queued_generations.len() > 1
    {
        return Err("synthetic owner input must keep one original source".into());
    }
    Ok(admission::admit_with_context(
        handle,
        &original,
        AdmissionTicket::stored(&input.ticket),
        input.context.clone(),
    )
    .await)
}

async fn start_rejection(
    input: &OwnerInput,
    handle: &ChannelMailboxHandle,
) -> Result<Option<String>, String> {
    check_address(input, handle).await?;
    if !handle
        .v2_admission_status(input.ticket.clone())
        .await?
        .committed()
    {
        return Ok(None);
    }
    let original = serde_json::from_value(input.original.clone())
        .map_err(|e| format!("stored original: {e}"))?;
    let context = handle
        .v2_admission_context(original, input.ticket.clone())
        .await?;
    if context.as_ref() != Some(&input.context) {
        return Err("owner rejection context mismatch".into());
    }
    let original = input.original.clone();
    handle
        .v2_request(move |slot| {
            let state = slot.as_ref().ok_or("owner absent")?.json()?;
            Ok(state["retained_inputs"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|r| {
                    (r["input"]["original"] == original
                        && r["input"]["attempt"].is_null()
                        && r["disposition"] == "known_not_sent")
                        .then(|| {
                            r["reason"]["reason"]
                                .as_str()
                                .filter(|reason| reason.starts_with("start_rejected: "))
                                .map(str::to_string)
                        })
                        .flatten()
                }))
        })
        .await
}
/// NotArmed only means no arm occurred; it does not guarantee automatic progress.
async fn may_observe(
    input: &OwnerInput,
    handle: &ChannelMailboxHandle,
    allow_admit: bool,
) -> Result<bool, String> {
    check_address(input, handle).await?;
    let original: PendingQueueItem = serde_json::from_value(input.original.clone())
        .map_err(|e| format!("stored original: {e}"))?;
    let ticket = AdmissionTicket::stored(&input.ticket);
    let found = admission::exact_turn(handle, &ticket, &original).await?;
    if !matches!(found, TurnBind::Unknown | TurnBind::Absent) {
        let context = handle
            .v2_admission_context(original.clone(), input.ticket.clone())
            .await?;
        if context.as_ref() != Some(&input.context) {
            return Err("owner execution context mismatch".into());
        }
    }
    match found {
        TurnBind::Unknown | TurnBind::NotArmed => Ok(false),
        TurnBind::Absent => {
            if !allow_admit || input.admission_committed == Some(true) {
                return Ok(false);
            }
            match admission::status(handle, &ticket).await? {
                AdmissionState::Committed => {}
                AdmissionState::Pending | AdmissionState::Absent => {
                    if let Admission::Rejected(reason) = admit(input, handle).await? {
                        return Err(reason);
                    }
                }
            }
            Ok(false)
        }
        TurnBind::Armed(turn) | TurnBind::Settled(turn) => {
            Ok(input.bind.as_ref().is_some_and(|bind| {
                bind["turn_id"] == input.context["turn_id"]
                    && bind["attempt"].as_str().is_some_and(|attempt| {
                        turn.is(attempt, bind.get("source").filter(|v| !v.is_null()))
                    })
            }))
        }
    }
}

async fn may_retry(input: &OwnerInput, handle: &ChannelMailboxHandle) -> Result<bool, String> {
    if !may_observe(input, handle, false).await? {
        return Ok(false);
    }
    let original = serde_json::from_value(input.original.clone())
        .map_err(|e| format!("stored original: {e}"))?;
    Ok(matches!(
        admission::exact_turn(handle, &AdmissionTicket::stored(&input.ticket), &original).await?,
        TurnBind::Settled(_)
    ))
}

/// The owner's armed or settled attempt an unbound observer connects to, if any.
async fn unbound_attempt(
    input: &OwnerInput,
    handle: &ChannelMailboxHandle,
) -> Result<Option<ExactTurn>, String> {
    if input.bind.is_some() {
        return Ok(None);
    }
    check_address(input, handle).await?;
    let original = serde_json::from_value(input.original.clone())
        .map_err(|e| format!("stored original: {e}"))?;
    let ticket = AdmissionTicket::stored(&input.ticket);
    Ok(
        match admission::exact_turn(handle, &ticket, &original).await? {
            TurnBind::Armed(turn) | TurnBind::Settled(turn)
                if observable(handle, &turn.attempt).await? =>
            {
                Some(turn)
            }
            _ => None,
        },
    )
}

/// The existing observer supplies its logical turn and exact attempt before completion is read.
async fn bind_observer(
    input: &mut OwnerInput,
    handle: &ChannelMailboxHandle,
    attempt: &str,
    source: Option<Value>,
    turn_id: &str,
) -> Result<(), String> {
    let expected = json!({"attempt":attempt,"source":source,"turn_id":turn_id});
    if input.bind.as_ref().is_some_and(|old| old != &expected)
        || input.context["turn_id"] != turn_id
    {
        return Err("stale observer bind".into());
    }
    check_address(input, handle).await?;
    let original = serde_json::from_value(input.original.clone())
        .map_err(|e| format!("stored original: {e}"))?;
    let ticket = AdmissionTicket::stored(&input.ticket);
    let found = admission::exact_turn(handle, &ticket, &original).await?;
    match found {
        TurnBind::Armed(turn) | TurnBind::Settled(turn)
            if turn.is(attempt, source.as_ref()) && observable(handle, attempt).await? =>
        {
            let context = handle
                .v2_admission_context(original, input.ticket.clone())
                .await?;
            if context.as_ref() != Some(&input.context) {
                return Err("owner execution context mismatch".into());
            }
            input.bind = Some(expected);
            Ok(())
        }
        _ => Err("observer does not name the owner's exact turn".into()),
    }
}

/// An attempt past its bootstrap window: a modal can no longer end it unsent for a retry.
async fn observable(handle: &ChannelMailboxHandle, attempt: &str) -> Result<bool, String> {
    let attempt = attempt.to_string();
    handle
        .v2_request(move |slot| {
            Ok(!slot
                .as_ref()
                .ok_or("V2 not adopted")?
                .bootstrapping(&attempt))
        })
        .await
}

/// A synthetic start's original: its reservation's message as a bot input of this process's
/// generation, and the ticket that turn names on every re-request.
pub(crate) fn synthetic_original(
    turn_id: &str,
    channel: u64,
    text: &str,
    author: u64,
) -> Result<(PendingQueueItem, AdmissionTicket), String> {
    let message = (turn_id.rsplit(':').next())
        .and_then(|id| id.parse::<u64>().ok())
        .ok_or("reserved source missing")?;
    let generation = crate::services::discord::runtime_store::process_generation();
    let original = serde_json::from_value(serde_json::json!({"author_id":author,
        "author_is_bot":true,"message_id":message,"source_message_ids":[message],
        "channel_id":channel,"queued_generation":generation,"text":text}))
    .map_err(|error| error.to_string())?;
    let ticket = AdmissionTicket::stored(&format!("turn-{channel}-{message}"));
    Ok((original, ticket))
}

/// A synthetic start's original and the owner's namespace when the owner takes a start on this
/// target; `None` leaves a process target or an unowned channel to its legacy start.
async fn owned_start(
    handle: &ChannelMailboxHandle,
    provider: &str,
    process: bool,
    (turn_id, channel, text): (&str, u64, &str),
) -> Result<Option<(PendingQueueItem, String)>, String> {
    use super::origin::InputOrigin;
    let execution = execution(provider, channel, process);
    if !super::ingress::owner_will_take(handle, execution, InputOrigin::Routine).await? {
        return Ok(None);
    }
    let namespace = handle
        .v2_request(|slot| {
            (slot
                .as_ref()
                .and_then(|owner| owner.snapshot.namespace.clone()))
            .ok_or_else(|| "owner namespace missing".into())
        })
        .await?;
    let (original, _) = synthetic_original(turn_id, channel, text, 1)?;
    Ok(Some((original, namespace)))
}

/// How the owner runs a start on this channel; a process routine never reaches the TUI owner.
fn execution(provider: &str, channel: u64, process: bool) -> Execution {
    match ProviderKind::from_str(provider) {
        Some(_) if process => Execution::Process,
        Some(provider) => super::ingress::execution(&provider, channel),
        None => Execution::NonTuiProvider,
    }
}

async fn confirm_admission(
    input: &OwnerInput,
    handle: &ChannelMailboxHandle,
) -> Result<bool, String> {
    match admit(input, handle).await? {
        Admission::Accepted(_) => Ok(true),
        Admission::Pending(_) => Ok(false),
        Admission::Rejected(reason) => Err(reason),
    }
}

#[cfg(all(test, unix))]
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
        let input = OwnerInput::new(&original, "unknown", "claude", json!({"turn_id":"logical"}));
        let uuid = uuid::Uuid::parse_str(&input.ticket).unwrap();
        assert_eq!(input.ticket, uuid.simple().to_string());
        let mut stored = serde_json::to_value(&input).unwrap();
        assert!(stored.get("admission_committed").is_none());
        assert_eq!(
            stored.as_object_mut().unwrap().remove("bind"),
            Some(Value::Null)
        );
        let mut input = OwnerInput::restore(stored).unwrap();
        assert_eq!(input.ticket, uuid.simple().to_string());
        assert!(input.bind.is_none());
        assert!(input.admission_committed.is_none());
        let before_admission = std::fs::read(&path).unwrap();
        assert!(
            bind_observer(&mut input, &handle, "nonce", None, "logical")
                .await
                .is_err()
        );
        assert_eq!(
            before_admission,
            std::fs::read(&path).unwrap(),
            "a stale observer never admits an absent ticket"
        );
        assert!(matches!(
            admit(&input, &handle).await.unwrap(),
            Admission::Accepted(_)
        ));
        handle.v2_arm_head_for_tests("nonce").await.unwrap();
        assert!(
            bind_observer(&mut input, &handle, "wrong", None, "logical")
                .await
                .is_err()
        );
        bind_observer(&mut input, &handle, "nonce", None, "logical")
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
        assert!(!may_observe(&input, &handle, false).await.unwrap());
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
        assert!(admit(&input, &handle).await.is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
    }
}
