use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use crate::services::agent_protocol::RuntimeHandoffKind;

const PENDING_PROMPT_TTL: Duration = Duration::from_secs(10);
const RECENT_OBSERVED_TTL: Duration = Duration::from_secs(30);
const SESSION_MAPPING_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const PROMPT_ANCHOR_TTL: Duration = Duration::from_secs(30 * 60);
// Short window matching how long a Discord notify await + transcript flush
// can plausibly take before `record_prompt_anchor` lands. 60s is generous;
// the marker is also cleared explicitly when an anchor is consumed.
const SSH_DIRECT_OBSERVATION_TTL: Duration = Duration::from_secs(60);
const OBSERVED_PROMPT_BUFFER: usize = 128;

static STATE: LazyLock<Mutex<TuiPromptDedupeState>> =
    LazyLock::new(|| Mutex::new(TuiPromptDedupeState::default()));
static OBSERVED_PROMPTS: LazyLock<broadcast::Sender<ObservedTuiPrompt>> =
    LazyLock::new(|| broadcast::channel(OBSERVED_PROMPT_BUFFER).0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedTuiPrompt {
    pub provider: String,
    pub tmux_session_name: String,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TuiPromptAnchor {
    pub channel_id: u64,
    pub message_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TuiRuntimeBinding {
    pub runtime_kind: RuntimeHandoffKind,
    pub output_path: String,
    pub relay_output_path: Option<String>,
    pub input_fifo_path: Option<String>,
    pub session_id: Option<String>,
    pub last_offset: u64,
    pub relay_last_offset: Option<u64>,
}

impl TuiRuntimeBinding {
    pub(crate) fn relay_output_path(&self) -> &str {
        self.relay_output_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(&self.output_path)
    }

    pub(crate) fn relay_last_offset(&self) -> u64 {
        self.relay_last_offset.unwrap_or(self.last_offset)
    }
}

#[derive(Clone, Debug)]
struct TimedValue<T> {
    value: T,
    recorded_at: Instant,
}

#[derive(Default)]
struct TuiPromptDedupeState {
    pending_by_tmux: HashMap<PromptKey, VecDeque<TimedValue<String>>>,
    recent_observed_by_tmux: HashMap<PromptKey, VecDeque<TimedValue<String>>>,
    tmux_by_provider_session: HashMap<PromptKey, TimedValue<String>>,
    channel_by_tmux: HashMap<String, TimedValue<u64>>,
    runtime_by_tmux: HashMap<String, TimedValue<TuiRuntimeBinding>>,
    prompt_anchor_by_tmux: HashMap<PromptKey, TimedValue<TuiPromptAnchor>>,
    // Short-lived marker set the moment an SSH-direct prompt is observed,
    // closing the window before `record_prompt_anchor` runs (the latter has
    // to wait for the Discord notify await to land).
    ssh_direct_observation_by_tmux: HashMap<PromptKey, TimedValue<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PromptKey {
    provider: String,
    key: String,
}

impl PromptKey {
    fn new(provider: &str, key: &str) -> Self {
        Self {
            provider: normalize_provider(provider),
            key: key.trim().to_string(),
        }
    }
}

pub fn subscribe_observed_prompts() -> broadcast::Receiver<ObservedTuiPrompt> {
    OBSERVED_PROMPTS.subscribe()
}

pub fn register_provider_session(
    provider: &str,
    provider_session_id: &str,
    tmux_session_name: &str,
) {
    let provider_session_id = provider_session_id.trim();
    let tmux_session_name = tmux_session_name.trim();
    if provider_session_id.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.tmux_by_provider_session.insert(
        PromptKey::new(provider, provider_session_id),
        TimedValue {
            value: tmux_session_name.to_string(),
            recorded_at: Instant::now(),
        },
    );
}

pub fn register_tmux_channel(tmux_session_name: &str, channel_id: u64) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || channel_id == 0 {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.channel_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: channel_id,
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn register_tmux_runtime_binding(tmux_session_name: &str, binding: TuiRuntimeBinding) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || binding.output_path.trim().is_empty() {
        return;
    }
    if binding.relay_output_path().trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.runtime_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: binding,
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn register_rehydrated_tmux_runtime_binding(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    binding: TuiRuntimeBinding,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty()
        || tmux_session_name.is_empty()
        || channel_id == 0
        || binding.output_path.trim().is_empty()
        || binding.relay_output_path().trim().is_empty()
    {
        return;
    }
    let session_id = binding.session_id.clone();
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.runtime_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: binding,
            recorded_at: Instant::now(),
        },
    );
    state.channel_by_tmux.insert(
        tmux_session_name.to_string(),
        TimedValue {
            value: channel_id,
            recorded_at: Instant::now(),
        },
    );
    if let Some(session_id) = session_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        state.tmux_by_provider_session.insert(
            PromptKey::new(&provider, session_id),
            TimedValue {
                value: tmux_session_name.to_string(),
                recorded_at: Instant::now(),
            },
        );
    }
}

pub fn owner_channel_for_tmux_session(tmux_session_name: &str) -> Option<u64> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .channel_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value)
}

pub(crate) fn record_prompt_anchor(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
    message_id: u64,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 || message_id == 0 {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.prompt_anchor_by_tmux.insert(
        PromptKey::new(&provider, tmux_session_name),
        TimedValue {
            value: TuiPromptAnchor {
                channel_id,
                message_id,
            },
            recorded_at: Instant::now(),
        },
    );
}

pub(crate) fn take_prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<TuiPromptAnchor> {
    let anchor = prompt_anchor_for_response(provider, tmux_session_name, channel_id)?;
    clear_prompt_anchor_for_response(provider, tmux_session_name, anchor);
    Some(anchor)
}

pub(crate) fn prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<TuiPromptAnchor> {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || channel_id == 0 {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&provider, tmux_session_name);
    let anchor = state.prompt_anchor_by_tmux.get(&key)?.value;
    if anchor.channel_id != channel_id {
        return None;
    }
    Some(anchor)
}

pub(crate) fn clear_prompt_anchor_for_response(
    provider: &str,
    tmux_session_name: &str,
    anchor: TuiPromptAnchor,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let removed = {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        state.purge_expired();
        let key = PromptKey::new(&provider, tmux_session_name);
        let Some(current) = state
            .prompt_anchor_by_tmux
            .get(&key)
            .map(|entry| entry.value)
        else {
            return false;
        };
        if current != anchor {
            return false;
        }
        state.prompt_anchor_by_tmux.remove(&key);
        true
    };
    if removed {
        clear_ssh_direct_observation_pending(&provider, tmux_session_name);
    }
    removed
}

pub(crate) fn runtime_binding_for_tmux_session(
    tmux_session_name: &str,
) -> Option<TuiRuntimeBinding> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .runtime_by_tmux
        .get(tmux_session_name)
        .map(|entry| entry.value.clone())
}

pub(crate) fn runtime_bindings_for_kind(
    runtime_kind: RuntimeHandoffKind,
) -> Vec<(String, TuiRuntimeBinding)> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .runtime_by_tmux
        .iter()
        .filter(|(_, entry)| entry.value.runtime_kind == runtime_kind)
        .map(|(tmux_session_name, entry)| (tmux_session_name.clone(), entry.value.clone()))
        .collect()
}

pub(crate) fn advance_tmux_runtime_binding_offset(
    tmux_session_name: &str,
    output_path: &str,
    last_offset: u64,
) -> bool {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || output_path.trim().is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let Some(entry) = state.runtime_by_tmux.get_mut(tmux_session_name) else {
        return false;
    };
    if entry.value.output_path == output_path {
        entry.value.last_offset = last_offset;
        if entry.value.relay_output_path.is_none() {
            entry.value.relay_last_offset = Some(last_offset);
        }
        entry.recorded_at = Instant::now();
        return true;
    }
    if entry.value.relay_output_path.as_deref() != Some(output_path) {
        return false;
    }
    entry.value.relay_last_offset = Some(last_offset);
    entry.recorded_at = Instant::now();
    true
}

pub fn record_discord_originated_prompt(provider: &str, tmux_session_name: &str, prompt: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() || prompt.trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .pending_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default()
        .push_back(TimedValue {
            value: prompt.to_string(),
            recorded_at: Instant::now(),
        });
}

pub fn remove_discord_originated_prompt(provider: &str, tmux_session_name: &str, prompt: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let Some(queue) = state.pending_by_tmux.get_mut(&key) else {
        return;
    };
    if let Some(index) = queue
        .iter()
        .position(|pending| prompts_match(&pending.value, prompt))
    {
        queue.remove(index);
    }
    if queue.is_empty() {
        state.pending_by_tmux.remove(&key);
    }
}

pub fn observe_prompt_by_provider_session(
    provider: &str,
    provider_session_id: &str,
    prompt: &str,
) -> PromptObservation {
    let tmux_session_name = resolve_tmux_session_name(provider, provider_session_id)
        .unwrap_or_else(|| provider_session_id.trim().to_string());
    observe_prompt_by_tmux(provider, &tmux_session_name, prompt)
}

pub fn observe_prompt_by_tmux(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> PromptObservation {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || prompt.trim().is_empty() {
        return PromptObservation::Ignored;
    }
    if take_matching_pending_prompt(&provider, tmux_session_name, prompt) {
        return PromptObservation::SuppressedDiscordDuplicate;
    }
    if take_or_record_recent_observed_prompt(&provider, tmux_session_name, prompt) {
        return PromptObservation::SuppressedRecentDuplicate;
    }
    mark_ssh_direct_observation_pending(&provider, tmux_session_name);
    let event = ObservedTuiPrompt {
        provider,
        tmux_session_name: tmux_session_name.to_string(),
        prompt: prompt.to_string(),
    };
    let _ = OBSERVED_PROMPTS.send(event);
    PromptObservation::PublishedSshDirect
}

fn mark_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) {
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.ssh_direct_observation_by_tmux.insert(
        PromptKey::new(provider, tmux_session_name),
        TimedValue {
            value: (),
            recorded_at: Instant::now(),
        },
    );
}

/// True when an SSH-direct prompt has been observed for this
/// `(provider, tmux_session)` pair within `SSH_DIRECT_OBSERVATION_TTL` and
/// the matching anchor has not yet been consumed. Watchers use this to keep
/// the post-terminal suppress guard from killing legitimate direct-input
/// responses during the brief window before `record_prompt_anchor` lands.
pub(crate) fn is_ssh_direct_observation_pending(
    provider: &str,
    tmux_session_name: &str,
) -> bool {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return false;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .ssh_direct_observation_by_tmux
        .contains_key(&PromptKey::new(&provider, tmux_session_name))
}

fn clear_ssh_direct_observation_pending(provider: &str, tmux_session_name: &str) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state
        .ssh_direct_observation_by_tmux
        .remove(&PromptKey::new(&provider, tmux_session_name));
}

pub(crate) fn record_suppressed_discord_origin_prompt(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) {
    let provider = normalize_provider(provider);
    let tmux_session_name = tmux_session_name.trim();
    if provider.is_empty() || tmux_session_name.is_empty() || prompt.trim().is_empty() {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state.record_recent_observed_prompt(&provider, tmux_session_name, prompt);
}

pub fn extract_prompt_from_hook_payload(payload: &Value) -> Option<String> {
    for key in [
        "prompt",
        "user_prompt",
        "userPrompt",
        "message",
        "text",
        "input",
    ] {
        if let Some(prompt) = payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(prompt.to_string());
        }
    }
    payload
        .get("payload")
        .and_then(extract_prompt_from_hook_payload)
}

pub fn extract_codex_rollout_user_prompt(json: &Value) -> Option<String> {
    let payload = json.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    extract_message_content_text(payload)
}

pub fn extract_claude_transcript_user_prompt(json: &Value) -> Option<String> {
    if json.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    if json
        .get("isMeta")
        .and_then(Value::as_bool)
        .is_some_and(|is_meta| is_meta)
    {
        return None;
    }
    let message = json.get("message")?;
    if message
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| role != "user")
    {
        return None;
    }
    extract_message_content_text(message)
}

pub fn extract_qwen_jsonl_user_prompt(json: &Value) -> Option<String> {
    if json.get("type").and_then(Value::as_str) != Some("user") {
        return None;
    }
    let message = json.get("message")?;
    if message
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| role != "user")
    {
        return None;
    }
    extract_message_content_text(message)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptObservation {
    PublishedSshDirect,
    SuppressedDiscordDuplicate,
    SuppressedRecentDuplicate,
    Ignored,
}

fn resolve_tmux_session_name(provider: &str, provider_session_id: &str) -> Option<String> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .tmux_by_provider_session
        .get(&PromptKey::new(provider, provider_session_id))
        .map(|entry| entry.value.clone())
}

fn take_matching_pending_prompt(provider: &str, tmux_session_name: &str, prompt: &str) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let Some(queue) = state.pending_by_tmux.get_mut(&key) else {
        return false;
    };
    let matched = queue
        .iter()
        .position(|pending| prompts_match(&pending.value, prompt));
    if let Some(index) = matched {
        queue.remove(index);
    }
    if queue.is_empty() {
        state.pending_by_tmux.remove(&key);
    }
    if matched.is_some() {
        state.record_recent_observed_prompt(provider, tmux_session_name, prompt);
    }
    matched.is_some()
}

fn take_or_record_recent_observed_prompt(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let queue = state.recent_observed_by_tmux.entry(key).or_default();
    if queue
        .iter()
        .any(|observed| prompts_match(&observed.value, prompt))
    {
        return true;
    }
    state.record_recent_observed_prompt(provider, tmux_session_name, prompt);
    false
}

pub(crate) fn prompts_match(expected: &str, observed: &str) -> bool {
    let expected_trimmed = normalize_line_endings(expected).trim().to_string();
    let observed_trimmed = normalize_line_endings(observed).trim().to_string();
    if expected_trimmed == observed_trimmed {
        return true;
    }
    let expected_fuzzy = fuzzy_prompt_key(&expected_trimmed);
    let observed_fuzzy = fuzzy_prompt_key(&observed_trimmed);
    if expected_fuzzy == observed_fuzzy {
        return true;
    }
    false
}

fn normalize_line_endings(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn fuzzy_prompt_key(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn normalize_provider(provider: &str) -> String {
    provider.trim().to_ascii_lowercase()
}

fn extract_message_content_text(payload: &Value) -> Option<String> {
    match payload.get("content")? {
        Value::String(text) => non_empty(text),
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                if let Some(text) = item
                    .get("text")
                    .or_else(|| item.get("input_text"))
                    .and_then(Value::as_str)
                    .and_then(non_empty)
                {
                    parts.push(text);
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

impl TuiPromptDedupeState {
    fn record_recent_observed_prompt(
        &mut self,
        provider: &str,
        tmux_session_name: &str,
        prompt: &str,
    ) {
        self.recent_observed_by_tmux
            .entry(PromptKey::new(provider, tmux_session_name))
            .or_default()
            .push_back(TimedValue {
                value: prompt.to_string(),
                recorded_at: Instant::now(),
            });
    }

    fn purge_expired(&mut self) {
        let now = Instant::now();
        self.pending_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > PENDING_PROMPT_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        self.recent_observed_by_tmux.retain(|_, queue| {
            while queue
                .front()
                .is_some_and(|entry| now.duration_since(entry.recorded_at) > RECENT_OBSERVED_TTL)
            {
                queue.pop_front();
            }
            !queue.is_empty()
        });
        self.tmux_by_provider_session
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.channel_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.runtime_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= SESSION_MAPPING_TTL);
        self.prompt_anchor_by_tmux
            .retain(|_, entry| now.duration_since(entry.recorded_at) <= PROMPT_ANCHOR_TTL);
        self.ssh_direct_observation_by_tmux.retain(|_, entry| {
            now.duration_since(entry.recorded_at) <= SSH_DIRECT_OBSERVATION_TTL
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn reset_state() {
        let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
        *state = TuiPromptDedupeState::default();
    }

    // U-14 Provider-keyed channel isolation: registering the same tmux name
    // under both `claude` and `codex` providers must keep two independent
    // mappings — the dedupe state must not collapse them, otherwise cc/cdx
    // turns running side-by-side could cross-relay into each other's
    // channels.
    #[test]
    fn provider_session_mapping_isolates_claude_and_codex_for_same_session_id() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_provider_session("claude", "session-shared", "tmux-claude");
        register_provider_session("codex", "session-shared", "tmux-codex");

        assert_eq!(
            resolve_tmux_session_name("claude", "session-shared"),
            Some("tmux-claude".to_string())
        );
        assert_eq!(
            resolve_tmux_session_name("codex", "session-shared"),
            Some("tmux-codex".to_string())
        );

        // Recording a Discord-originated prompt for one provider must not
        // suppress an SSH-direct prompt the other provider observes.
        record_discord_originated_prompt("claude", "tmux-claude", "shared-text");

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-claude", "shared-text"),
            PromptObservation::SuppressedDiscordDuplicate
        );
        // The codex pane has no pending entry, so the same text is a fresh
        // direct-input observation, not a duplicate.
        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-codex", "shared-text"),
            PromptObservation::PublishedSshDirect
        );
    }

    // U-12 `relay_output_path` falls back to `output_path` when no dedicated
    // relay path is configured. A blank/whitespace-only override must not
    // shadow the primary output_path — otherwise the relay would tail an
    // empty path and silently drop frames.
    #[test]
    fn relay_output_path_falls_back_to_output_path_when_unset_or_blank() {
        let none_binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        assert_eq!(none_binding.relay_output_path(), "/tmp/transcript.jsonl");

        let blank_binding = TuiRuntimeBinding {
            relay_output_path: Some("   ".to_string()),
            ..none_binding.clone()
        };
        assert_eq!(blank_binding.relay_output_path(), "/tmp/transcript.jsonl");

        let override_binding = TuiRuntimeBinding {
            relay_output_path: Some("/tmp/relay.jsonl".to_string()),
            ..none_binding.clone()
        };
        assert_eq!(override_binding.relay_output_path(), "/tmp/relay.jsonl");
    }

    // U-12 `relay_last_offset()` mirrors `last_offset` when the override is
    // None — without this, the very first idle scan after a rehydrate
    // would tail from byte 0 and replay the entire transcript.
    #[test]
    fn relay_last_offset_falls_back_to_last_offset_when_unset() {
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: "/tmp/transcript.jsonl".to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 4096,
            relay_last_offset: None,
        };
        assert_eq!(binding.relay_last_offset(), 4096);

        let with_override = TuiRuntimeBinding {
            relay_last_offset: Some(1024),
            ..binding
        };
        assert_eq!(with_override.relay_last_offset(), 1024);
    }

    // U-10 `advance_tmux_runtime_binding_offset` is the cold-start entry
    // point used by relay readers to record where they left off. Calls with
    // a mismatched output_path that is not the configured relay override
    // must be rejected — otherwise a sibling reader writing the wrong path
    // could fast-forward our offset past unread frames.
    #[test]
    fn advance_offset_rejects_mismatched_path_when_relay_override_differs() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-cold",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/primary.jsonl".to_string(),
                relay_output_path: Some("/tmp/relay.jsonl".to_string()),
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );

        // Primary path advances `last_offset` and (because relay override
        // is set) leaves `relay_last_offset` alone.
        assert!(advance_tmux_runtime_binding_offset(
            "tmux-cold",
            "/tmp/primary.jsonl",
            500
        ));
        let after_primary = runtime_binding_for_tmux_session("tmux-cold").unwrap();
        assert_eq!(after_primary.last_offset, 500);
        assert!(after_primary.relay_last_offset.is_none());

        // Relay override path advances `relay_last_offset`.
        assert!(advance_tmux_runtime_binding_offset(
            "tmux-cold",
            "/tmp/relay.jsonl",
            900
        ));
        let after_relay = runtime_binding_for_tmux_session("tmux-cold").unwrap();
        assert_eq!(after_relay.relay_last_offset, Some(900));

        // An unrelated path is rejected and does not corrupt either offset.
        assert!(!advance_tmux_runtime_binding_offset(
            "tmux-cold",
            "/tmp/wrong.jsonl",
            9999
        ));
        let after_wrong = runtime_binding_for_tmux_session("tmux-cold").unwrap();
        assert_eq!(after_wrong.last_offset, 500);
        assert_eq!(after_wrong.relay_last_offset, Some(900));
    }

    #[test]
    fn suppresses_exact_pending_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("claude", "tmux-a", "hello");

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "hello"),
            PromptObservation::SuppressedDiscordDuplicate
        );
    }

    #[test]
    fn stores_runtime_binding_by_tmux_session() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            },
        );

        assert_eq!(
            runtime_binding_for_tmux_session("tmux-runtime"),
            Some(TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            })
        );
    }

    #[test]
    fn lists_runtime_bindings_by_kind() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-codex",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: None,
            },
        );
        register_tmux_runtime_binding(
            "tmux-claude",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 88,
                relay_last_offset: None,
            },
        );

        assert_eq!(
            runtime_bindings_for_kind(RuntimeHandoffKind::CodexTui),
            vec![(
                "tmux-codex".to_string(),
                TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::CodexTui,
                    output_path: "/tmp/codex-rollout.jsonl".to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some("thread-123".to_string()),
                    last_offset: 77,
                    relay_last_offset: None,
                },
            )]
        );
    }

    #[test]
    fn prompt_anchor_is_consumed_for_matching_tmux_and_channel() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        record_prompt_anchor("Claude", "tmux-anchor", 42, 9001);

        assert_eq!(
            take_prompt_anchor_for_response("claude", "tmux-anchor", 43),
            None
        );
        assert_eq!(
            take_prompt_anchor_for_response("claude", "tmux-anchor", 42),
            Some(TuiPromptAnchor {
                channel_id: 42,
                message_id: 9001,
            })
        );
        assert_eq!(
            take_prompt_anchor_for_response("claude", "tmux-anchor", 42),
            None
        );
    }

    #[test]
    fn prompt_anchor_can_be_peeked_until_delivery_commits() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        let anchor = TuiPromptAnchor {
            channel_id: 42,
            message_id: 9001,
        };
        record_prompt_anchor(
            "Claude",
            "tmux-anchor",
            anchor.channel_id,
            anchor.message_id,
        );

        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            Some(anchor)
        );
        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            Some(anchor)
        );
        assert!(!clear_prompt_anchor_for_response(
            "claude",
            "tmux-anchor",
            TuiPromptAnchor {
                channel_id: 42,
                message_id: 9002,
            },
        ));
        assert!(clear_prompt_anchor_for_response(
            "claude",
            "tmux-anchor",
            anchor,
        ));
        assert_eq!(
            prompt_anchor_for_response("claude", "tmux-anchor", 42),
            None
        );
    }

    #[test]
    fn ssh_direct_observation_marker_is_set_on_publish_and_cleared_with_anchor() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        // No observation yet → the bypass signal must stay false so the
        // post-terminal suppress guard keeps catching ghost output.
        assert!(!is_ssh_direct_observation_pending("claude", "tmux-direct"));

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-direct", "echo direct"),
            PromptObservation::PublishedSshDirect
        );
        // observe → marker is set immediately, before the relay subscriber
        // has even started its Discord notify await. This closes the race
        // window where a very fast TUI response would otherwise hit the
        // watcher with no anchor and get suppressed.
        assert!(is_ssh_direct_observation_pending("claude", "tmux-direct"));

        // Other (provider, tmux) pairs must not see the marker — cc/cdx
        // running side-by-side must not cross-bypass.
        assert!(!is_ssh_direct_observation_pending("codex", "tmux-direct"));
        assert!(!is_ssh_direct_observation_pending("claude", "tmux-other"));

        // Consuming the full anchor (i.e., response delivered to Discord)
        // also clears the pre-anchor marker so subsequent ghost output is
        // again subject to the suppress guard.
        let anchor = TuiPromptAnchor {
            channel_id: 77,
            message_id: 4242,
        };
        record_prompt_anchor("claude", "tmux-direct", anchor.channel_id, anchor.message_id);
        assert!(clear_prompt_anchor_for_response("claude", "tmux-direct", anchor));
        assert!(!is_ssh_direct_observation_pending("claude", "tmux-direct"));
    }

    #[test]
    fn advances_runtime_binding_offset_for_same_output_path() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: "/tmp/claude-transcript.jsonl".to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 77,
                relay_last_offset: None,
            },
        );

        assert!(!advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/other.jsonl",
            200
        ));
        assert_eq!(
            runtime_binding_for_tmux_session("tmux-runtime")
                .expect("binding")
                .last_offset,
            77
        );
        assert!(advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/claude-transcript.jsonl",
            200
        ));
        assert_eq!(
            runtime_binding_for_tmux_session("tmux-runtime")
                .expect("binding")
                .last_offset,
            200
        );
    }

    #[test]
    fn advances_runtime_binding_relay_offset_separately_from_runtime_path() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        register_tmux_runtime_binding(
            "tmux-runtime",
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::CodexTui,
                output_path: "/tmp/codex-rollout.jsonl".to_string(),
                relay_output_path: Some("/tmp/tmux-wrapper.jsonl".to_string()),
                input_fifo_path: None,
                session_id: Some("thread-123".to_string()),
                last_offset: 77,
                relay_last_offset: Some(33),
            },
        );

        assert!(advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/tmux-wrapper.jsonl",
            88
        ));
        let binding = runtime_binding_for_tmux_session("tmux-runtime").expect("binding");
        assert_eq!(binding.last_offset, 77);
        assert_eq!(binding.relay_last_offset, Some(88));
        assert!(!advance_tmux_runtime_binding_offset(
            "tmux-runtime",
            "/tmp/other.jsonl",
            99
        ));
    }

    #[test]
    fn suppresses_trailing_newline_pending_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("claude", "tmux-a", "hello\n");

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "hello"),
            PromptObservation::SuppressedDiscordDuplicate
        );
    }

    #[test]
    fn suppresses_fuzzy_whitespace_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-b", "Please   inspect\n\nthe failing test");

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-b", "please inspect the failing test"),
            PromptObservation::SuppressedDiscordDuplicate
        );
    }

    #[test]
    fn merged_draft_does_not_suppress_pending_discord_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-b", "[TUI-REL-OLD] respond with marker");

        assert_eq!(
            observe_prompt_by_tmux(
                "codex",
                "tmux-b",
                "[TUI-REL-OLD] respond with marker [TUI-REL-NEW] respond with marker",
            ),
            PromptObservation::PublishedSshDirect
        );
    }

    #[test]
    fn expired_pending_prompt_publishes_as_direct_input() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("claude", "tmux-a", "hello");
        {
            let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
            let queue = state
                .pending_by_tmux
                .get_mut(&PromptKey::new("claude", "tmux-a"))
                .expect("pending prompt queue");
            queue.front_mut().expect("pending prompt").recorded_at =
                Instant::now() - PENDING_PROMPT_TTL - Duration::from_secs(1);
        }

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "hello"),
            PromptObservation::PublishedSshDirect
        );
    }

    #[test]
    fn removed_prompt_after_submit_failure_publishes_as_direct_input() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-b", "failed submit");
        remove_discord_originated_prompt("codex", "tmux-b", "failed submit");

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-b", "failed submit"),
            PromptObservation::PublishedSshDirect
        );
    }

    #[test]
    fn publishes_unmatched_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_by_tmux("claude", "tmux-a", "typed over ssh"),
            PromptObservation::PublishedSshDirect
        );
    }

    #[test]
    fn suppresses_recent_direct_duplicate_prompt() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "typed over ssh"),
            PromptObservation::PublishedSshDirect
        );
        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "typed over ssh\n"),
            PromptObservation::SuppressedRecentDuplicate
        );
    }

    #[test]
    fn pending_match_leaves_recent_tombstone_for_second_observer() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset_state();
        record_discord_originated_prompt("codex", "tmux-c", "from discord");

        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "from discord"),
            PromptObservation::SuppressedDiscordDuplicate
        );
        assert_eq!(
            observe_prompt_by_tmux("codex", "tmux-c", "from discord"),
            PromptObservation::SuppressedRecentDuplicate
        );
    }

    #[test]
    fn extracts_codex_rollout_user_message_text() {
        let json = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "hello" },
                    { "type": "input_text", "text": "world" }
                ]
            }
        });

        assert_eq!(
            extract_codex_rollout_user_prompt(&json).as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn extracts_claude_transcript_user_message_text() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "text", "text": "world" }
                ]
            },
            "sessionId": "sess-tui",
        });

        assert_eq!(
            extract_claude_transcript_user_prompt(&json).as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn ignores_claude_transcript_meta_user_message_text() {
        let json = serde_json::json!({
            "type": "user",
            "isMeta": true,
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "_" }
                ]
            },
            "sessionId": "sess-tui",
        });

        assert_eq!(extract_claude_transcript_user_prompt(&json), None);
    }

    #[test]
    fn extracts_qwen_jsonl_user_message_text() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "text", "text": "world" }
                ]
            }
        });

        assert_eq!(
            extract_qwen_jsonl_user_prompt(&json).as_deref(),
            Some("hello\nworld")
        );
    }

    #[test]
    fn ignores_qwen_tool_result_user_messages() {
        let json = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "content": "done",
                    "is_error": false
                }]
            }
        });

        assert_eq!(extract_qwen_jsonl_user_prompt(&json), None);
    }
}
