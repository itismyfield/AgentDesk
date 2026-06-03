//! Structured, deduped Discord cards for Claude/Codex TUI `<task-notification>`
//! auto-turn events (#3075).
//!
//! Background: Claude Code / Codex inject a `<task-notification>` XML-ish block
//! into their own TUI session when a subagent / background task / dynamic
//! workflow reaches a terminal state. AgentDesk observes that injected text via
//! the SSH-direct TUI prompt relay. Before #3075 the text was mirrored verbatim
//! as a `터미널에 직접 주입된 입력` code block — a noisy machine event masquerading
//! as human input.
//!
//! This module turns a `<task-notification>` payload into a compact, scannable
//! card and DEDUPES repeated completions for the same `task-id` by EDITING the
//! previously-posted card instead of posting N new messages (#3075 cites a
//! single task firing 5×). The parsing is deliberately defensive: the payload is
//! XML-ish with embedded free-form Markdown/JSON, so we only extract the stable
//! structured fields and never pattern-match generated prose (a constraint the
//! issue calls out explicitly — no `Stale Monitor` / `No action needed`
//! matching).
//!
//! The card store (`task-id → posted message id`) lives here, keyed per channel,
//! and is bounded so a long-lived process cannot leak unbounded card anchors.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serde_json::Value;
use serenity::{ChannelId, MessageId};

use super::SharedData;

/// Max number of distinct task cards tracked per channel before the
/// least-recently-touched card anchor is evicted. Bounds memory for a
/// long-lived process; an evicted task-id simply posts a fresh card on its next
/// notification (the dedupe window has effectively closed for it).
const MAX_CARDS_PER_CHANNEL: usize = 128;

/// A tracked task card grows stale after this window; a notification for a
/// task-id last touched longer ago posts a fresh card rather than editing a
/// likely-scrolled-away message. Keeps dedupe scoped to an active burst.
const CARD_STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// Preview budget for a free-form Markdown `result` body rendered into a card.
/// Long subagent reports are truncated to keep the card scannable on mobile; the
/// full payload remains available via the existing output/log path.
const RESULT_PREVIEW_CHARS: usize = 600;

/// Number of leading non-blank `result` lines surfaced as the card body preview
/// for a free-form Markdown completion report.
const RESULT_PREVIEW_LINES: usize = 3;

/// Structured fields extracted from a `<task-notification>` payload (#3075).
///
/// All fields are optional/defensive: a real payload is XML-ish with embedded
/// content and the exact tag set varies (some repeats omit `<tool-use-id>`,
/// `<output-file>` is hidden by default, `result` may be Markdown or JSON).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TaskNotification {
    pub task_id: Option<String>,
    pub status: Option<String>,
    pub summary: Option<String>,
    pub result: Option<String>,
    pub usage: Option<String>,
    pub duration: Option<String>,
    pub tool_uses: Option<String>,
}

/// Parse a `<task-notification>` payload into its structured fields (#3075).
///
/// Defensive XML-ish extraction: we read the inner text of the first occurrence
/// of each known tag, accepting both hyphen and underscore spellings of the
/// id/use tags (`task-id`/`task_id`, `tool-use-id`/`tool_use_id`,
/// `output-file`/`output_file`) because the observed payloads use both. Terminal
/// control sequences a TUI injector may prepend are stripped first. Unknown tags
/// and attributes are ignored. Returns the parsed fields; callers decide how to
/// render. `output-file` is intentionally NOT surfaced (hidden by default per
/// the issue) so it is not stored here.
pub(super) fn parse_task_notification(raw: &str) -> TaskNotification {
    let text = strip_terminal_controls(raw);
    let trimmed = text.trim();
    TaskNotification {
        task_id: extract_tag(trimmed, &["task-id", "task_id"]),
        status: extract_tag(trimmed, &["status"]),
        summary: extract_tag(trimmed, &["summary"]),
        result: extract_tag(trimmed, &["result"]),
        usage: extract_tag(trimmed, &["usage"]),
        duration: extract_tag(trimmed, &["duration"]),
        tool_uses: extract_tag(trimmed, &["tool-uses", "tool_uses", "tool-use-count"]),
    }
}

/// Extract the inner text of the first `<name>…</name>` for any of `names`.
///
/// Tolerates attributes on the open tag (`<name attr="x">`). Returns the trimmed
/// inner text, or `None` if absent/empty. Used only for the stable structured
/// fields; never for prose pattern-matching.
fn extract_tag(haystack: &str, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = extract_one_tag(haystack, name) {
            return Some(value);
        }
    }
    None
}

fn extract_one_tag(haystack: &str, name: &str) -> Option<String> {
    let open_prefix = format!("<{name}");
    let close = format!("</{name}>");
    let open_at = haystack.find(&open_prefix)?;
    // Find the '>' that closes the (possibly attributed) open tag.
    let after_prefix = open_at + open_prefix.len();
    let rest = &haystack[after_prefix..];
    // The char right after the tag name must be '>' or whitespace (so `<status>`
    // is not matched by a hypothetical `<status-bar>` request). If it is `>` the
    // tag has no attributes; otherwise scan to the next `>`.
    let first = rest.chars().next()?;
    if first != '>' && !first.is_whitespace() {
        return None;
    }
    let gt_rel = rest.find('>')?;
    let inner_start = after_prefix + gt_rel + 1;
    let inner_end_rel = haystack[inner_start..].find(&close)?;
    let inner = &haystack[inner_start..inner_start + inner_end_rel];
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Render a `<task-notification>` payload as a compact Discord card (#3075).
///
/// `update_count` is the number of times this task-id has been seen (1 on the
/// first card). When > 1 the header notes `updated Nx` and the body shows the
/// LATEST result preview. A JSON `result` is summarized by top-level shape
/// (`results`/`reviews`/`plans`) rather than dumped raw; a free-form Markdown
/// `result` is truncated to a short preview with any PR URLs surfaced.
pub(super) fn format_task_notification_card(note: &TaskNotification, update_count: u32) -> String {
    let status = note.status.as_deref().unwrap_or("");
    let icon = status_icon(status);
    let status_label = if status.is_empty() {
        "Task event".to_string()
    } else {
        format!("Task {status}")
    };
    let mut header = format!("{icon} {status_label}");
    if update_count > 1 {
        header.push_str(&format!(" · updated {update_count}x"));
    }

    let mut lines = vec![header];

    if let Some(summary) = note.summary.as_deref().filter(|s| !s.is_empty()) {
        lines.push(format!("**{}**", sanitize_oneline(summary)));
    }

    if let Some(result) = note.result.as_deref().filter(|s| !s.is_empty()) {
        let body = render_result_preview(result, update_count > 1);
        if !body.is_empty() {
            lines.push(String::new());
            lines.push(body);
        }
        for url in extract_pr_urls(result) {
            lines.push(format!("🔗 {url}"));
        }
    }

    // Footer: short task id + usage/duration metadata, all from structured
    // fields (never generated prose). `output-file` is intentionally omitted.
    let mut footer_parts: Vec<String> = Vec::new();
    if let Some(task_id) = note.task_id.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(format!("task {}", short_task_id(task_id)));
    }
    if let Some(usage) = note.usage.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(sanitize_oneline(usage));
    }
    if let Some(tool_uses) = note.tool_uses.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(format!("{} tools", sanitize_oneline(tool_uses)));
    }
    if let Some(duration) = note.duration.as_deref().filter(|s| !s.is_empty()) {
        footer_parts.push(sanitize_oneline(duration));
    }
    if !footer_parts.is_empty() {
        lines.push(String::new());
        lines.push(format!("-# {}", footer_parts.join(" · ")));
    }

    lines.join("\n")
}

fn status_icon(status: &str) -> &'static str {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "done" | "success" | "succeeded" => "✅",
        "failed" | "error" | "errored" => "❌",
        "cancelled" | "canceled" | "stopped" => "🛑",
        "" => "📋",
        _ => "📋",
    }
}

/// First 8 hex-ish chars of a task id for a compact, stable footer label.
fn short_task_id(task_id: &str) -> String {
    let trimmed = task_id.trim();
    trimmed.chars().take(8).collect()
}

/// Render the `result` body: a JSON aggregate is summarized by shape; otherwise
/// a free-form Markdown body is reduced to a short preview.
fn render_result_preview(result: &str, latest: bool) -> String {
    let prefix = if latest { "Latest: " } else { "" };
    if let Some(summary) = summarize_json_result(result) {
        return format!("{prefix}{summary}");
    }
    let preview = markdown_preview(result);
    if preview.is_empty() {
        String::new()
    } else {
        format!("{prefix}{preview}")
    }
}

/// If `result` parses as JSON, render a compact aggregate by top-level shape
/// (#3075 §3): `results[]` / `reviews[]` / `plans[]` counts + a short list,
/// never the raw JSON. Returns `None` for non-JSON bodies.
fn summarize_json_result(result: &str) -> Option<String> {
    let value: Value = serde_json::from_str(result.trim()).ok()?;
    let obj = value.as_object()?;
    let mut parts: Vec<String> = Vec::new();
    for (key, label) in [
        ("results", "results"),
        ("reviews", "reviews"),
        ("plans", "plans"),
    ] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            parts.push(format!("{} {label}", arr.len()));
        }
    }
    if parts.is_empty() {
        // A JSON object/array we don't have a specific shape for: report that it
        // is a structured result without dumping it.
        let kind = if value.is_array() {
            "JSON array"
        } else {
            "JSON object"
        };
        return Some(format!("structured {kind} result (preview suppressed)"));
    }
    Some(format!("aggregate: {}", parts.join(", ")))
}

/// Short Markdown preview: first few non-blank lines, char-capped.
fn markdown_preview(result: &str) -> String {
    let mut collected: Vec<String> = Vec::new();
    for line in result.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        collected.push(sanitize_oneline(line));
        if collected.len() >= RESULT_PREVIEW_LINES {
            break;
        }
    }
    let joined = collected.join("\n");
    truncate_chars(&joined, RESULT_PREVIEW_CHARS)
}

/// Extract up to a few distinct GitHub PR/issue URLs from a result body so they
/// are clickable in the card. Order-preserving, deduped, bounded.
fn extract_pr_urls(result: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for token in result.split(|c: char| {
        c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '"' | '\'')
    }) {
        let token = token.trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!'));
        if (token.starts_with("https://github.com/") || token.starts_with("http://github.com/"))
            && (token.contains("/pull/") || token.contains("/issues/"))
            && !seen.iter().any(|u| u == token)
        {
            seen.push(token.to_string());
            if seen.len() >= 3 {
                break;
            }
        }
    }
    seen
}

/// Collapse a value to a single line and strip Discord-fence/markup hazards for
/// inline rendering.
fn sanitize_oneline(value: &str) -> String {
    value
        .replace('\r', " ")
        .replace('\n', " ")
        .replace("```", "` ` `")
}

fn truncate_chars(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// Char-bounded truncation appending `...` (ASCII) on overflow. Shared with the
/// SSH-direct / continuation formatters in `tui_prompt_relay` (#3075 consolidated
/// the duplicate helper here).
pub(super) fn truncate_chars_ascii(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Strip ANSI/terminal control sequences a TUI injector may prepend before the
/// `<task-notification>` tag, preserving newlines/tabs. Shared sanitizer so the
/// relay's classifier, the SSH-direct/continuation formatters, and this card
/// parser all see the same clean text (#3075 consolidated the duplicate).
pub(super) fn strip_terminal_controls(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek().copied() == Some('[') {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if ch.is_control() && ch != '\n' && ch != '\r' && ch != '\t' {
            continue;
        }
        output.push(ch);
    }
    output
}

// ---------------------------------------------------------------------------
// Per-channel task-id → posted card store (dedupe), bounded.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TaskCardEntry {
    message_id: u64,
    update_count: u32,
    touched_at: Instant,
}

#[derive(Default)]
struct TaskCardStore {
    // channel_id -> (task_id -> entry)
    by_channel: HashMap<u64, HashMap<String, TaskCardEntry>>,
}

static CARD_STORE: LazyLock<Mutex<TaskCardStore>> =
    LazyLock::new(|| Mutex::new(TaskCardStore::default()));

#[cfg(test)]
pub(super) fn reset_card_store_for_tests() {
    let mut store = CARD_STORE.lock().unwrap_or_else(|e| e.into_inner());
    store.by_channel.clear();
}

/// Outcome of reserving a card slot for a `(channel, task-id)` (#3075 dedupe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CardSlot {
    /// First (or post-eviction / post-stale) sighting: post a NEW card. The
    /// returned `update_count` is 1; the caller must record the posted message
    /// id via [`record_card_message`].
    Post { update_count: u32 },
    /// A live card exists for this task-id WITH a recorded message id: EDIT it in
    /// place rather than posting a new message. `update_count` is the new
    /// (incremented) count.
    Edit { message_id: u64, update_count: u32 },
    /// A card for this task-id has been RESERVED (a [`CardSlot::Post`] was handed
    /// out) but its real message id has not been recorded yet via
    /// [`record_card_message`] — another post for the same task-id is still in
    /// flight. The caller MUST treat this as a no-op (drop this repeat) rather
    /// than attempting to edit: there is no real message id to target, and
    /// constructing `MessageId::new(0)` would panic. The reserved slot is left
    /// intact so the in-flight post still records its id and subsequent repeats
    /// resolve to [`CardSlot::Edit`].
    Pending,
}

/// Reserve the card slot for `(channel_id, task_id)`.
///
/// A task with no usable id always [`CardSlot::Post`]s (it cannot be deduped).
/// An existing, non-stale entry yields [`CardSlot::Edit`] with its message id
/// and an incremented `update_count`; the entry's `touched_at`/`update_count`
/// are advanced so a burst of N notifications collapses to ONE card. A first or
/// stale/evicted entry yields [`CardSlot::Post`] (count 1) and the caller is
/// expected to follow up with [`record_card_message`] once the post lands.
pub(super) fn reserve_card_slot(channel_id: u64, task_id: Option<&str>) -> CardSlot {
    let Some(task_id) = task_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return CardSlot::Post { update_count: 1 };
    };
    let mut store = CARD_STORE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    purge_and_bound(&mut store, channel_id, now);
    let channel = store.by_channel.entry(channel_id).or_default();
    if let Some(entry) = channel.get_mut(task_id) {
        if now.duration_since(entry.touched_at) <= CARD_STALE_AFTER {
            // A reserved-but-not-yet-recorded slot (placeholder message_id == 0)
            // means the first post for this task-id is still in flight. We cannot
            // edit a nonexistent message (and `MessageId::new(0)` panics), so this
            // repeat is dropped as a no-op. The placeholder is left intact and its
            // count is NOT advanced: the in-flight post will record its real id and
            // any later repeat then resolves to `Edit { real_id }`.
            if entry.message_id == 0 {
                return CardSlot::Pending;
            }
            entry.update_count = entry.update_count.saturating_add(1);
            entry.touched_at = now;
            return CardSlot::Edit {
                message_id: entry.message_id,
                update_count: entry.update_count,
            };
        }
        // Stale: fall through to a fresh post (overwritten on record).
    }
    // First sighting (or stale). Insert a placeholder so a tight burst that
    // races before the post commits still collapses; the real message id is
    // written by record_card_message.
    channel.insert(
        task_id.to_string(),
        TaskCardEntry {
            message_id: 0,
            update_count: 1,
            touched_at: now,
        },
    );
    CardSlot::Post { update_count: 1 }
}

/// Record the posted message id for a freshly-posted card so subsequent
/// notifications for the same task-id can EDIT it (#3075).
pub(super) fn record_card_message(channel_id: u64, task_id: Option<&str>, message_id: u64) {
    let Some(task_id) = task_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    if message_id == 0 {
        return;
    }
    let mut store = CARD_STORE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let channel = store.by_channel.entry(channel_id).or_default();
    let entry = channel.entry(task_id.to_string()).or_insert(TaskCardEntry {
        message_id: 0,
        update_count: 1,
        touched_at: now,
    });
    entry.message_id = message_id;
    entry.touched_at = now;
}

/// Convenience over [`record_card_message`] that parses the task-id out of the
/// raw `<task-notification>` payload first (#3075 relay anchor path).
pub(super) fn record_posted_card(channel_id: u64, raw_prompt: &str, message_id: u64) {
    let task_id = parse_task_notification(raw_prompt).task_id;
    record_card_message(channel_id, task_id.as_deref(), message_id);
}

/// Drop the tracked card for a task-id (e.g. after an edit failed because the
/// message is gone) so the next notification posts a fresh card.
pub(super) fn forget_card(channel_id: u64, task_id: Option<&str>) {
    let Some(task_id) = task_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return;
    };
    let mut store = CARD_STORE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(channel) = store.by_channel.get_mut(&channel_id) {
        channel.remove(task_id);
        if channel.is_empty() {
            store.by_channel.remove(&channel_id);
        }
    }
}

/// Clear ONLY a still-reserved placeholder slot for a task-id — i.e. an entry a
/// [`CardSlot::Post`] handed out whose real message id was never recorded
/// (`message_id == 0`) because the first post FAILED (#3075 codex P2).
///
/// Unlike [`forget_card`], this is an EXACT-MATCH on the placeholder we own: if
/// the entry has since recorded a real message id (a concurrent post landed) or
/// was replaced by a newer reservation that already recorded its id, we leave it
/// untouched. This is the failure-path counterpart to [`record_card_message`]:
/// a post either commits its real id (record) or releases its reservation
/// (this), so a later same-task notification reserves fresh and reposts instead
/// of being suppressed as `Pending` until the 1h stale purge.
///
/// Returns `true` if a placeholder was actually cleared.
pub(super) fn forget_reserved_card(channel_id: u64, task_id: Option<&str>) -> bool {
    let Some(task_id) = task_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let mut store = CARD_STORE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(channel) = store.by_channel.get_mut(&channel_id) else {
        return false;
    };
    // Only remove while it is still the unrecorded placeholder (message_id == 0).
    // A non-zero id means a real post committed (ours late, or a concurrent one);
    // leave it so that card keeps deduping repeats.
    let owned_placeholder = channel
        .get(task_id)
        .is_some_and(|entry| entry.message_id == 0);
    if owned_placeholder {
        channel.remove(task_id);
        if channel.is_empty() {
            store.by_channel.remove(&channel_id);
        }
    }
    owned_placeholder
}

/// Purge stale entries for a channel and evict the least-recently-touched cards
/// past the per-channel bound.
fn purge_and_bound(store: &mut TaskCardStore, channel_id: u64, now: Instant) {
    let Some(channel) = store.by_channel.get_mut(&channel_id) else {
        return;
    };
    channel.retain(|_, entry| now.duration_since(entry.touched_at) <= CARD_STALE_AFTER);
    while channel.len() >= MAX_CARDS_PER_CHANNEL {
        if let Some(oldest_key) = channel
            .iter()
            .min_by_key(|(_, entry)| entry.touched_at)
            .map(|(key, _)| key.clone())
        {
            channel.remove(&oldest_key);
        } else {
            break;
        }
    }
}

/// Outcome of resolving a `<task-notification>` against the #3075 dedupe store.
#[derive(Debug)]
pub(super) enum TaskCardOutcome {
    /// First sighting: the caller must POST `content` as a fresh anchor and run
    /// the normal active-turn lifecycle, then record the posted message id via
    /// [`record_posted_card`].
    Post { content: String },
    /// A repeat sighting was handled in place (edited the live card, or dropped
    /// because another post for this task-id is still in flight). The caller must
    /// NOT post a new anchor and must early-return — but, unlike the post path, it
    /// must first clear/resolve any external-input turn lease it recorded for this
    /// observation so a dangling lease cannot block session-bound / bridge-tail
    /// delivery (#3075 codex P1 #2).
    Repeat,
}

/// Render the structured card for a `<task-notification>` and apply the #3075
/// dedupe policy.
///
/// On the first sighting of a task-id this reserves the slot and returns
/// [`TaskCardOutcome::Post`] with the card content; the caller is expected to
/// post it and call [`record_card_message`] / [`record_posted_card`] with the
/// resulting message id. On a repeat sighting it edits the live card in place
/// (or, if that message is gone, forgets it and reposts once; or, if the first
/// post is still in flight, drops the repeat as a no-op) and returns
/// [`TaskCardOutcome::Repeat`].
pub(super) async fn resolve_task_card_content(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    raw_prompt: &str,
) -> TaskCardOutcome {
    let parsed = parse_task_notification(raw_prompt);
    let task_id = parsed.task_id.clone();
    match reserve_card_slot(channel_id.get(), task_id.as_deref()) {
        CardSlot::Post { update_count } => TaskCardOutcome::Post {
            content: format_task_notification_card(&parsed, update_count),
        },
        CardSlot::Pending => {
            // The first post for this task-id is still in flight; another post is
            // racing ahead of `record_posted_card`. There is no real message id to
            // edit yet (the placeholder is 0, and `MessageId::new(0)` would panic),
            // so this repeat is a safe no-op. Treat as a handled repeat so the
            // caller early-returns AND clears its just-recorded lease.
            tracing::debug!(
                channel_id = channel_id.get(),
                task_id = task_id.as_deref().unwrap_or(""),
                "task-notification repeat arrived before first post recorded its id; dropping repeat as no-op"
            );
            TaskCardOutcome::Repeat
        }
        CardSlot::Edit {
            message_id,
            update_count,
        } => {
            let card = format_task_notification_card(&parsed, update_count);
            let edit = super::gateway::edit_outbound_message(
                http.clone(),
                shared.clone(),
                channel_id,
                MessageId::new(message_id),
                &card,
            )
            .await;
            if let Err(error) = edit {
                tracing::debug!(
                    channel_id = channel_id.get(),
                    message_id,
                    error = %error,
                    "task-notification card edit failed; reposting a fresh card"
                );
                forget_card(channel_id.get(), task_id.as_deref());
                if let Ok(message) = channel_id.say(&**http, card).await {
                    record_card_message(channel_id.get(), task_id.as_deref(), message.id.get());
                }
            }
            TaskCardOutcome::Repeat
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(fields: &[(&str, &str)]) -> String {
        let mut body = String::from("<task-notification>");
        for (tag, value) in fields {
            body.push_str(&format!("<{tag}>{value}</{tag}>"));
        }
        body.push_str("</task-notification>");
        body
    }

    #[test]
    fn parses_core_fields_with_both_id_spellings() {
        let hyphen = payload(&[
            ("task-id", "aa37b21a7adafc7c0"),
            ("status", "completed"),
            ("summary", "Implement #3034 part 2"),
            ("result", "Done. See PR."),
        ]);
        let parsed = parse_task_notification(&hyphen);
        assert_eq!(parsed.task_id.as_deref(), Some("aa37b21a7adafc7c0"));
        assert_eq!(parsed.status.as_deref(), Some("completed"));
        assert_eq!(parsed.summary.as_deref(), Some("Implement #3034 part 2"));
        assert_eq!(parsed.result.as_deref(), Some("Done. See PR."));

        let underscore = payload(&[("task_id", "codex-bg-1"), ("status", "completed")]);
        assert_eq!(
            parse_task_notification(&underscore).task_id.as_deref(),
            Some("codex-bg-1")
        );
    }

    #[test]
    fn parser_tolerates_attributes_and_terminal_controls() {
        let raw = "\u{1b}[0m<task-notification kind=\"subagent\">\
            <status>completed</status><summary>hi</summary></task-notification>";
        let parsed = parse_task_notification(raw);
        assert_eq!(parsed.status.as_deref(), Some("completed"));
        assert_eq!(parsed.summary.as_deref(), Some("hi"));
    }

    // #3075 class 1: single subagent completion → card with summary title +
    // PREVIEWED result, NOT the full dump, and PR URL surfaced.
    #[test]
    fn single_subagent_completion_card_previews_not_dumps() {
        let long_result = format!(
            "First useful line about the change.\nSecond line of detail.\n{}\nopened https://github.com/itismyfield/AgentDesk/pull/3034",
            "x".repeat(5000)
        );
        let note = parse_task_notification(&payload(&[
            ("task-id", "aa37b21a7adafc7c0"),
            ("status", "completed"),
            ("summary", "Implement #3034 part 2"),
            ("result", &long_result),
            ("usage", "194k tok"),
            ("duration", "4.1s"),
        ]));
        let card = format_task_notification_card(&note, 1);
        assert!(card.contains("✅"));
        assert!(card.contains("Implement #3034 part 2"));
        assert!(card.contains("First useful line about the change."));
        // The 5000-char filler line must NOT be dumped wholesale.
        assert!(!card.contains(&"x".repeat(5000)));
        assert!(
            card.len() < 1500,
            "card should stay compact, got {}",
            card.len()
        );
        // PR URL surfaced + usage footer.
        assert!(card.contains("https://github.com/itismyfield/AgentDesk/pull/3034"));
        assert!(card.contains("194k tok"));
        assert!(card.contains("task aa37b21a"));
    }

    // #3075 class 2: JSON aggregate result → summarized counts, NO raw JSON.
    #[test]
    fn json_aggregate_result_is_summarized_not_dumped() {
        let json = r#"{"results":[{"issue":1},{"issue":2},{"issue":3}],"reviews":[{"pr":9}]}"#;
        let note = parse_task_notification(&payload(&[
            ("task-id", "wxvufewff0000000"),
            ("status", "completed"),
            ("summary", "triage"),
            ("result", json),
        ]));
        let card = format_task_notification_card(&note, 1);
        assert!(card.contains("3 results"));
        assert!(card.contains("1 reviews"));
        // No raw JSON braces dumped.
        assert!(!card.contains("\"issue\""));
        assert!(!card.contains("[{"));
    }

    // #3075 class 3: 5 repeated completions for the same task-id collapse to ONE
    // card (edit, not 5 posts).
    #[test]
    fn repeated_same_task_id_dedupes_to_single_card() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        let channel = 1_479_671_298_497_183_835_u64;
        let task = Some("aa37b21a7adafc7c0");

        // First: post.
        match reserve_card_slot(channel, task) {
            CardSlot::Post { update_count } => assert_eq!(update_count, 1),
            other => panic!("expected Post, got {other:?}"),
        }
        record_card_message(channel, task, 5001);

        // Next 4: all edit the SAME message id, count climbing 2..=5.
        for expected in 2..=5u32 {
            match reserve_card_slot(channel, task) {
                CardSlot::Edit {
                    message_id,
                    update_count,
                } => {
                    assert_eq!(message_id, 5001, "must edit the first posted card");
                    assert_eq!(update_count, expected);
                }
                other => panic!("expected Edit, got {other:?}"),
            }
        }
        let card = format_task_notification_card(
            &parse_task_notification(&payload(&[
                ("task-id", "aa37b21a7adafc7c0"),
                ("status", "completed"),
                ("summary", "Implement #3034 part 2"),
                ("result", "latest detail"),
            ])),
            5,
        );
        assert!(card.contains("updated 5x"));
        assert!(card.contains("Latest: latest detail"));
    }

    // #3075: a notification that OMITS tool-use-id (and even task-id) must still
    // render; a missing task-id simply cannot dedupe (always Post).
    #[test]
    fn missing_task_id_always_posts_and_never_panics() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        let channel = 42_u64;
        for _ in 0..3 {
            assert_eq!(
                reserve_card_slot(channel, None),
                CardSlot::Post { update_count: 1 },
            );
        }
        // Empty/whitespace id is treated as no id.
        assert_eq!(
            reserve_card_slot(channel, Some("   ")),
            CardSlot::Post { update_count: 1 },
        );

        let note = parse_task_notification(
            "<task-notification><status>completed</status>\
             <summary>no ids here</summary></task-notification>",
        );
        let card = format_task_notification_card(&note, 1);
        assert!(card.contains("no ids here"));
    }

    #[test]
    fn store_is_bounded_per_channel() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        let channel = 7_u64;
        for i in 0..(MAX_CARDS_PER_CHANNEL + 50) {
            let id = format!("task-{i}");
            reserve_card_slot(channel, Some(&id));
            record_card_message(channel, Some(&id), (i as u64) + 1);
        }
        let store = CARD_STORE.lock().unwrap_or_else(|e| e.into_inner());
        let count = store
            .by_channel
            .get(&channel)
            .map(HashMap::len)
            .unwrap_or(0);
        assert!(
            count <= MAX_CARDS_PER_CHANNEL,
            "channel card map must stay bounded, got {count}"
        );
    }

    // #3075 codex P1 #1: the SAME task-id fires twice BEFORE the first post
    // records its real message id. The repeat must NOT resolve to
    // `Edit { message_id: 0 }` (which would feed `MessageId::new(0)` → panic); it
    // must be a `Pending` no-op. After the real id is recorded, a later repeat
    // edits the real card.
    #[test]
    fn repeat_before_first_post_recorded_is_pending_not_edit_zero() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        let channel = 314_159_u64;
        let task = Some("race-task-id");

        // First sighting reserves the slot (placeholder message_id == 0).
        assert_eq!(
            reserve_card_slot(channel, task),
            CardSlot::Post { update_count: 1 },
        );

        // A second notification for the SAME task-id arrives BEFORE
        // record_card_message runs: must be Pending (a safe no-op), never
        // Edit { message_id: 0 }.
        match reserve_card_slot(channel, task) {
            CardSlot::Pending => {}
            CardSlot::Edit { message_id, .. } => {
                panic!(
                    "repeat before record must not Edit; got message_id={message_id} (0 panics MessageId::new)"
                );
            }
            other => panic!("expected Pending, got {other:?}"),
        }
        // A third pre-record repeat is also Pending, and the placeholder count is
        // NOT advanced.
        assert_eq!(reserve_card_slot(channel, task), CardSlot::Pending);

        // The in-flight first post finally records its real id.
        record_card_message(channel, task, 7_777);

        // Now a repeat correctly edits the REAL card; count climbs from 1.
        match reserve_card_slot(channel, task) {
            CardSlot::Edit {
                message_id,
                update_count,
            } => {
                assert_eq!(message_id, 7_777, "must edit the recorded real id");
                assert_eq!(update_count, 2, "first real edit increments to 2");
            }
            other => panic!("expected Edit after record, got {other:?}"),
        }
    }

    // #3075 codex P2: the FIRST post for a task-id FAILS, so its reserved
    // placeholder (message_id == 0) is never recorded. Releasing it via
    // `forget_reserved_card` must let the NEXT same-task notification resolve to
    // `Post` (repost) rather than being suppressed as `Pending` until the 1h
    // stale purge.
    #[test]
    fn failed_first_post_clears_placeholder_so_next_reposts() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        let channel = 271_828_u64;
        let task = Some("post-fail-task");

        // First sighting reserves the placeholder slot.
        assert_eq!(
            reserve_card_slot(channel, task),
            CardSlot::Post { update_count: 1 },
        );

        // Simulate the post FAILING (no record_card_message). Releasing our owned
        // placeholder must report that it cleared something.
        assert!(
            forget_reserved_card(channel, task),
            "an unrecorded placeholder we own must be cleared on post failure"
        );

        // The next notification for the SAME task-id must reserve fresh and POST,
        // NOT resolve to Pending (which would suppress it for up to 1h).
        assert_eq!(
            reserve_card_slot(channel, task),
            CardSlot::Post { update_count: 1 },
            "after a failed first post the placeholder must be gone so the next reposts"
        );
    }

    // #3075 codex P2 race/exact-match guard: `forget_reserved_card` must NEVER
    // evict a slot whose real message id was already recorded — only the still-0
    // placeholder it owns. A late post-failure cleanup that races a concurrent
    // successful post (which recorded a real id) must be a no-op.
    #[test]
    fn forget_reserved_card_preserves_recorded_real_id() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        let channel = 161_803_u64;
        let task = Some("recorded-task");

        reserve_card_slot(channel, task);
        // A concurrent post landed and recorded a real id.
        record_card_message(channel, task, 9_001);

        // A stale failure-path cleanup must NOT clear the recorded card.
        assert!(
            !forget_reserved_card(channel, task),
            "a slot with a recorded real id must not be treated as an owned placeholder"
        );

        // The live card still dedupes repeats (Edit, not Post).
        match reserve_card_slot(channel, task) {
            CardSlot::Edit { message_id, .. } => assert_eq!(message_id, 9_001),
            other => panic!("expected Edit against the preserved real id, got {other:?}"),
        }
    }

    // Missing/empty task-id has no placeholder to clear; must be a harmless no-op.
    #[test]
    fn forget_reserved_card_noop_for_missing_task_id() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        assert!(!forget_reserved_card(1_u64, None));
        assert!(!forget_reserved_card(1_u64, Some("   ")));
        // Unknown task-id on an absent channel is also a no-op.
        assert!(!forget_reserved_card(1_u64, Some("never-seen")));
    }

    #[test]
    fn forget_card_lets_next_notification_post_fresh() {
        let _guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reset_card_store_for_tests();
        let channel = 9_u64;
        let task = Some("ghost-task");
        reserve_card_slot(channel, task);
        record_card_message(channel, task, 800);
        assert!(matches!(
            reserve_card_slot(channel, task),
            CardSlot::Edit { .. }
        ));
        forget_card(channel, task);
        assert_eq!(
            reserve_card_slot(channel, task),
            CardSlot::Post { update_count: 1 },
        );
    }
}
