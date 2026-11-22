//! Engine producers' reqwest delivery to the Discord API, through the adapter the Discord layer installs.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use crate::domain::outbound::{DeliveryResult, OutboundDelivery, OutboundIntent};

/// One bot token's connection to the Discord API.
#[derive(Clone)]
pub(crate) struct HttpOutbound {
    pub(crate) client: reqwest::Client,
    pub(crate) token: String,
    pub(crate) discord_api_base: String,
}

/// The Discord layer's delivery over the process-wide deduper; `Option<String>` is the compact summary.
pub(crate) type Deliver = fn(
    HttpOutbound,
    OutboundIntent,
    Option<String>,
) -> Pin<Box<dyn Future<Output = DeliveryResult> + Send>>;

static DELIVER: OnceLock<Deliver> = OnceLock::new();

/// Installs the Discord adapter before any command runs; the first install wins.
pub(crate) fn install(deliver: Deliver) {
    let _ = DELIVER.set(deliver);
}

impl HttpOutbound {
    pub(crate) fn new(
        client: reqwest::Client,
        token: impl Into<String>,
        discord_api_base: impl Into<String>,
    ) -> Self {
        Self {
            client,
            token: token.into(),
            discord_api_base: discord_api_base.into(),
        }
    }

    /// `summary` is the compact body the policy may fall back to.
    pub(crate) async fn deliver_with_summary(
        &self,
        intent: OutboundIntent,
        summary: Option<String>,
    ) -> DeliveryResult {
        #[cfg(test)]
        crate::services::discord::outbound::install_engine_ports();
        match DELIVER.get() {
            Some(deliver) => deliver(self.clone(), intent, summary).await,
            None => DeliveryResult::PermanentFailure {
                reason: "discord http outbound not installed".to_string(),
            },
        }
    }
}

impl OutboundDelivery for HttpOutbound {
    fn deliver(&self, intent: OutboundIntent) -> impl Future<Output = DeliveryResult> + Send {
        self.deliver_with_summary(intent, None)
    }
}
