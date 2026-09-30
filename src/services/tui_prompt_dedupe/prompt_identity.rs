//! Claude `prompt_id` as the relayed-entry ledger's second key: the hook has it
//! before the row's uuid exists. Only hooks record it (forks rewrite row ids).

use super::*;

/// Where an observed `prompt_id` came from; only a hook submission records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudePromptId<'a> {
    HookSubmit(&'a str),
    TranscriptRow(&'a str),
}

impl<'a> ClaudePromptId<'a> {
    pub(super) fn value(self) -> &'a str {
        match self {
            Self::HookSubmit(value) | Self::TranscriptRow(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PromptIdMatch {
    Absent,
    Same,
    /// Seen with other text; the id no longer suppresses anything.
    Ambiguous,
}

pub fn extract_prompt_id_from_hook_payload(payload: &Value) -> Option<String> {
    non_empty_str(payload.get("prompt_id"))
}

pub fn extract_claude_transcript_prompt_id(json: &Value) -> Option<String> {
    non_empty_str(json.get("promptId"))
}

fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn same_text(recorded: &str, observed: &str) -> bool {
    normalize_line_endings(recorded).trim() == normalize_line_endings(observed).trim()
}

/// Compares `prompt` with the recorded text for `prompt_id`; other text marks
/// the id ambiguous so neither text is suppressed through it afterwards.
pub(super) fn check_relayed_prompt_id(
    provider: &str,
    tmux_session_name: &str,
    prompt_id: &str,
    prompt: &str,
) -> PromptIdMatch {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let Some(entry) = state
        .relayed_prompt_ids_by_tmux
        .get_mut(&PromptKey::new(provider, tmux_session_name))
        .and_then(|queue| {
            queue
                .iter_mut()
                .find(|seen| seen.value.prompt_id == prompt_id)
        })
    else {
        return PromptIdMatch::Absent;
    };
    if entry.value.ambiguous {
        return PromptIdMatch::Ambiguous;
    }
    if same_text(&entry.value.prompt, prompt) {
        return PromptIdMatch::Same;
    }
    entry.value.ambiguous = true;
    PromptIdMatch::Ambiguous
}

/// Records a hook-submitted `prompt_id` at the relay point. A present id keeps
/// its first record time; other text only marks it ambiguous.
pub(super) fn record_relayed_prompt_id(
    provider: &str,
    tmux_session_name: &str,
    prompt_id: &str,
    prompt: &str,
    recorded_by: u64,
) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let queue = state
        .relayed_prompt_ids_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default();
    if let Some(entry) = queue
        .iter_mut()
        .find(|seen| seen.value.prompt_id == prompt_id)
    {
        if !same_text(&entry.value.prompt, prompt) {
            entry.value.ambiguous = true;
        }
        return;
    }
    queue.push_back(TimedValue {
        value: RelayedPromptId {
            prompt_id: prompt_id.to_string(),
            prompt: prompt.to_string(),
            ambiguous: false,
            recorded_by,
        },
        recorded_at: Instant::now(),
    });
    while queue.len() > RELAYED_ENTRY_ID_RING_CAP {
        queue.pop_front();
    }
}

/// Drops the prompt id recorded by observation `recorded_by` once its announcement
/// is known unsent, so the idle scanner can announce the prompt again.
pub fn withdraw_relayed_prompt_id(provider: &str, tmux_session_name: &str, recorded_by: u64) {
    if recorded_by == SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED {
        return;
    }
    let provider = normalize_provider(provider);
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let key = PromptKey::new(&provider, tmux_session_name.trim());
    if let Some(queue) = state.relayed_prompt_ids_by_tmux.get_mut(&key) {
        queue.retain(|seen| seen.value.recorded_by != recorded_by);
    }
}

/// Test-only: backdates every content, uuid and prompt-id record for one key.
#[cfg(test)]
pub(crate) fn age_observed_prompt_records_for_tests(
    provider: &str,
    tmux_session_name: &str,
    by: Duration,
) {
    let key = PromptKey::new(provider, tmux_session_name);
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    for entry in state
        .recent_observed_by_tmux
        .entry(key.clone())
        .or_default()
    {
        entry.recorded_at -= by;
    }
    for entry in state
        .relayed_entry_ids_by_tmux
        .entry(key.clone())
        .or_default()
    {
        entry.recorded_at -= by;
    }
    for entry in state.relayed_prompt_ids_by_tmux.entry(key).or_default() {
        entry.recorded_at -= by;
    }
}

#[cfg(test)]
#[path = "prompt_identity_tests.rs"]
mod tests;
