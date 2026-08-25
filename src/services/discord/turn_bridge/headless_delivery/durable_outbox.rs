use super::*;

pub(super) const HEADLESS_DELIVERY_OUTBOX_VISIBLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);
const HEADLESS_DELIVERY_OUTBOX_VISIBLE_POLL: std::time::Duration =
    std::time::Duration::from_millis(100);

const DURABLE_HEADLESS_OUTBOX_ENV: &str = "AGENTDESK_HEADLESS_DURABLE_OUTBOX";
pub(super) const MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct HeadlessSessionRoutingKey<'a, const MAX_BYTES: usize>(&'a str);

impl<'a, const MAX_BYTES: usize> HeadlessSessionRoutingKey<'a, MAX_BYTES> {
    fn new(value: &'a str) -> Option<Self> {
        (!value.trim().is_empty() && value.len() <= MAX_BYTES && !value.contains('\0'))
            .then_some(Self(value))
    }

    fn as_str(self) -> &'a str {
        self.0
    }
}

/// Content-free durable identity: provider, channel, owner, and generation.
/// The optional raw session key remains routing data, not tuple identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DurableHeadlessOutboxIdentity<'a> {
    provider: &'a str,
    channel_id: u64,
    owning_user_msg_id: MessageId,
    born_generation: u64,
}

impl<'a> DurableHeadlessOutboxIdentity<'a> {
    pub(super) fn new(
        provider: &'a ProviderKind,
        channel_id: u64,
        owning_user_msg_id: Option<MessageId>,
        born_generation: u64,
    ) -> Option<Self> {
        let provider = provider.is_supported().then(|| provider.as_str())?;
        (channel_id != 0 && born_generation != 0).then_some(())?;
        Some(Self {
            provider,
            channel_id,
            owning_user_msg_id: owning_user_msg_id?,
            born_generation,
        })
    }

    fn persistent_dedupe_key(self) -> String {
        format!(
            "headless_delivery:v2|p={}:{}|c={}|u={}|g={}",
            self.provider.len(),
            self.provider,
            self.channel_id,
            self.owning_user_msg_id.get(),
            self.born_generation,
        )
    }
}

pub(super) async fn wait_for_headless_delivery_outbox_visible(
    pool: &sqlx::PgPool,
    outbox_id: i64,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let row = sqlx::query("SELECT status, error FROM message_outbox WHERE id = $1")
            .bind(outbox_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                format!("poll headless delivery outbox row {outbox_id} failed: {error}")
            })?;
        let Some(row) = row else {
            return Err(format!(
                "headless delivery outbox row {outbox_id} disappeared before visible delivery"
            ));
        };
        let status: String = row
            .try_get("status")
            .map_err(|error| format!("read headless outbox row {outbox_id} status: {error}"))?;
        match status.as_str() {
            "sent" => return Ok(()),
            "failed" => {
                let error: Option<String> = row.try_get("error").ok().flatten();
                return Err(format!(
                    "headless delivery outbox row {outbox_id} failed before visible delivery: {}",
                    error.unwrap_or_else(|| "unknown error".to_string())
                ));
            }
            _ => {}
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "headless delivery outbox row {outbox_id} remained {status} for {}s before visible delivery",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(HEADLESS_DELIVERY_OUTBOX_VISIBLE_POLL.min(deadline - now)).await;
    }
}

fn durable_headless_outbox_enabled_value(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes" | "enable" | "enabled"
        )
    })
}

fn durable_headless_outbox_enabled() -> bool {
    let value = std::env::var(DURABLE_HEADLESS_OUTBOX_ENV).ok();
    durable_headless_outbox_enabled_value(value.as_deref())
}

pub(super) async fn enqueue_headless_outbox(
    pool: &sqlx::PgPool,
    message: crate::services::message_outbox::OutboxMessage<'_>,
    owning_user_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    born_generation: u64,
    cancel_token: Option<&CancelToken>,
) -> Result<
    crate::services::message_outbox::OutboxEnqueueOutcome,
    crate::services::message_outbox::OutboxEnqueueError,
> {
    enqueue_headless_outbox_with_rollout(
        pool,
        message,
        owning_user_msg_id,
        provider,
        born_generation,
        cancel_token,
        durable_headless_outbox_enabled(),
    )
    .await
}

pub(super) async fn enqueue_headless_outbox_with_rollout(
    pool: &sqlx::PgPool,
    message: crate::services::message_outbox::OutboxMessage<'_>,
    owning_user_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    born_generation: u64,
    cancel_token: Option<&CancelToken>,
    durable_enabled: bool,
) -> Result<
    crate::services::message_outbox::OutboxEnqueueOutcome,
    crate::services::message_outbox::OutboxEnqueueError,
> {
    let channel_id = message
        .target
        .strip_prefix("channel:")
        .and_then(|value| value.parse().ok());
    let bounded_session_key = message
        .session_key
        .and_then(HeadlessSessionRoutingKey::<MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES>::new);
    let identity = durable_enabled
        .then(|| {
            channel_id.and_then(|channel_id| {
                DurableHeadlessOutboxIdentity::new(
                    provider,
                    channel_id,
                    owning_user_msg_id,
                    born_generation,
                )
            })
        })
        .flatten();
    let identity =
        identity.filter(|_| message.session_key.is_none() || bounded_session_key.is_some());
    let Some(identity) = identity else {
        return crate::services::message_outbox::enqueue_outbox_pg_returning_outcome_with_ttl_and_cancel(
            pool,
            message,
            0,
            cancel_token,
        )
        .await;
    };

    crate::services::message_outbox::enqueue_outbox_pg_returning_outcome_with_exact_dedupe_and_cancel(
        pool,
        message,
        &identity.persistent_dedupe_key(),
        cancel_token,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_rollout_defaults_off_and_requires_an_explicit_true_value() {
        assert!(!durable_headless_outbox_enabled_value(None));
        for value in ["", "0", "false", "off", "unexpected"] {
            assert!(!durable_headless_outbox_enabled_value(Some(value)));
        }
        for value in ["1", "true", "TRUE", "on", "yes", "enabled"] {
            assert!(durable_headless_outbox_enabled_value(Some(value)));
        }
    }

    #[test]
    fn exact_identity_preserves_session_bytes_and_excludes_content() {
        let key = "  claude/host:AgentDesk-5191  ";
        let identity = DurableHeadlessOutboxIdentity::new(
            &ProviderKind::Claude,
            5191,
            Some(MessageId::new(8)),
            7,
        )
        .expect("complete exact identity");
        assert_eq!(
            HeadlessSessionRoutingKey::<MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES>::new(key)
                .unwrap()
                .as_str(),
            key
        );
        let dedupe_key = identity.persistent_dedupe_key();
        assert!(dedupe_key.contains("|p=6:claude|c=5191|u=8|g=7"));
        assert!(!dedupe_key.contains(key));
    }

    #[test]
    fn exact_identity_key_changes_on_each_required_axis() {
        let owner = Some(MessageId::new(8));
        let base = DurableHeadlessOutboxIdentity::new(&ProviderKind::Claude, 5191, owner, 7)
            .unwrap()
            .persistent_dedupe_key();
        for changed in [
            DurableHeadlessOutboxIdentity::new(&ProviderKind::Codex, 5191, owner, 7)
                .unwrap()
                .persistent_dedupe_key(),
            DurableHeadlessOutboxIdentity::new(&ProviderKind::Claude, 5192, owner, 7)
                .unwrap()
                .persistent_dedupe_key(),
            DurableHeadlessOutboxIdentity::new(
                &ProviderKind::Claude,
                5191,
                Some(MessageId::new(9)),
                7,
            )
            .unwrap()
            .persistent_dedupe_key(),
            DurableHeadlessOutboxIdentity::new(&ProviderKind::Claude, 5191, owner, 8)
                .unwrap()
                .persistent_dedupe_key(),
        ] {
            assert_ne!(changed, base);
        }
    }

    #[test]
    fn exact_identity_requires_owner_and_session_routing_key_is_bounded() {
        assert!(DurableHeadlessOutboxIdentity::new(&ProviderKind::Claude, 5191, None, 7).is_none());
        for invalid in ["", "   ", "bad\0key"] {
            assert!(
                HeadlessSessionRoutingKey::<MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES>::new(invalid)
                    .is_none()
            );
        }
        assert!(
            HeadlessSessionRoutingKey::<MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES>::new(
                &"x".repeat(MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES)
            )
            .is_some()
        );
        let oversized = "x".repeat(MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES + 1);
        assert!(
            HeadlessSessionRoutingKey::<MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES>::new(&oversized)
                .is_none()
        );
    }
}
