use serde_json::Value;

const DEFAULT_API_URL: &str = "http://127.0.0.1:8791";

pub(crate) fn start(channel: u64, key: &str, description: &str) -> Result<(), String> {
    let body = serde_json::json!({
        "key": key,
        "description": description,
    });
    let response = request_json(
        "POST",
        &format!("/api/channels/{channel}/monitoring"),
        Some(body),
    )?;
    print_json(&response);
    Ok(())
}

pub(crate) fn stop(channel: u64, key: &str) -> Result<(), String> {
    let response = request_json(
        "DELETE",
        &format!(
            "/api/channels/{channel}/monitoring/{}",
            encode_path_segment(key)
        ),
        None,
    )?;
    print_json(&response);
    Ok(())
}

fn api_base() -> String {
    let raw = match std::env::var("ADK_API_URL") {
        Ok(value) => value,
        Err(_) => match std::env::var("AGENTDESK_API_URL") {
            Ok(value) => value,
            Err(_) => DEFAULT_API_URL.to_string(),
        },
    };
    raw.trim_end_matches('/').to_string()
}

fn auth_token() -> Option<String> {
    crate::config::load_graceful().server.auth_token
}

fn request_json(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let url = format!("{}{}", api_base(), path);
    let agent = ureq::Agent::new();
    let mut request = match method {
        "POST" => agent.post(&url),
        "DELETE" => agent.delete(&url),
        other => return Err(format!("unsupported monitoring HTTP method: {other}")),
    };

    if let Some(token) = auth_token() {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }

    let response = match body {
        Some(body) => request
            .set("Content-Type", "application/json")
            .send_string(&body.to_string()),
        None => request.call(),
    };

    match response {
        Ok(response) => response
            .into_json()
            .map_err(|error| format!("monitoring API response parse failed: {error}")),
        Err(ureq::Error::Status(code, response)) => {
            let body = match response.into_string() {
                Ok(body) => body,
                Err(_) => String::new(),
            };
            Err(format_monitoring_error(code, &body))
        }
        Err(ureq::Error::Transport(error)) => {
            Err(format!("monitoring API request failed: {error}"))
        }
    }
}

fn format_monitoring_error(code: u16, body: &str) -> String {
    let detail = match serde_json::from_str::<Value>(body).ok().and_then(|value| {
        value
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
    }) {
        Some(error) => error.trim().to_string(),
        None => body.trim().to_string(),
    };

    if detail.is_empty() {
        format!("monitoring API request failed ({code})")
    } else {
        format!("monitoring API request failed ({code}): {detail}")
    }
}

fn print_json(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(rendered) => println!("{rendered}"),
        Err(_) => println!("{value}"),
    }
}

fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_segment_leaves_uuid_like_keys_readable() {
        assert_eq!(encode_path_segment("monitor-123_abc"), "monitor-123_abc");
    }

    #[test]
    fn encode_path_segment_escapes_slashes_and_spaces() {
        assert_eq!(
            encode_path_segment("agent one/monitor"),
            "agent%20one%2Fmonitor"
        );
    }
}
