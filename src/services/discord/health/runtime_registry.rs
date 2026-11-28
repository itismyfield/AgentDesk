use std::sync::{Arc, Mutex, Weak};

use futures::future::BoxFuture;
use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

use super::HealthRegistry;
use crate::services::discord::SharedData;
use crate::services::discord::queue_io::input_owner::ingress;
use crate::services::engine_runtime::health_port::owner_input::install_owner_input_actor;
use crate::services::engine_runtime::health_port::{
    OwnerInputRuntimePort, RuntimeRegistryPort, install_runtime_registry_accessor,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

pub(crate) fn install_owner_actor() {
    install_owner_input_actor(&crate::services::discord::queue_io::input_owner::routine::ACTOR);
}

#[derive(Default)]
pub(crate) struct RuntimeRegistrySlot(Mutex<Option<Weak<HealthRegistry>>>);

impl RuntimeRegistrySlot {
    pub(crate) fn publish(&self, registry: &Arc<HealthRegistry>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(Arc::downgrade(registry));
        }
    }

    pub(crate) fn get(&self) -> Option<Arc<HealthRegistry>> {
        self.0.lock().ok()?.as_ref().and_then(Weak::upgrade)
    }

    pub(crate) fn port(&self) -> Option<Arc<dyn RuntimeRegistryPort>> {
        self.get()
            .map(|registry| registry as Arc<dyn RuntimeRegistryPort>)
    }
}

static REGISTRY: RuntimeRegistrySlot = RuntimeRegistrySlot(Mutex::new(None));

pub(crate) fn publish_runtime_registry(registry: &Arc<HealthRegistry>) {
    install_runtime_registry_accessor(registry_port);
    REGISTRY.publish(registry);
}

pub(crate) fn runtime_registry() -> Option<Arc<HealthRegistry>> {
    REGISTRY.get()
}

fn registry_port() -> Option<Arc<dyn RuntimeRegistryPort>> {
    REGISTRY.port()
}

#[cfg(test)]
pub(crate) struct RestoreRuntimeRegistry(Option<Weak<HealthRegistry>>);

#[cfg(test)]
impl RestoreRuntimeRegistry {
    pub(crate) fn capture() -> Self {
        Self(REGISTRY.0.lock().unwrap().clone())
    }
}

#[cfg(test)]
impl Drop for RestoreRuntimeRegistry {
    fn drop(&mut self) {
        *REGISTRY.0.lock().unwrap() = self.0.take();
    }
}

async fn owner_runtime(
    registry: &HealthRegistry,
    provider: &str,
    channel: u64,
) -> Result<(Arc<SharedData>, ChannelId), String> {
    let provider = ProviderKind::from_str(provider).ok_or("owner provider invalid")?;
    if channel == 0 {
        return Err("owner channel invalid".into());
    }
    let channel = ChannelId::new(channel);
    let shared = registry
        .shared_for_provider_on_channel(&provider, channel)
        .await
        .ok_or("owner runtime unavailable")?;
    Ok((shared, channel))
}

impl RuntimeRegistryPort for HealthRegistry {}

impl OwnerInputRuntimePort for HealthRegistry {
    fn owner_mailbox<'a>(
        &'a self,
        provider: &'a str,
        channel: u64,
    ) -> BoxFuture<'a, Result<ChannelMailboxHandle, String>> {
        Box::pin(async move {
            let (shared, channel) = owner_runtime(self, provider, channel).await?;
            ingress::adopt(&shared, &shared.provider, channel).await?;
            Ok(shared.mailbox(channel))
        })
    }

    fn owner_runtime_pool<'a>(
        &'a self,
        provider: &'a str,
        channel: u64,
    ) -> BoxFuture<'a, Result<PgPool, String>> {
        Box::pin(async move {
            let (shared, _) = owner_runtime(self, provider, channel).await?;
            shared
                .pg_pool
                .clone()
                .ok_or_else(|| "owner database unavailable".into())
        })
    }

    fn wake_owner<'a>(&'a self, provider: &'a str, channel: u64) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Ok((shared, channel)) = owner_runtime(self, provider, channel).await else {
                return;
            };
            #[cfg(unix)]
            (shared.router_ports.owner_wake)(&shared, channel);
            #[cfg(not(unix))]
            let _ = (shared, channel);
        })
    }
}
