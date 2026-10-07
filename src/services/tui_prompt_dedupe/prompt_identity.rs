//! Claude `prompt_id` as the relayed-entry ledger's second key, known before the row's
//! uuid. Only hooks record it (forks rewrite row ids); once announced it suppresses rows.

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
    /// Same text, but the hook's announcement has no POST result yet; suppresses nothing.
    Unannounced,
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
    if !same_text(&entry.value.prompt, prompt) {
        entry.value.ambiguous = true;
        return PromptIdMatch::Ambiguous;
    }
    if entry.value.announced {
        PromptIdMatch::Same
    } else {
        PromptIdMatch::Unannounced
    }
}

/// Holds a published hook's `prompt_id` unannounced so a row read before the POST
/// result can mark it ambiguous; a present id keeps its first record unchanged.
pub(super) fn record_observed_hook_prompt_id(
    provider: &str,
    tmux_session_name: &str,
    prompt_id: &str,
    prompt: &str,
    observed_by: u64,
) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let queue = state
        .relayed_prompt_ids_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default();
    if queue.iter().any(|seen| seen.value.prompt_id == prompt_id) {
        return;
    }
    queue.push_back(TimedValue {
        value: RelayedPromptId {
            prompt_id: prompt_id.to_string(),
            prompt: prompt.to_string(),
            ambiguous: false,
            announced: false,
            observed_by,
        },
        recorded_at: Instant::now(),
    });
    while queue.len() > RELAYED_ENTRY_ID_RING_CAP {
        queue.pop_front();
    }
}

/// Lets the observation's hook `prompt_id` suppress once its announcement was sent
/// or may have been; an id already marked ambiguous stays ambiguous.
pub fn record_announced_prompt_id(prompt: &ObservedTuiPrompt) {
    settle_hook_prompt_id(prompt, true);
}

/// Drops the hook `prompt_id` the observation left unannounced, for a relay that
/// never POSTed or whose POST certainly created nothing; an announced id stays.
pub fn withdraw_unannounced_prompt_id(prompt: &ObservedTuiPrompt) {
    settle_hook_prompt_id(prompt, false);
}

fn settle_hook_prompt_id(prompt: &ObservedTuiPrompt, announced: bool) {
    let Some(prompt_id) = prompt.hook_prompt_id.as_deref() else {
        return;
    };
    let observed_by = prompt.ssh_direct_observation_generation;
    if observed_by == SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED {
        return;
    }
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(&prompt.provider, prompt.tmux_session_name.trim());
    let Some(queue) = state.relayed_prompt_ids_by_tmux.get_mut(&key) else {
        return;
    };
    let Some(index) = queue.iter().position(|seen| {
        seen.value.prompt_id == prompt_id
            && seen.value.observed_by == observed_by
            && !seen.value.announced
    }) else {
        return;
    };
    if announced {
        queue[index].value.announced = true;
    } else {
        queue.remove(index);
    }
}

/// Ages the content, uuid and prompt-id records while preserving other keys' ages.
#[cfg(test)]
pub(crate) fn age_observed_prompt_records_for_tests(
    provider: &str,
    tmux_session_name: &str,
    by: Duration,
) {
    let key = PromptKey::new(provider, tmux_session_name);
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.aged_stamp_for_tests(by);
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
