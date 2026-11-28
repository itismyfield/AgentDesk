use std::any::Any;
use std::sync::{Arc, Weak};

use futures::future::BoxFuture;
use poise::serenity_prelude::{ChannelId, Http};

use crate::domain::bot_role::UtilityBotRole;
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

pub(crate) trait ChannelRuntimeLookup: Send + Sync {
    fn shared_for_provider_on_channel<'a>(
        &'a self,
        provider: &'a ProviderKind,
        channel: ChannelId,
    ) -> BoxFuture<'a, Option<Arc<SharedData>>>;
}

#[derive(Clone)]
pub(in crate::services::discord) struct UnboundRuntimeSnapshot {
    pub generation: u64,
    pub entries: Vec<(String, Arc<SharedData>)>,
}

pub(in crate::services::discord) trait RuntimeHealth:
    Any + ChannelRuntimeLookup
{
    fn registered_token_hashes<'a>(
        &'a self,
        provider: &'a ProviderKind,
    ) -> BoxFuture<'a, Vec<String>>;

    fn utility_bot_http_clone(&self, role: UtilityBotRole) -> BoxFuture<'_, Option<Arc<Http>>>;

    fn utility_bot_user_id(&self, role: UtilityBotRole) -> BoxFuture<'_, Option<u64>>;

    fn unbound_runtime_snapshot(&self) -> BoxFuture<'_, UnboundRuntimeSnapshot>;

    fn with_unbound_membership(
        &self,
        expected_generation: u64,
        publish: &mut dyn FnMut(),
    ) -> Result<(), &'static str>;
}

#[derive(Default)]
pub(in crate::services) struct WeakRuntimeHealth(Option<Weak<dyn RuntimeHealth>>);

impl<T: RuntimeHealth> From<&Arc<T>> for WeakRuntimeHealth {
    fn from(registry: &Arc<T>) -> Self {
        let weak = Arc::downgrade(registry);
        Self(Some(weak))
    }
}

impl WeakRuntimeHealth {
    pub(in crate::services::discord) fn upgrade(&self) -> Option<Arc<dyn RuntimeHealth>> {
        self.0.as_ref()?.upgrade()
    }
}
