use std::io::Read;
use std::time::Duration;

use serde_json::Value;
use url::Url;

const RELAY_TIMEOUT: Duration = Duration::from_secs(2);

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
