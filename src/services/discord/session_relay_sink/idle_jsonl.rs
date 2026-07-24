use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serenity::model::id::ChannelId;

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::discord::SharedData;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

const MISMATCHED_INFLIGHT_LOG_THROTTLE: Duration = Duration::from_secs(60);
static MISMATCHED_INFLIGHT_LOGGED_AT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

/// The idle cursor may advance only for intentional classification drops or a
/// committed frontier. Temporary ownership/grace suppression holds the range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdleJsonlSuppression {
    DropAndConsume,
    DeferUntilCommitted,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum IdleRelayRangeAction {
    DropAndConsume,
    HoldPending,
    AdvanceCommitted,
    SendPendingSuffixFrom(u64),
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum IdleJsonlInflightGateDecision {
    SuppressWithoutConsuming,
    DeferUntilCommitted,
}

pub(super) fn idle_jsonl_should_retry_without_dedup_shared<T>(
    shared_for_dedup: Option<&T>,
) -> bool {
    shared_for_dedup.is_none()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IdleJsonlSessionInitRearm {
    Keep,
    Clear,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct IdleJsonlRelaySource {
    pub(super) path: String,
    pub(super) allow_continued_session_without_init: bool,
}

pub(super) fn idle_jsonl_relay_source_for_matched(
    matched: &MatchedChannel,
) -> IdleJsonlRelaySource {
    idle_jsonl_relay_source(
        &matched.provider,
        &matched.expected_session_name,
        &matched.expected_rollout_path,
    )
}

fn idle_jsonl_relay_source(
    provider: &ProviderKind,
    session_name: &str,
    fallback_path: &str,
) -> IdleJsonlRelaySource {
    if *provider == ProviderKind::Codex
        && let Some(binding) =
            crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(session_name)
        && binding.runtime_kind == RuntimeHandoffKind::CodexTui
        && !binding.output_path.trim().is_empty()
        && std::path::Path::new(&binding.output_path).exists()
    {
        return IdleJsonlRelaySource {
            path: binding.output_path,
            allow_continued_session_without_init: true,
        };
    }

    IdleJsonlRelaySource {
        path: fallback_path.to_string(),
        allow_continued_session_without_init: false,
    }
}

pub(super) fn idle_jsonl_current_eof(provider: &ProviderKind, session_name: &str) -> Option<u64> {
    let fallback =
        crate::services::cluster::session_matcher::expected_rollout_path_for(session_name);
    let source = idle_jsonl_relay_source(provider, session_name, &fallback);
    std::fs::metadata(source.path)
        .ok()
        .map(|metadata| metadata.len())
}

pub(super) fn idle_jsonl_inflight_mismatches_session(
    inflight: &InflightTurnState,
    tmux_session_name: &str,
) -> bool {
    if tmux_session_name.trim().is_empty() {
        return true;
    }
    match inflight.tmux_session_name.as_deref() {
        Some(inflight_tmux_session) => inflight_tmux_session != tmux_session_name,
        None => true,
    }
}

pub(super) fn idle_jsonl_should_skip_mismatched_inflight(
    _last_inflight_seen_at: &mut HashMap<String, Instant>,
    matched: &MatchedChannel,
    channel_id: u64,
    inflight: &InflightTurnState,
) -> bool {
    let tmux_session_name = &matched.expected_session_name;
    if !idle_jsonl_inflight_mismatches_session(inflight, tmux_session_name) {
        return false;
    }
    log_mismatched_inflight_skip(&matched.provider, channel_id, tmux_session_name, inflight);
    true
}

pub(super) fn idle_jsonl_apply_active_inflight_gate(
    last_inflight_seen_at: &mut HashMap<String, Instant>,
    matched: &MatchedChannel,
    channel_id: u64,
    inflight: &InflightTurnState,
) -> IdleJsonlInflightGateDecision {
    if idle_jsonl_should_skip_mismatched_inflight(
        last_inflight_seen_at,
        matched,
        channel_id,
        inflight,
    ) {
        return IdleJsonlInflightGateDecision::SuppressWithoutConsuming;
    }
    last_inflight_seen_at.insert(matched.expected_session_name.clone(), Instant::now());
    IdleJsonlInflightGateDecision::DeferUntilCommitted
}

pub(super) fn idle_jsonl_session_has_init(
    session_init_seen: &mut HashSet<String>,
    tmux_session_name: &str,
    payload: &[u8],
) -> bool {
    if idle_jsonl_payload_contains_init_event(payload) {
        session_init_seen.insert(tmux_session_name.to_string());
        return true;
    }
    session_init_seen.contains(tmux_session_name)
}

pub(super) fn idle_jsonl_consume_offset(
    session_init_seen: &mut HashSet<String>,
    tmux_session_name: &str,
    offset: &mut u64,
    consumed_to: u64,
    rearm: IdleJsonlSessionInitRearm,
) {
    *offset = consumed_to;
    if rearm == IdleJsonlSessionInitRearm::Clear {
        session_init_seen.remove(tmux_session_name);
    }
}

pub(super) fn idle_jsonl_clear_session_init_on_generation_signature_change(
    session_init_seen: &mut HashSet<String>,
    session_generation_signatures: &mut HashMap<String, i64>,
    tmux_session_name: &str,
    current_generation_signature: i64,
) -> bool {
    let generation_changed = session_generation_signatures
        .insert(tmux_session_name.to_string(), current_generation_signature)
        .is_some_and(|previous_generation_signature| {
            previous_generation_signature != current_generation_signature
        });
    if generation_changed {
        session_init_seen.remove(tmux_session_name);
    }
    generation_changed
}

pub(super) fn idle_jsonl_clear_session_init_on_generation_reset(
    session_init_seen: &mut HashSet<String>,
    tmux_session_name: &str,
    generation_reset: bool,
) {
    if generation_reset {
        session_init_seen.remove(tmux_session_name);
    }
}

pub(super) async fn idle_jsonl_prepare_dedup_shared(
    health_registry: &HealthRegistry,
    matched: &MatchedChannel,
    channel: ChannelId,
    tmux_session_name: &str,
    len: u64,
    session_init_seen: &mut HashSet<String>,
) -> Option<Arc<SharedData>> {
    let shared_for_dedup = health_registry
        .shared_for_provider_on_channel(&matched.provider, channel)
        .await
        .or(health_registry.shared_for_provider(&matched.provider).await);
    if let Some(shared) = shared_for_dedup.as_ref() {
        super::super::tmux::reset_stale_relay_watermark_if_output_regressed(
            shared.as_ref(),
            channel,
            tmux_session_name,
            len,
            "idle_jsonl_relay",
        );
        let generation_reset = super::super::tmux::reset_relay_watermark_on_generation_change(
            shared.as_ref(),
            channel,
            tmux_session_name,
            "idle_jsonl_relay",
        );
        idle_jsonl_clear_session_init_on_generation_reset(
            session_init_seen,
            tmux_session_name,
            generation_reset,
        );
    }
    shared_for_dedup
}

pub(super) fn prune_idle_jsonl_session_state(
    seen_sessions: &HashSet<String>,
    offsets: &mut HashMap<String, u64>,
    first_seen_at: &mut HashMap<String, Instant>,
    last_inflight_seen_at: &mut HashMap<String, Instant>,
    session_init_seen: &mut HashSet<String>,
    session_generation_signatures: &mut HashMap<String, i64>,
    pending_ends: &mut HashMap<String, u64>,
) {
    offsets.retain(|session, _| seen_sessions.contains(session));
    first_seen_at.retain(|session, _| seen_sessions.contains(session));
    last_inflight_seen_at.retain(|session, _| seen_sessions.contains(session));
    session_init_seen.retain(|session| seen_sessions.contains(session));
    session_generation_signatures.retain(|session, _| seen_sessions.contains(session));
    pending_ends.retain(|session, _| seen_sessions.contains(session));
    prune_mismatched_inflight_log_sessions(seen_sessions);
}

pub(super) fn prune_mismatched_inflight_log_sessions(seen_sessions: &HashSet<String>) {
    let Some(logged_at) = MISMATCHED_INFLIGHT_LOGGED_AT.get() else {
        return;
    };
    let Ok(mut logged_at) = logged_at.lock() else {
        return;
    };
    logged_at.retain(|session, _| seen_sessions.contains(session));
}

fn log_mismatched_inflight_skip(
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_name: &str,
    inflight: &InflightTurnState,
) {
    let logged_at = MISMATCHED_INFLIGHT_LOGGED_AT.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut logged_at) = logged_at.lock() else {
        return;
    };
    if let Some(last_logged_at) = logged_at.get_mut(tmux_session_name) {
        if last_logged_at.elapsed() < MISMATCHED_INFLIGHT_LOG_THROTTLE {
            return;
        }
        *last_logged_at = Instant::now();
    } else {
        logged_at.insert(tmux_session_name.to_string(), Instant::now());
    }
    tracing::debug!(
        provider = provider.as_str(),
        channel_id,
        tmux_session = %tmux_session_name,
        inflight_tmux_session = %inflight.tmux_session_name.as_deref().unwrap_or("(none)"),
        user_msg_id = inflight.user_msg_id,
        "idle JSONL relay skipped session because channel inflight belongs to another tmux session"
    );
}

pub(super) fn idle_jsonl_suppressed_range_action(
    committed: u64,
    _start: u64,
    end: u64,
    suppression: IdleJsonlSuppression,
) -> IdleRelayRangeAction {
    if suppression == IdleJsonlSuppression::DropAndConsume {
        return IdleRelayRangeAction::DropAndConsume;
    }
    if committed >= end {
        IdleRelayRangeAction::AdvanceCommitted
    } else {
        IdleRelayRangeAction::HoldPending
    }
}

/// Pure decision for the idle relay's intentional classification drops and
/// current-generation committed-frontier dedup. Temporary grace suppression is
/// handled separately by [`idle_jsonl_suppressed_range_action`].
pub(super) fn idle_relay_range_action(
    payload: &[u8],
    start: u64,
    end: u64,
    committed: u64,
    allow_continued_session_without_init: bool,
    session_init_seen: bool,
    was_deferred: bool,
) -> IdleRelayRangeAction {
    if idle_jsonl_payload_contains_schedule_wakeup_setup(payload)
        || (!was_deferred && idle_jsonl_payload_contains_user_event(payload))
        || (!was_deferred
            && !allow_continued_session_without_init
            && !session_init_seen
            && !idle_jsonl_payload_contains_init_event(payload))
    {
        return IdleRelayRangeAction::DropAndConsume;
    }
    if committed >= end {
        IdleRelayRangeAction::AdvanceCommitted
    } else {
        IdleRelayRangeAction::SendPendingSuffixFrom(committed.max(start))
    }
}

pub(super) fn read_jsonl_range(path: &str, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut payload = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut payload)?;
    Ok(payload)
}

pub(super) fn idle_jsonl_payload_contains_user_event(payload: &[u8]) -> bool {
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("user") {
            return true;
        }
    }
    false
}

pub(super) fn idle_jsonl_payload_contains_schedule_wakeup_setup(payload: &[u8]) -> bool {
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if jsonl_event_contains_schedule_wakeup_setup_reference(&value) {
            return true;
        }
    }
    false
}

fn jsonl_event_contains_schedule_wakeup_setup_reference(value: &serde_json::Value) -> bool {
    match value.get("type").and_then(serde_json::Value::as_str) {
        Some("assistant") => assistant_event_contains_schedule_wakeup_reference(value),
        Some("result") => value
            .get("result")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains("ScheduleWakeup")),
        _ => false,
    }
}

fn assistant_event_contains_schedule_wakeup_reference(value: &serde_json::Value) -> bool {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    content.iter().any(|item| {
        let item_type = item.get("type").and_then(serde_json::Value::as_str);
        match item_type {
            Some("tool_use") => {
                item.get("name").and_then(serde_json::Value::as_str) == Some("ScheduleWakeup")
            }
            Some("text") => item
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains("ScheduleWakeup")),
            _ => false,
        }
    })
}

pub(super) fn idle_jsonl_payload_contains_init_event(payload: &[u8]) -> bool {
    for line in String::from_utf8_lossy(payload).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if value.get("type").and_then(serde_json::Value::as_str) == Some("system")
            && value.get("subtype").and_then(serde_json::Value::as_str) == Some("init")
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_inflight_log_prune_drops_unseen_sessions() {
        let logged_at = MISMATCHED_INFLIGHT_LOGGED_AT.get_or_init(|| Mutex::new(HashMap::new()));
        {
            let mut logged_at = logged_at.lock().expect("logged_at lock");
            logged_at.insert("session-prune-seen".to_string(), Instant::now());
            logged_at.insert("session-prune-gone".to_string(), Instant::now());
        }

        let mut seen_sessions = HashSet::new();
        seen_sessions.insert("session-prune-seen".to_string());
        prune_mismatched_inflight_log_sessions(&seen_sessions);

        let mut logged_at = logged_at.lock().expect("logged_at lock");
        assert!(logged_at.contains_key("session-prune-seen"));
        assert!(!logged_at.contains_key("session-prune-gone"));
        logged_at.remove("session-prune-seen");
    }

    #[test]
    fn idle_jsonl_missing_dedup_shared_retries_without_send_or_consume() {
        let session_name = "AgentDesk-claude-4164-none-shared";
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s4164\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"wake answer\"}]}}\n"
        )
        .as_bytes();
        let mut session_init_seen = HashSet::new();
        let mut offset = 128u64;
        let start = offset;
        let end = start + payload.len() as u64;
        let shared_for_dedup: Option<&()> = None;
        let mut send_attempts = 0;

        let session_has_init =
            idle_jsonl_session_has_init(&mut session_init_seen, session_name, payload);
        assert!(session_has_init);
        assert_eq!(
            idle_relay_range_action(payload, start, end, 0, false, session_has_init, false),
            IdleRelayRangeAction::SendPendingSuffixFrom(start),
            "without the missing-shared gate this eligible range would fall through to send"
        );

        let retry_without_consuming =
            idle_jsonl_should_retry_without_dedup_shared(shared_for_dedup);
        assert!(retry_without_consuming);
        if !retry_without_consuming
            && idle_relay_range_action(payload, start, end, 0, false, session_has_init, false)
                == IdleRelayRangeAction::SendPendingSuffixFrom(start)
        {
            send_attempts += 1;
            idle_jsonl_consume_offset(
                &mut session_init_seen,
                session_name,
                &mut offset,
                end,
                IdleJsonlSessionInitRearm::Keep,
            );
        }

        assert_eq!(send_attempts, 0, "None-shared window must not enqueue");
        assert_eq!(offset, start, "None-shared window must leave cursor intact");
        assert!(
            session_init_seen.contains(session_name),
            "retry keeps the init marker for the next idle tick"
        );
    }

    #[test]
    fn idle_jsonl_shared_dedup_sends_range_once_then_skips_committed_retry() {
        use std::sync::atomic::Ordering;

        let _authority =
            crate::services::discord::outbound::delivery_record::authority_test_seam::force(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(4_164);
        let session_name = "AgentDesk-claude-4164-shared";
        let payload = concat!(
            "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s4164\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"wake answer\"}]}}\n"
        )
        .as_bytes();
        let start = 0u64;
        let end = payload.len() as u64;
        let mut offset = start;
        let mut duplicate_actor_offset = start;
        let mut session_init_seen = HashSet::new();
        let mut send_attempts = 0;

        assert!(!idle_jsonl_should_retry_without_dedup_shared(Some(
            shared.as_ref()
        )));
        let session_has_init =
            idle_jsonl_session_has_init(&mut session_init_seen, session_name, payload);
        let committed =
            crate::services::discord::outbound::delivery_record::effective_committed_offset(
                shared.as_ref(),
                &provider,
                channel,
                session_name,
                Some(end),
            );
        assert_eq!(committed, 0);
        match idle_relay_range_action(
            payload,
            start,
            end,
            committed,
            false,
            session_has_init,
            false,
        ) {
            IdleRelayRangeAction::SendPendingSuffixFrom(from) => {
                assert_eq!(from, start);
                send_attempts += 1;
            }
            other => panic!("first shared pass must send, got {other:?}"),
        }
        assert_eq!(
            offset, start,
            "enqueue acceptance is not a confirmed commit and must not consume the cursor"
        );

        shared
            .tmux_relay_coord(channel)
            .confirmed_end_offset
            .store(end, Ordering::Release);
        let committed =
            crate::services::discord::outbound::delivery_record::effective_committed_offset(
                shared.as_ref(),
                &provider,
                channel,
                session_name,
                Some(end),
            );
        assert_eq!(committed, end);
        match idle_relay_range_action(
            payload,
            start,
            end,
            committed,
            false,
            session_init_seen.contains(session_name),
            false,
        ) {
            IdleRelayRangeAction::AdvanceCommitted => {
                idle_jsonl_consume_offset(
                    &mut session_init_seen,
                    session_name,
                    &mut duplicate_actor_offset,
                    end,
                    IdleJsonlSessionInitRearm::Keep,
                );
            }
            other => panic!("committed replay must dedup-skip, got {other:?}"),
        }

        assert_eq!(
            send_attempts, 1,
            "shared dedup sends the range exactly once"
        );
        assert_eq!(offset, start);
        assert_eq!(duplicate_actor_offset, end);
    }

    #[test]
    fn deferred_active_range_ignores_user_prefix_and_relays_uncommitted_tail() {
        let payload = concat!(
            "{\"type\":\"user\",\"message\":{\"content\":\"wake\"}}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"wake answer\"}]}}\n",
            "{\"type\":\"result\",\"result\":\"wake answer\"}\n"
        )
        .as_bytes();
        assert_eq!(
            idle_relay_range_action(payload, 100, 300, 180, false, false, true),
            IdleRelayRangeAction::SendPendingSuffixFrom(180),
            "the user event explains why the range was deferred; it must not discard the assistant tail after grace"
        );
        assert_eq!(
            idle_relay_range_action(payload, 100, 300, 180, false, false, false),
            IdleRelayRangeAction::DropAndConsume,
            "a fresh active-turn classification still intentionally drops the range"
        );
    }

    #[test]
    fn temporary_suppression_holds_until_confirmed_commit() {
        let start = 128;
        let end = 256;

        assert_eq!(
            idle_jsonl_suppressed_range_action(
                127,
                start,
                end,
                IdleJsonlSuppression::DeferUntilCommitted,
            ),
            IdleRelayRangeAction::HoldPending
        );
        assert_eq!(
            idle_jsonl_suppressed_range_action(
                end,
                start,
                end,
                IdleJsonlSuppression::DeferUntilCommitted,
            ),
            IdleRelayRangeAction::AdvanceCommitted
        );
        assert_eq!(
            idle_jsonl_suppressed_range_action(0, start, end, IdleJsonlSuppression::DropAndConsume,),
            IdleRelayRangeAction::DropAndConsume
        );
    }
}
