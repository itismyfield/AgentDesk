use std::sync::OnceLock;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use crate::services::turn_orchestrator::queued_head::v2::source_admission::ExactTurn;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, PendingQueueItem, RoutineInputSettlement,
};

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
            ticket: new_owner_input_ticket(),
            provider: provider.into(),
            namespace: namespace.into(),
            channel: original.channel_id.unwrap_or_default(),
            original: serde_json::to_value(original).expect("queue original serializes"),
            context,
            bind: None,
            admission_committed: None,
        }
    }

    pub(crate) fn restore(value: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}

pub(crate) fn new_owner_input_ticket() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

pub(crate) trait OwnerInputRuntimePort: Send + Sync {
    fn owner_mailbox<'a>(
        &'a self,
        provider: &'a str,
        channel: u64,
    ) -> BoxFuture<'a, Result<ChannelMailboxHandle, String>>;

    fn owner_runtime_pool<'a>(
        &'a self,
        provider: &'a str,
        channel: u64,
    ) -> BoxFuture<'a, Result<PgPool, String>>;

    fn wake_owner<'a>(&'a self, provider: &'a str, channel: u64) -> BoxFuture<'a, ()>;
}

pub(crate) trait OwnerInputActor: Sync {
    fn owned_start<'a>(
        &self,
        handle: &'a ChannelMailboxHandle,
        provider: &'a str,
        process: bool,
        start: (&'a str, u64, &'a str),
    ) -> BoxFuture<'a, Result<Option<(PendingQueueItem, String)>, String>>;

    fn confirm_admission<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<bool, String>>;

    fn start_rejection<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<Option<String>, String>>;

    fn may_observe<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
        allow_admit: bool,
    ) -> BoxFuture<'a, Result<bool, String>>;

    fn may_retry<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<bool, String>>;

    fn unbound_attempt<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<Option<ExactTurn>, String>>;

    fn bind_observer<'a>(
        &self,
        input: &'a mut OwnerInput,
        handle: &'a ChannelMailboxHandle,
        attempt: &'a str,
        source: Option<Value>,
        turn_id: &'a str,
    ) -> BoxFuture<'a, Result<(), String>>;

    fn settle_delivery<'a>(
        &self,
        input: &'a OwnerInput,
        handle: &'a ChannelMailboxHandle,
    ) -> BoxFuture<'a, Result<RoutineInputSettlement, String>>;
}

static ACTOR: OnceLock<&'static dyn OwnerInputActor> = OnceLock::new();

pub(crate) fn install_owner_input_actor(actor: &'static dyn OwnerInputActor) {
    ACTOR.get_or_init(|| actor);
}

pub(crate) fn owner_input_actor() -> &'static dyn OwnerInputActor {
    *ACTOR.get().expect("owner input actor installed by Discord")
}
