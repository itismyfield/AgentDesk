use super::*;

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
    extract_codex_rollout_user_prompt_with_entry_id(json).map(|(prompt, _)| prompt)
}

pub fn extract_codex_rollout_user_prompt_with_entry_id(
    json: &Value,
) -> Option<(String, Option<String>)> {
    let payload = json.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    let prompt = reject_synthetic_tui_user_prompt(extract_message_content_text(payload)?)?;
    let entry_id = extract_codex_rollout_entry_id(json, payload);
    Some((prompt, entry_id))
}

fn extract_codex_rollout_entry_id(json: &Value, payload: &Value) -> Option<String> {
    payload
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| payload.get("item_id").and_then(Value::as_str))
        .or_else(|| json.get("id").and_then(Value::as_str))
        .or_else(|| json.get("item_id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub fn extract_claude_transcript_user_prompt(json: &Value) -> Option<String> {
    extract_claude_transcript_user_prompt_with_entry_id(json).map(|(prompt, _)| prompt)
}

/// #3540: same extraction as [`extract_claude_transcript_user_prompt`], but also
/// returns the JSONL entry's STABLE identity (`uuid`) when present.
///
/// Claude Code stamps every transcript `user` entry with a content-stable
/// top-level `uuid` (measured: ~18k user uuids across ~3.8k transcript files
/// with ZERO cross-file collisions — it is a genuine per-entry identity, not a
/// timestamp derivative). The relay-watermark reset path (`/relay-scan`
/// self-loop + jsonl head rotation) re-presents an already-relayed prompt at a
/// shifted byte offset; the rotation is a `truncate_jsonl_head_safe` rename
/// (head clipped, surviving bytes preserved verbatim), so the SAME logical
/// prompt keeps its uuid even though its offset moved. The idle-transcript
/// scanner threads this uuid into the dedupe layer so an already-relayed entry
/// is suppressed by IDENTITY (see [`observe_prompt_candidates_by_tmux`]) without
/// ever inspecting inflight / EOF / current_msg_id — sidestepping the
/// observationally-indistinguishable phantom-vs-slow-live-turn problem entirely.
///
/// Defensive extraction: the uuid is read from the top-level object (where
/// Claude Code places it for `user` entries) with a `message.uuid` fallback for
/// forward/backward tolerance. A missing uuid yields `None`, in which case the
/// scanner falls back to the existing content-keyed 30s recent-observed dedup —
/// no regression, just the same window as before #3540.
pub fn extract_claude_transcript_user_prompt_with_entry_id(
    json: &Value,
) -> Option<(String, Option<String>)> {
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
    let prompt = reject_synthetic_claude_user_prompt(extract_message_content_text(message)?)?;
    let entry_id = extract_claude_transcript_entry_id(json, message);
    Some((prompt, entry_id))
}

/// #3540: pull the stable entry identity for a Claude transcript `user` entry.
/// Prefers the top-level `uuid` (where Claude Code writes it), falls back to a
/// `message.uuid` if a future format ever moves it. Returns a normalized,
/// non-empty `String` or `None` (the scanner treats `None` as "no stable
/// identity available — use the content-keyed fallback").
fn extract_claude_transcript_entry_id(json: &Value, message: &Value) -> Option<String> {
    json.get("uuid")
        .and_then(Value::as_str)
        .or_else(|| message.get("uuid").and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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
    reject_synthetic_tui_user_prompt(extract_message_content_text(message)?)
}

/// #3527: `[User: <author> (ID: <digits>)] …` is AgentDesk's OWN Discord→TUI
/// relay format (`discord/router/response_format.rs`), never an external SSH/cron
/// injection — so the observer must not mint a synthetic turn for a re-observed
/// one (the discord-originated ledger only suppresses the first, consumed/
/// TTL-bounded sighting; a quiescence-timeout re-observation slips through).
///
/// The marker can be PRECEDED by prepended context (`[External Recall]`, reply/
/// upload context, …) AND can be collapsed mid-line: the legacy pane observer
/// (`tmux_watcher/prompt_observe.rs`) submits `join("")` / `join(" ")` /
/// `join("\n")` variants of one block, so a line-anchored check would miss the
/// collapsed ones (codex #3527). Scan the WHOLE string: find `[User: `, then any
/// following `(ID: <digits>)]` (author may itself contain parens).
pub(super) fn is_discord_relayed_user_prompt(prompt: &str) -> bool {
    let Some(user_at) = prompt.find("[User: ") else {
        return false;
    };
    let mut tail = &prompt[user_at + "[User: ".len()..];
    while let Some(id_at) = tail.find("(ID: ") {
        let after_id = &tail[id_at + "(ID: ".len()..];
        if let Some(close) = after_id.find(")]") {
            let digits = &after_id[..close];
            if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return true;
            }
        }
        tail = &tail[id_at + "(ID: ".len()..];
    }
    false
}

pub(super) fn is_user_prefixed_subagent_notification_machine_event(prompt: &str) -> bool {
    let mut current = prompt.trim_start();
    let mut saw_user_prefix = false;

    loop {
        if let Some(tail) = strip_provider_session_reuse_prologue(current) {
            current = tail.trim_start();
            continue;
        }

        let stripped_chrome = strip_leading_tui_response_chrome(current);
        if stripped_chrome != current {
            current = stripped_chrome.trim_start();
            continue;
        }

        if let Some(tail) = strip_leading_user_author_prefix(current) {
            saw_user_prefix = true;
            current = tail.trim_start();
            continue;
        }

        break;
    }

    saw_user_prefix && starts_with_xmlish_tag(current.trim_start(), "subagent_notification")
}

fn strip_provider_session_reuse_prologue(normalized: &str) -> Option<&str> {
    const RESUMED_THREAD_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
         instructions already present in this Codex thread still apply. Treat only this turn's \
         user request, reply context, uploaded files, and memory recall below as new actionable \
         input.";
    const FRESH_FORK_PROLOGUE: &str = "The prior authoritative Discord, role, and tool \
         instructions already issued to this role in the current dcserver lifetime still apply. \
         Treat only this turn's user request, reply context, uploaded files, and memory recall \
         below as new actionable input.";

    let rest = normalized
        .strip_prefix("[Provider Session Reuse]")?
        .trim_start();
    provider_reuse_tail(rest, RESUMED_THREAD_PROLOGUE)
        .or_else(|| provider_reuse_tail(rest, FRESH_FORK_PROLOGUE))
}

fn provider_reuse_tail<'a>(rest: &'a str, prologue: &str) -> Option<&'a str> {
    rest.strip_prefix(prologue)
        .and_then(|tail| tail.strip_prefix("\n\n"))
}

fn strip_leading_tui_response_chrome(input: &str) -> &str {
    let mut stripped = input;
    loop {
        let trimmed = stripped.trim_start();
        if let Some(rest) = trimmed.strip_prefix("No response requested.")
            && (rest.is_empty()
                || rest.starts_with('\n')
                || rest.starts_with('\r')
                || rest.chars().next().is_some_and(|ch| !ch.is_whitespace()))
        {
            stripped = rest;
            continue;
        }
        return trimmed;
    }
}

fn strip_leading_user_author_prefix(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("[User: ")?;
    let close = rest.find(']')?;
    Some(rest[close + 1..].trim_start())
}

fn starts_with_xmlish_tag(text: &str, tag: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(tag) else {
        return false;
    };
    rest.starts_with('>') || rest.chars().next().is_some_and(char::is_whitespace)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptObservation {
    PublishedSshDirect,
    /// A structured `<task-notification>` was published for status/card
    /// rendering without creating external-input ownership or a response tail.
    PublishedTaskNotification,
    SuppressedDiscordDuplicate,
    SuppressedRecentDuplicate,
    /// #3540: the observed prompt's stable JSONL entry `uuid` was ALREADY relayed
    /// for this `(provider, tmux)` pair. Distinct from
    /// [`Self::SuppressedRecentDuplicate`]: that is a content match bounded by the
    /// 30s recent window, whereas this is an IDENTITY match bounded only by the
    /// 30min entry-id TTL. The idle-transcript scanner treats it like the other
    /// suppressions — `should_tail_response == false` — so a re-encountered
    /// already-relayed entry (watermark reset / jsonl head rotation) never mints a
    /// phantom synthetic inflight. A genuinely new prompt carries a new uuid and
    /// is never returned here.
    SuppressedReplayedEntry,
    Ignored,
}

pub(super) fn resolve_tmux_session_name(
    provider: &str,
    provider_session_id: &str,
) -> Option<String> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .tmux_by_provider_session
        .get(&PromptKey::new(provider, provider_session_id))
        .map(|entry| entry.value.clone())
}

pub(super) fn take_matching_pending_prompt(
    provider: &str,
    tmux_session_name: &str,
    prompt: &str,
) -> bool {
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

pub(super) fn take_or_record_recent_observed_prompt(
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

/// #3540: `true` iff `entry_id` is in the already-relayed ledger for this
/// `(provider, tmux)` pair (and not yet purged). Read-only — recording happens
/// separately in [`record_relayed_entry_id`] at the actual relay point.
pub(super) fn relayed_entry_id_already_seen(
    provider: &str,
    tmux_session_name: &str,
    entry_id: &str,
) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    state
        .relayed_entry_ids_by_tmux
        .get(&PromptKey::new(provider, tmux_session_name))
        .is_some_and(|queue| queue.iter().any(|seen| seen.value == entry_id))
}

/// #3540: record `entry_id` as relayed for this `(provider, tmux)` pair. Called
/// only at the actual relay point (after pending/recent dedup pass), so a
/// dedup-suppressed candidate is never mis-recorded as relayed. Idempotent: a
/// re-record of an id already present refreshes nothing and does not duplicate
/// (the identity check would have short-circuited the caller anyway). Ring-capped
/// per key at [`RELAYED_ENTRY_ID_RING_CAP`] (oldest dropped first); TTL-purged by
/// `PROMPT_ANCHOR_TTL`.
pub(super) fn record_relayed_entry_id(provider: &str, tmux_session_name: &str, entry_id: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let queue = state
        .relayed_entry_ids_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default();
    if queue.iter().any(|seen| seen.value == entry_id) {
        return;
    }
    queue.push_back(TimedValue {
        value: entry_id.to_string(),
        recorded_at: Instant::now(),
    });
    while queue.len() > RELAYED_ENTRY_ID_RING_CAP {
        queue.pop_front();
    }
}

fn record_local_only_entry_id(prompt: &ObservedTuiPrompt) {
    if classify_local_only_slash_control(&prompt.prompt).is_none() {
        return;
    }
    let Some(entry_id) = prompt
        .source_event_id
        .as_deref()
        .map(str::trim)
        .filter(|entry_id| !entry_id.is_empty())
    else {
        return;
    };
    record_relayed_entry_id(&prompt.provider, &prompt.tmux_session_name, entry_id);
}

/// Mark a local-only entry as replayed after its Discord session note was accepted.
pub(crate) fn record_local_only_entry_id_after_note_delivery(prompt: &ObservedTuiPrompt) {
    record_local_only_entry_id(prompt);
}

/// Seal a local-only transcript half collapsed into an already-rendered note.
pub(crate) fn seal_deduped_local_only_entry_id(prompt: &ObservedTuiPrompt) {
    record_local_only_entry_id(prompt);
}

pub(crate) fn prompts_match(expected: &str, observed: &str) -> bool {
    let expected_trimmed = normalize_line_endings(expected).trim().to_string();
    let observed_trimmed = normalize_line_endings(observed).trim().to_string();
    if expected_trimmed == observed_trimmed {
        return true;
    }
    if let (Some(expected_command), Some(observed_command)) = (
        slash_command_prompt_key(&expected_trimmed),
        slash_command_prompt_key(&observed_trimmed),
    ) {
        if expected_command == observed_command {
            return true;
        }
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

#[derive(Debug, PartialEq, Eq)]
struct SlashCommandPromptKey {
    name: String,
    args: String,
}

fn slash_command_prompt_key(value: &str) -> Option<SlashCommandPromptKey> {
    slash_command_xml_prompt_key(value).or_else(|| slash_command_invocation_prompt_key(value))
}

fn slash_command_xml_prompt_key(value: &str) -> Option<SlashCommandPromptKey> {
    let trimmed = value.trim();
    if !(trimmed.starts_with("<command-message>") || trimmed.starts_with("<command-name>")) {
        return None;
    }
    let command_name = extract_xml_tag(trimmed, "command-name")?;
    let (name, name_args) = parse_slash_command_invocation(command_name)?;
    let args = extract_xml_tag(trimmed, "command-args")
        .and_then(non_empty)
        .unwrap_or(name_args);
    Some(SlashCommandPromptKey {
        name,
        args: fuzzy_prompt_key(&args),
    })
}

fn slash_command_invocation_prompt_key(value: &str) -> Option<SlashCommandPromptKey> {
    let (name, args) = parse_slash_command_invocation(value)?;
    Some(SlashCommandPromptKey {
        name,
        args: fuzzy_prompt_key(&args),
    })
}

fn parse_slash_command_invocation(value: &str) -> Option<(String, String)> {
    let trimmed = value.trim();
    let (name, args) = match trimmed.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args),
        None => (trimmed, ""),
    };
    if !name.starts_with('/') || name.len() <= 1 {
        return None;
    }
    Some((name.to_ascii_lowercase(), args.trim().to_string()))
}

fn extract_xml_tag<'a>(value: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let after_open = value.split_once(&open)?.1;
    let (body, _) = after_open.split_once(&close)?;
    Some(body.trim())
}

pub(super) fn normalize_provider(provider: &str) -> String {
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
