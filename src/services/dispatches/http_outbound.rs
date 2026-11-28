//! Engine producers' reqwest delivery to the Discord API, through the adapter the Discord layer installs.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use crate::domain::outbound::port::{DispatchMessagePostError, DispatchMessagePostErrorKind};
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

pub(crate) type Edit = for<'a> fn(
    HttpOutbound,
    &'a str,
    &'a str,
    &'a str,
) -> Pin<
    Box<dyn Future<Output = Result<String, DispatchMessagePostError>> + Send + 'a>,
>;

struct HttpOutboundPort {
    deliver: Deliver,
    edit: Edit,
}

static PORT: OnceLock<HttpOutboundPort> = OnceLock::new();

/// Installs the Discord adapter before any command runs; the first install wins.
pub(crate) fn install(deliver: Deliver, edit: Edit) {
    let _ = PORT.set(HttpOutboundPort { deliver, edit });
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
        match PORT.get() {
            Some(port) => (port.deliver)(self.clone(), intent, summary).await,
            None => DeliveryResult::PermanentFailure {
                reason: "discord http outbound not installed".to_string(),
            },
        }
    }

    pub(crate) async fn edit_message(
        self,
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<String, DispatchMessagePostError> {
        let port = PORT.get().ok_or_else(|| {
            DispatchMessagePostError::new(
                DispatchMessagePostErrorKind::Other,
                "discord http outbound not installed".to_string(),
            )
        })?;
        (port.edit)(self, channel_id, message_id, content).await
    }
}

impl OutboundDelivery for HttpOutbound {
    fn deliver(&self, intent: OutboundIntent) -> impl Future<Output = DeliveryResult> + Send {
        self.deliver_with_summary(intent, None)
    }
}
