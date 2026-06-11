use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

const RELAY_TIMEOUT: Duration = Duration::from_secs(2);
const FAILURE_MARKER_TTL_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HookRelayFailureMarker {
    pub provider: String,
    pub event: String,
    pub session_id: String,
    pub endpoint: String,
    pub error: String,
    pub recorded_at: DateTime<Utc>,
}

pub fn run_cli(
    endpoint: &str,
    provider: &str,
    event: &str,
    session_id: &str,
) -> Result<(), String> {
    run_cli_with_name(
        endpoint,
        provider,
        event,
        session_id,
        relay_cli_name(provider),
    )
}

fn run_cli_with_name(
    endpoint: &str,
    provider: &str,
    event: &str,
    session_id: &str,
    relay_name: &str,
) -> Result<(), String> {
    let mut stdin = String::new();
    std::io::stdin()
        .read_to_string(&mut stdin)
        .map_err(|error| format!("read hook stdin: {error}"))?;
    let payload = if stdin.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&stdin).map_err(|error| format!("parse hook stdin JSON: {error}"))?
    };

    let effective_session_id = relay_event_session_id(provider, session_id, &payload);
    // Compute the hook stdout (which may carry a tool_feedback nudge for memento
    // searches) before `payload` is moved into the relay POST below.
    let stdout = hook_stdout(provider, event, &payload);
    if let Err(error) = relay_hook_event(endpoint, provider, event, &effective_session_id, payload)
    {
        // Provider TUI hooks must not become turn blockers. The receiver path
        // is a boundary signal optimization; provider output capture remains
        // the source of output truth.
        eprintln!("agentdesk {relay_name} warning: {error}");
        if let Err(marker_error) =
            record_hook_relay_failure(endpoint, provider, event, &effective_session_id, &error)
        {
            eprintln!("agentdesk {relay_name} marker warning: {marker_error}");
        }
    }
    println!("{stdout}");
    Ok(())
}

fn relay_event_session_id(provider: &str, command_session_id: &str, payload: &Value) -> String {
    if provider.trim().eq_ignore_ascii_case("codex")
        && let Some(payload_session_id) = payload_session_id(payload)
    {
        return payload_session_id;
    }
    command_session_id.to_string()
}

fn payload_session_id(payload: &Value) -> Option<String> {
    for key in ["session_id", "sessionId", "sessionID"] {
        if let Some(session_id) = payload
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(session_id.to_string());
        }
    }
    payload.get("payload").and_then(payload_session_id)
}

fn relay_cli_name(provider: &str) -> &'static str {
    match provider.trim().to_ascii_lowercase().as_str() {
        "codex" => "codex-hook-relay",
        _ => "claude-hook-relay",
    }
}

fn hook_success_stdout(provider: &str) -> &'static str {
    match provider.trim().to_ascii_lowercase().as_str() {
        // Codex parses suppressOutput for some events but does not implement it
        // consistently yet; return an empty success object so managed relay
        // hooks stay observational.
        "codex" => "{}",
        _ => r#"{"suppressOutput":true}"#,
    }
}

/// Stdout returned to the provider TUI for a relayed hook event.
///
/// Defaults to the observational `hook_success_stdout`, but for a memento
/// `PostToolUse` search (`recall`/`context`) it injects an immediate
/// `tool_feedback` nudge via `hookSpecificOutput.additionalContext`. The
/// memory search→feedback ratio was effectively zero because models ignore the
/// advisory `_meta.hints`; surfacing the ask in the model-visible PostToolUse
/// context closes that loop without touching the Memento server or the prompt.
///
/// Claude and Codex use different hook stdout contracts: Claude keeps
/// `suppressOutput: true`, while Codex CLI 0.137.0 expects only the
/// `hookSpecificOutput` block. Codex non-nudge events, including `Stop`, remain
/// observational `{}`.
///
/// Session-end feedback flushing is not implemented in the hook relay; that
/// path is deferred to #3332 for a server-side design.
fn hook_stdout(provider: &str, event: &str, payload: &Value) -> String {
    let provider_key = provider.trim().to_ascii_lowercase();
    let event_is_post_tool_use = event.trim().eq_ignore_ascii_case("PostToolUse");
    if event_is_post_tool_use && memento_search_tool_name(payload).is_some() {
        let additional_context = memento_feedback_instruction(extract_search_event_id(payload));
        return hook_specific_stdout(&provider_key, "PostToolUse", additional_context);
    }
    hook_success_stdout(provider).to_string()
}

fn hook_specific_stdout(
    provider_key: &str,
    event_name: &str,
    additional_context: String,
) -> String {
    if provider_key == "codex" {
        return json!({
            "hookSpecificOutput": {
                "hookEventName": event_name,
                "additionalContext": additional_context,
            }
        })
        .to_string();
    }
    json!({
        "suppressOutput": true,
        "hookSpecificOutput": {
            "hookEventName": event_name,
            "additionalContext": additional_context,
        }
    })
    .to_string()
}

/// Returns the lowercased tool name when the PostToolUse payload is a memento
/// search (`recall` or `context` — the two tools that return a
/// `_meta.searchEventId` eligible for `tool_feedback`). `remember`, `forget`,
/// and the rest are excluded.
fn memento_search_tool_name(payload: &Value) -> Option<String> {
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .map(|name| name.trim().to_ascii_lowercase())?;
    if !tool_name.contains("memento") {
        return None;
    }
    // Match the trailing tool segment exactly so both `mcp__memento__recall` and
    // the dotted `memento.recall` form qualify, while `recall_context_combined`
    // or a non-memento server's `recall` do not.
    let leaf = tool_name
        .rsplit(|c| c == '_' || c == '.')
        .next()
        .unwrap_or("");
    matches!(leaf, "recall" | "context").then_some(tool_name)
}

/// Best-effort extraction of `searchEventId` from the PostToolUse payload.
///
/// memento returns it under `_meta.searchEventId`, but in the hook payload the
/// recall result may be nested inside a stringified MCP text block, so a
/// structural lookup is unreliable. Scan the serialized `tool_response` (then
/// the whole payload) for the first integer following the `searchEventId`
/// marker. Returns `None` when absent — the nudge still fires, just without an
/// explicit id (the model has the id in its own recall result).
fn extract_search_event_id(payload: &Value) -> Option<String> {
    for hay in [payload.get("tool_response"), Some(payload)]
        .into_iter()
        .flatten()
    {
        if let Some(id) = scan_search_event_id(&hay.to_string()) {
            return Some(id);
        }
    }
    None
}

fn scan_search_event_id(serialized: &str) -> Option<String> {
    // Anchor on `searchEventId` as a JSON *key*: the marker must be followed by
    // an (optionally backslash-escaped) closing quote and a `:`, then the value
    // digits. This rejects longer keys (`searchEventIdHash`), bare-word mentions
    // inside fragment text, and `null`/empty values — each of which would
    // otherwise let a greedy digit scan capture an unrelated number. Multiple
    // occurrences are tried so a non-matching first hit doesn't abort the scan.
    let marker = "searchEventId";
    let mut haystack = serialized;
    loop {
        let rel = haystack.find(marker)?;
        let after = &haystack[rel + marker.len()..];
        let key_tail = after
            .strip_prefix("\\\"")
            .or_else(|| after.strip_prefix('"'))
            .unwrap_or(after)
            .trim_start();
        if let Some(value_part) = key_tail.strip_prefix(':') {
            let value = value_part.trim_start();
            let value = value
                .strip_prefix("\\\"")
                .or_else(|| value.strip_prefix('"'))
                .unwrap_or(value);
            let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                return Some(digits);
            }
        }
        haystack = after;
    }
}

fn memento_feedback_instruction(search_event_id: Option<String>) -> String {
    let target = match search_event_id {
        Some(id) => format!("search_event_id={id}"),
        None => "the search_event_id shown under `_meta.searchEventId` in that result".to_string(),
    };
    format!(
        "Action required: you just received a memento search result. Submit one \
`mcp__memento__tool_feedback` call immediately for THIS result with \
{target}, `relevant` = whether any returned fragment was on-topic, and `sufficient` = whether the \
results were enough to proceed. If `mcp__memento__tool_feedback` is not in your active tools \
(memento tools are deferred), first load it with ToolSearch query \
`select:mcp__memento__tool_feedback`, then make the call. Do this now, then continue."
    )
}

pub fn relay_hook_event(
    endpoint: &str,
    provider: &str,
    event: &str,
    session_id: &str,
    payload: Value,
) -> Result<(), String> {
    let url = hook_url(endpoint, provider, event, session_id)?;
    let agent = ureq::AgentBuilder::new().timeout(RELAY_TIMEOUT).build();
    let response = agent
        .post(url.as_str())
        .set("Content-Type", "application/json")
        .send_json(payload)
        .map_err(|error| format!("post hook event: {error}"))?;
    if (200..300).contains(&response.status()) {
        Ok(())
    } else {
        Err(format!("hook receiver returned HTTP {}", response.status()))
    }
}

fn hook_url(endpoint: &str, provider: &str, event: &str, session_id: &str) -> Result<Url, String> {
    let mut url =
        Url::parse(endpoint).map_err(|error| format!("parse hook endpoint {endpoint}: {error}"))?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "hook endpoint cannot be a base URL".to_string())?;
        segments.clear();
        segments.push("hooks");
        segments.push(provider);
        segments.push(event);
    }
    url.query_pairs_mut()
        .clear()
        .append_pair("session_id", session_id);
    Ok(url)
}

fn failure_marker_subdir(provider: &str) -> String {
    let provider = marker_component(&provider.trim().to_ascii_lowercase());
    format!("runtime/{provider}_tui_hook_relay_failures")
}

fn failure_marker_dir(provider: &str) -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join(failure_marker_subdir(provider)))
}

fn marker_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn record_hook_relay_failure(
    endpoint: &str,
    provider: &str,
    event: &str,
    session_id: &str,
    error: &str,
) -> Result<(), String> {
    let marker_dir =
        failure_marker_dir(provider).ok_or_else(|| "runtime root is unavailable".to_string())?;
    std::fs::create_dir_all(&marker_dir)
        .map_err(|err| format!("create hook relay failure marker dir: {err}"))?;

    let marker = HookRelayFailureMarker {
        provider: provider.trim().to_ascii_lowercase(),
        event: event.to_string(),
        session_id: session_id.to_string(),
        endpoint: endpoint.to_string(),
        error: error.to_string(),
        recorded_at: Utc::now(),
    };
    let filename = format!(
        "{}-{}-{}-{}.json",
        marker_component(session_id),
        marker_component(event),
        marker.recorded_at.timestamp_millis(),
        uuid::Uuid::new_v4().simple()
    );
    let marker_path = marker_dir.join(filename);
    let temp_path =
        marker_path.with_extension(format!("json.tmp.{}", uuid::Uuid::new_v4().simple()));
    let rendered = serde_json::to_vec(&marker)
        .map_err(|err| format!("serialize hook relay failure marker: {err}"))?;
    std::fs::write(&temp_path, rendered).map_err(|err| {
        format!(
            "write hook relay failure marker temp {}: {err}",
            temp_path.display()
        )
    })?;
    std::fs::rename(&temp_path, &marker_path).map_err(|err| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "publish hook relay failure marker {}: {err}",
            marker_path.display()
        )
    })?;
    Ok(())
}

pub(crate) fn drain_hook_relay_failure_markers(
    provider: &str,
    session_id: &str,
) -> Vec<HookRelayFailureMarker> {
    drain_hook_relay_failure_markers_at(provider, session_id, Utc::now())
}

fn drain_hook_relay_failure_markers_at(
    provider: &str,
    session_id: &str,
    now: DateTime<Utc>,
) -> Vec<HookRelayFailureMarker> {
    let Some(marker_dir) = failure_marker_dir(provider) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&marker_dir) else {
        return Vec::new();
    };

    let expected_provider = provider.trim().to_ascii_lowercase();
    let mut markers = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !is_failure_marker_path(&path) {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<HookRelayFailureMarker>(&contents) {
            Ok(marker) if marker_is_stale(&marker, now) => {
                let _ = std::fs::remove_file(&path);
            }
            Ok(marker)
                if marker.provider == expected_provider && marker.session_id == session_id =>
            {
                let _ = std::fs::remove_file(&path);
                markers.push(marker);
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    provider = expected_provider,
                    "invalid tui hook relay failure marker"
                );
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    markers
}

fn is_failure_marker_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension == "json")
}

fn marker_is_stale(marker: &HookRelayFailureMarker, now: DateTime<Utc>) -> bool {
    now.signed_duration_since(marker.recorded_at)
        > chrono::Duration::seconds(FAILURE_MARKER_TTL_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::MutexGuard;

    /// Guard that serializes every test mutation of `AGENTDESK_ROOT_DIR`
    /// against the process-global lock shared with `credential.rs`, the
    /// integration harness, and other env-touching tests. Without this
    /// cross-module lock, concurrent tests would race on the same env var
    /// and intermittently observe each other's tempdirs (issue #2444).
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvVarGuard {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let lock = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self {
                key,
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn hook_url_routes_to_provider_event_with_session_query() {
        let url = hook_url(
            "http://127.0.0.1:49152/base",
            "claude",
            "Stop",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:49152/hooks/claude/Stop?session_id=01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[test]
    fn hook_url_percent_encodes_path_segments() {
        let url = hook_url("http://127.0.0.1:1", "claude tui", "Stop Hook", "sid 1").unwrap();

        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:1/hooks/claude%20tui/Stop%20Hook?session_id=sid+1"
        );
    }

    #[test]
    fn hook_success_stdout_is_provider_scoped() {
        assert_eq!(hook_success_stdout("claude"), r#"{"suppressOutput":true}"#);
        assert_eq!(hook_success_stdout("codex"), "{}");
    }

    #[test]
    fn claude_posttooluse_memento_recall_injects_feedback_nudge_with_id() {
        let payload = serde_json::json!({
            "tool_name": "mcp__memento__recall",
            "tool_response": [{
                "type": "text",
                "text": "{\"fragments\":[],\"_meta\":{\"searchEventId\":\"22752\"}}"
            }]
        });
        let out = hook_stdout("claude", "PostToolUse", &payload);
        let value: Value = serde_json::from_str(&out).unwrap();
        let ctx = value["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(ctx.contains("search_event_id=22752"));
        assert!(ctx.contains("mcp__memento__tool_feedback"));
        assert!(ctx.contains("immediately"));
        let delayed_framing = ["Before", " your next"].concat();
        assert!(!ctx.contains(&delayed_framing));
        // Deferred-tool friction hint: tell the model how to load the tool.
        assert!(ctx.contains("select:mcp__memento__tool_feedback"));
        assert_eq!(value["suppressOutput"], true);
    }

    #[test]
    fn codex_posttooluse_memento_recall_injects_feedback_nudge_without_suppress_output() {
        let payload = serde_json::json!({
            "tool_name": "mcp__memento__recall",
            "tool_response": [{
                "type": "text",
                "text": "{\"fragments\":[],\"_meta\":{\"searchEventId\":\"3300\"}}"
            }]
        });
        let out = hook_stdout("codex", "PostToolUse", &payload);
        let value: Value = serde_json::from_str(&out).unwrap();
        let ctx = value["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert_eq!(value["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert!(ctx.contains("search_event_id=3300"));
        assert!(ctx.contains("mcp__memento__tool_feedback"));
        assert!(ctx.contains("immediately"));
        assert!(value.get("suppressOutput").is_none());
    }

    #[test]
    fn claude_posttooluse_memento_context_injects_even_without_extractable_id() {
        // tool is a memento search but the payload carries no searchEventId:
        // still nudge, deferring the id to the model's own recall result.
        let payload = serde_json::json!({ "tool_name": "mcp__memento__context" });
        let out = hook_stdout("claude", "PostToolUse", &payload);
        let value: Value = serde_json::from_str(&out).unwrap();
        let ctx = value["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("_meta.searchEventId"));
        assert!(ctx.contains("mcp__memento__tool_feedback"));
        assert!(!ctx.contains("search_event_id="));
    }

    #[test]
    fn claude_stop_stays_observational_with_suppress_output() {
        assert_eq!(
            hook_stdout("claude", "Stop", &serde_json::json!({})),
            r#"{"suppressOutput":true}"#
        );
    }

    #[test]
    fn non_search_and_non_posttooluse_stay_observational() {
        // Wrong event.
        let recall = serde_json::json!({ "tool_name": "mcp__memento__recall" });
        assert_eq!(
            hook_stdout("claude", "PreToolUse", &recall),
            r#"{"suppressOutput":true}"#
        );
        // Right event, non-search memento tool.
        let forget = serde_json::json!({ "tool_name": "mcp__memento__forget" });
        assert_eq!(
            hook_stdout("claude", "PostToolUse", &forget),
            r#"{"suppressOutput":true}"#
        );
        // Codex non-nudge events keep the established empty success object.
        assert_eq!(hook_stdout("codex", "PreToolUse", &recall), "{}");
        assert_eq!(hook_stdout("codex", "Stop", &serde_json::json!({})), "{}");
    }

    #[test]
    fn scan_search_event_id_handles_escaped_and_plain_forms() {
        assert_eq!(
            scan_search_event_id(r#"{"_meta":{"searchEventId":"22752"}}"#).as_deref(),
            Some("22752")
        );
        assert_eq!(
            scan_search_event_id(r#"...\"searchEventId\":\"4310\"..."#).as_deref(),
            Some("4310")
        );
        assert_eq!(
            scan_search_event_id(r#"{"searchEventId":981}"#).as_deref(),
            Some("981")
        );
        assert_eq!(scan_search_event_id(r#"{"other":"1"}"#), None);
    }

    #[test]
    fn scan_search_event_id_rejects_false_positives() {
        // Longer key that merely starts with the marker.
        assert_eq!(scan_search_event_id(r#"{"searchEventIdHash":"99"}"#), None);
        // Bare-word mention inside fragment text (no key colon follows).
        assert_eq!(
            scan_search_event_id(r#"{"text":"the searchEventId was 4242 last time"}"#),
            None
        );
        // Null / empty values are not captured.
        assert_eq!(scan_search_event_id(r#"{"searchEventId":null}"#), None);
        assert_eq!(scan_search_event_id(r#"{"searchEventId":""}"#), None);
        // A non-matching first occurrence must not abort the scan: the real key
        // (with a numeric value) appears after a longer-key decoy.
        assert_eq!(
            scan_search_event_id(r#"{"searchEventIdHash":"zz","searchEventId":"77"}"#).as_deref(),
            Some("77")
        );
    }

    #[test]
    fn memento_search_tool_name_matches_both_forms_and_rejects_lookalikes() {
        let target =
            |name: &str| memento_search_tool_name(&serde_json::json!({ "tool_name": name }));
        assert!(target("mcp__memento__recall").is_some());
        assert!(target("mcp__memento__context").is_some());
        assert!(target("memento.recall").is_some());
        // Lookalikes / other servers must not match.
        assert!(target("mcp__memento__recall_context_combined").is_none());
        assert!(target("mcp__memento__forget").is_none());
        assert!(target("mcp__other__recall").is_none());
    }

    #[test]
    fn codex_relay_uses_payload_session_id_over_stable_command_identity() {
        let payload = serde_json::json!({
            "session_id": "actual-codex-session",
            "transcript_path": "/tmp/ignored"
        });

        assert_eq!(
            relay_event_session_id("codex", "agentdesk-codex-hook-relay", &payload),
            "actual-codex-session"
        );
    }

    #[test]
    fn claude_relay_keeps_command_session_id_even_when_payload_has_session() {
        let payload = serde_json::json!({
            "session_id": "payload-session"
        });

        assert_eq!(
            relay_event_session_id("claude", "command-session", &payload),
            "command-session"
        );
    }

    #[test]
    fn relay_failure_marker_directories_are_provider_scoped() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp_dir.path());

        assert_ne!(
            failure_marker_dir("claude").unwrap(),
            failure_marker_dir("codex").unwrap()
        );
    }

    #[test]
    fn relay_failure_marker_round_trips_for_session() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp_dir.path());

        record_hook_relay_failure(
            "http://127.0.0.1:49152",
            "Claude",
            "Stop",
            "session-1",
            "post hook event: connection refused",
        )
        .unwrap();

        let markers = drain_hook_relay_failure_markers("claude", "session-1");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].provider, "claude");
        assert_eq!(markers[0].event, "Stop");
        assert_eq!(markers[0].session_id, "session-1");
        assert_eq!(
            markers[0].error,
            "post hook event: connection refused".to_string()
        );
        assert!(drain_hook_relay_failure_markers("claude", "session-1").is_empty());
    }

    #[test]
    fn relay_failure_marker_write_publishes_only_complete_json_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp_dir.path());

        record_hook_relay_failure(
            "http://127.0.0.1:49152",
            "claude",
            "Stop",
            "session-1",
            "post hook event: connection refused",
        )
        .unwrap();

        let marker_dir = failure_marker_dir("claude").unwrap();
        let entries = std::fs::read_dir(marker_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert!(is_failure_marker_path(&entries[0]));
        let marker = serde_json::from_str::<HookRelayFailureMarker>(
            &std::fs::read_to_string(&entries[0]).unwrap(),
        )
        .unwrap();
        assert_eq!(marker.session_id, "session-1");
    }

    #[test]
    fn drain_prunes_stale_unmatched_markers() {
        let temp_dir = tempfile::tempdir().unwrap();
        let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp_dir.path());
        let marker_dir = failure_marker_dir("claude").unwrap();
        std::fs::create_dir_all(&marker_dir).unwrap();
        let stale_marker = HookRelayFailureMarker {
            provider: "claude".to_string(),
            event: "Stop".to_string(),
            session_id: "stale-session".to_string(),
            endpoint: "http://127.0.0.1:49152".to_string(),
            error: "post hook event: connection refused".to_string(),
            recorded_at: Utc::now() - chrono::Duration::seconds(FAILURE_MARKER_TTL_SECS + 1),
        };
        let stale_path = marker_dir.join("stale.json");
        std::fs::write(&stale_path, serde_json::to_vec(&stale_marker).unwrap()).unwrap();

        assert!(drain_hook_relay_failure_markers_at("claude", "session-1", Utc::now()).is_empty());
        assert!(!stale_path.exists());
    }
}
