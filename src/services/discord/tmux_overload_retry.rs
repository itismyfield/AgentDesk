use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::provider::ProviderKind;

use super::SharedData;

pub(super) const PROVIDER_OVERLOAD_MAX_RETRIES: u8 = 3;

pub(super) static PROVIDER_OVERLOAD_RETRY_STATE: LazyLock<
    dashmap::DashMap<u64, ProviderOverloadRetryState>,
> = LazyLock::new(dashmap::DashMap::new);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProviderOverloadRetryState {
    pub(super) fingerprint: String,
    pub(super) attempts: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProviderOverloadDecision {
    Retry {
        attempt: u8,
        delay: std::time::Duration,
        fingerprint: String,
    },
    Exhausted,
}

pub(super) fn normalized_retry_payload_text(user_text: &str) -> &str {
    let trimmed = user_text.trim();
    if let Some((header, body)) = trimmed.split_once("\n\n") {
        if header.contains("이전 대화 복원") || header.contains("자동 재시도") {
            return body.trim();
        }
    }
    trimmed
}

pub(super) fn provider_overload_fingerprint(user_text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    normalized_retry_payload_text(user_text).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

pub(super) fn provider_overload_retry_delay(attempt: u8) -> std::time::Duration {
    let shift = u32::from(attempt.saturating_sub(1));
    std::time::Duration::from_secs(120 * (1u64 << shift))
}

pub(super) fn clear_provider_overload_retry_state(channel_id: ChannelId) {
    PROVIDER_OVERLOAD_RETRY_STATE.remove(&channel_id.get());
}

pub(super) fn record_provider_overload_retry(
    channel_id: ChannelId,
    user_text: &str,
) -> ProviderOverloadDecision {
    let fingerprint = provider_overload_fingerprint(user_text);
    let next_attempt = PROVIDER_OVERLOAD_RETRY_STATE
        .get(&channel_id.get())
        .and_then(|state| {
            if state.fingerprint == fingerprint {
                Some(state.attempts.saturating_add(1))
            } else {
                None
            }
        })
        .unwrap_or(1);

    if next_attempt > PROVIDER_OVERLOAD_MAX_RETRIES {
        clear_provider_overload_retry_state(channel_id);
        ProviderOverloadDecision::Exhausted
    } else {
        PROVIDER_OVERLOAD_RETRY_STATE.insert(
            channel_id.get(),
            ProviderOverloadRetryState {
                fingerprint: fingerprint.clone(),
                attempts: next_attempt,
            },
        );
        ProviderOverloadDecision::Retry {
            attempt: next_attempt,
            delay: provider_overload_retry_delay(next_attempt),
            fingerprint,
        }
    }
}

pub(super) fn schedule_provider_overload_retry(
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: ProviderKind,
    channel_id: ChannelId,
    user_message_id: serenity::MessageId,
    retry_text: String,
    attempt: u8,
    delay: std::time::Duration,
    fingerprint: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;

        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }

        let should_send = PROVIDER_OVERLOAD_RETRY_STATE
            .get(&channel_id.get())
            .map(|state| state.fingerprint == fingerprint && state.attempts == attempt)
            .unwrap_or(false);
        if !should_send {
            return;
        }

        if super::mailbox_has_active_turn(&shared, channel_id).await {
            clear_provider_overload_retry_state(channel_id);
            return;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ↻ watcher overload auto-retry: channel {} attempt {}/{} after {}s",
            channel_id.get(),
            attempt,
            PROVIDER_OVERLOAD_MAX_RETRIES,
            delay.as_secs()
        );
        super::turn_bridge::auto_retry_with_history(
            &http,
            &shared,
            &provider,
            channel_id,
            user_message_id,
            &retry_text,
        )
        .await;
    });
}

#[cfg(test)]
mod tests {
    use super::{
        PROVIDER_OVERLOAD_RETRY_STATE, ProviderOverloadDecision,
        clear_provider_overload_retry_state, normalized_retry_payload_text,
        provider_overload_fingerprint, provider_overload_retry_delay,
        record_provider_overload_retry,
    };
    use poise::serenity_prelude::ChannelId;

    #[test]
    fn retry_delay_is_exponential_backoff() {
        assert_eq!(provider_overload_retry_delay(1).as_secs(), 120);
        assert_eq!(provider_overload_retry_delay(2).as_secs(), 240);
        assert_eq!(provider_overload_retry_delay(3).as_secs(), 480);
    }

    #[test]
    fn retry_state_machine_escalates_then_exhausts() {
        let channel = ChannelId::new(999_000_378_001);
        clear_provider_overload_retry_state(channel);

        let text = "── dispatch ──\nDISPATCH:abc test task";

        let d1 = record_provider_overload_retry(channel, text);
        match &d1 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 1),
            _ => panic!("expected Retry, got {:?}", d1),
        }

        let d2 = record_provider_overload_retry(channel, text);
        match &d2 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 2),
            _ => panic!("expected Retry, got {:?}", d2),
        }

        let d3 = record_provider_overload_retry(channel, text);
        match &d3 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 3),
            _ => panic!("expected Retry, got {:?}", d3),
        }

        let d4 = record_provider_overload_retry(channel, text);
        assert_eq!(d4, ProviderOverloadDecision::Exhausted);
        assert!(!PROVIDER_OVERLOAD_RETRY_STATE.contains_key(&channel.get()));
    }

    #[test]
    fn retry_state_resets_on_different_fingerprint() {
        let channel = ChannelId::new(999_000_378_002);
        clear_provider_overload_retry_state(channel);

        let text_a = "first task payload";
        let text_b = "totally different payload";

        let d1 = record_provider_overload_retry(channel, text_a);
        match &d1 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 1),
            _ => panic!("expected Retry"),
        }

        let d2 = record_provider_overload_retry(channel, text_b);
        match &d2 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 1),
            _ => panic!("expected Retry after fingerprint change"),
        }

        clear_provider_overload_retry_state(channel);
    }

    #[test]
    fn clear_retry_state_removes_entry() {
        let channel = ChannelId::new(999_000_378_003);
        record_provider_overload_retry(channel, "some text");
        assert!(PROVIDER_OVERLOAD_RETRY_STATE.contains_key(&channel.get()));
        clear_provider_overload_retry_state(channel);
        assert!(!PROVIDER_OVERLOAD_RETRY_STATE.contains_key(&channel.get()));
    }

    #[test]
    fn normalized_payload_strips_retry_header() {
        let input = "⚠️ 자동 재시도 (2/3)\n\noriginal user message";
        assert_eq!(
            normalized_retry_payload_text(input),
            "original user message"
        );
    }

    #[test]
    fn normalized_payload_strips_history_restore_header() {
        let input = "📋 이전 대화 복원 중...\n\nactual prompt text";
        assert_eq!(normalized_retry_payload_text(input), "actual prompt text");
    }

    #[test]
    fn normalized_payload_keeps_plain_text() {
        let input = "just a normal message";
        assert_eq!(normalized_retry_payload_text(input), input);
    }

    #[test]
    fn fingerprint_stable_for_same_input() {
        let a = provider_overload_fingerprint("hello world");
        let b = provider_overload_fingerprint("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_differs_for_different_input() {
        let a = provider_overload_fingerprint("task A");
        let b = provider_overload_fingerprint("task B");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_normalizes_retry_headers() {
        let raw = "original message";
        let with_header = "⚠️ 자동 재시도 (1/3)\n\noriginal message";
        assert_eq!(
            provider_overload_fingerprint(raw),
            provider_overload_fingerprint(with_header)
        );
    }
}
