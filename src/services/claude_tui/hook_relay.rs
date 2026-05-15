use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

const RELAY_TIMEOUT: Duration = Duration::from_secs(2);
const FAILURE_MARKER_SUBDIR: &str = "runtime/claude_tui_hook_relay_failures";

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
    let mut stdin = String::new();
    std::io::stdin()
        .read_to_string(&mut stdin)
        .map_err(|error| format!("read hook stdin: {error}"))?;
    let payload = if stdin.trim().is_empty() {
        Value::Object(Default::default())
    } else {
        serde_json::from_str(&stdin).map_err(|error| format!("parse hook stdin JSON: {error}"))?
    };

    if let Err(error) = relay_hook_event(endpoint, provider, event, session_id, payload) {
        // Claude hooks must not become turn blockers. The receiver path is a
        // boundary signal optimization; transcript tail remains the source of
        // output truth.
        eprintln!("agentdesk claude-hook-relay warning: {error}");
        if let Err(marker_error) =
            record_hook_relay_failure(endpoint, provider, event, session_id, &error)
        {
            eprintln!("agentdesk claude-hook-relay marker warning: {marker_error}");
        }
    }
    println!(r#"{{"suppressOutput":true}}"#);
    Ok(())
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

fn failure_marker_dir() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| root.join(FAILURE_MARKER_SUBDIR))
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
        failure_marker_dir().ok_or_else(|| "runtime root is unavailable".to_string())?;
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
    let rendered = serde_json::to_vec(&marker)
        .map_err(|err| format!("serialize hook relay failure marker: {err}"))?;
    std::fs::write(&marker_path, rendered).map_err(|err| {
        format!(
            "write hook relay failure marker {}: {err}",
            marker_path.display()
        )
    })
}

pub(crate) fn drain_hook_relay_failure_markers(
    provider: &str,
    session_id: &str,
) -> Vec<HookRelayFailureMarker> {
    let Some(marker_dir) = failure_marker_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&marker_dir) else {
        return Vec::new();
    };

    let expected_provider = provider.trim().to_ascii_lowercase();
    let mut markers = Vec::new();
    for entry in entries.flatten().take(512) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        match serde_json::from_str::<HookRelayFailureMarker>(&contents) {
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
                    "invalid claude_tui hook relay failure marker"
                );
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    markers
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
    fn relay_failure_marker_round_trips_for_session() {
        let _guard = ENV_LOCK.lock().unwrap();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let temp_dir = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp_dir.path()) };

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

        match previous_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
        }
    }
}
