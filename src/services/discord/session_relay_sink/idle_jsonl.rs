use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::cluster::session_matcher::MatchedChannel;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;

const MISMATCHED_INFLIGHT_LOG_THROTTLE: Duration = Duration::from_secs(60);
static MISMATCHED_INFLIGHT_LOGGED_AT: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

/// REAL loop ordering: classification gates run on the WHOLE payload FIRST (an
/// `init` event anywhere keeps the range relayable), the offset-authority dedup
/// SECOND. Extracting it makes the "init in committed prefix, suffix uncommitted"
/// black-hole regression testable without spinning the live poll loop.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum IdleRelayRangeAction {
    /// Classification dropped the range (grace window, user/tool-result event,
    /// ScheduleWakeup setup, or non-init active-session payload). Advance the
    /// offset past `end` without relaying.
    SkipClassified,
    /// The offset authority already covers `[start, end)` (`committed >= end`).
    /// Advance past `end` without relaying (dedup, whole range).
    SkipAlreadyRelayed,
    /// PARTIAL overlap (`start < committed < end`): the prefix was already relayed;
    /// relay ONLY the uncommitted `[committed, end)` suffix of THIS classified turn (not
    /// re-gated as a fresh non-init payload → no black-hole, codex r6 P1).
    SendSuffixFrom(u64),
    /// Nothing covered (`committed <= start`): relay the whole `[start, end)`.
    SendFull,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct IdleJsonlRelaySource {
    pub(super) path: String,
    pub(super) allow_continued_session_without_init: bool,
}

pub(super) fn idle_jsonl_relay_source_for_matched(
    matched: &MatchedChannel,
) -> IdleJsonlRelaySource {
    if matched.provider == ProviderKind::Codex
        && let Some(binding) = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
            &matched.expected_session_name,
        )
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
        path: matched.expected_rollout_path.clone(),
        allow_continued_session_without_init: false,
    }
}

pub(super) fn idle_jsonl_inflight_mismatches_session(
    inflight: &InflightTurnState,
    tmux_session_name: &str,
) -> bool {
    tmux_session_name.trim().is_empty()
        || inflight.tmux_session_name.as_deref() != Some(tmux_session_name)
}

pub(super) fn idle_jsonl_should_skip_mismatched_inflight(
    last_inflight_seen_at: &mut HashMap<String, Instant>,
    matched: &MatchedChannel,
    channel_id: u64,
    inflight: &InflightTurnState,
) -> bool {
    let tmux_session_name = &matched.expected_session_name;
    if !idle_jsonl_inflight_mismatches_session(inflight, tmux_session_name) {
        return false;
    }
    last_inflight_seen_at.remove(tmux_session_name);
    log_mismatched_inflight_skip(&matched.provider, channel_id, tmux_session_name, inflight);
    true
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
        channel = channel_id,
        tmux_session = %tmux_session_name,
        inflight_tmux_session = %inflight.tmux_session_name.as_deref().unwrap_or("(none)"),
        user_msg_id = inflight.user_msg_id,
        "idle JSONL relay skipped session because channel inflight belongs to another tmux session"
    );
}

/// Pure decision for the idle relay's classification + offset-authority dedup,
/// in the loop's real order. `payload` is the full `[start, end)` bytes.
/// `in_new_session_grace` mirrors the runtime `first_seen.elapsed() < grace`
/// gate. `committed` is the offset authority's `committed_relay_offset`.
pub(super) fn idle_relay_range_action(
    payload: &[u8],
    start: u64,
    end: u64,
    committed: u64,
    in_new_session_grace: bool,
    allow_continued_session_without_init: bool,
) -> IdleRelayRangeAction {
    // Classification first, on the WHOLE payload (matches the loop's gate
    // ordering at the top of `run_idle_jsonl_relay_loop`).
    if in_new_session_grace
        || idle_jsonl_payload_contains_user_event(payload)
        || idle_jsonl_payload_contains_schedule_wakeup_setup(payload)
        || (!allow_continued_session_without_init
            && !idle_jsonl_payload_contains_init_event(payload))
    {
        return IdleRelayRangeAction::SkipClassified;
    }
    // Offset-authority dedup second, on the already-classified range.
    if committed >= end {
        IdleRelayRangeAction::SkipAlreadyRelayed
    } else if committed > start {
        IdleRelayRangeAction::SendSuffixFrom(committed)
    } else {
        IdleRelayRangeAction::SendFull
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
