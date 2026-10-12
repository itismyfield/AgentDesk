//! Shared authenticated CLI transport with an explicit target.
use super::*;

pub(super) fn request_json(method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
    request_json_at(&api_base(), method, path, body)
}

/// Use a caller-resolved target for local startup diagnostics as well as CLI requests.
pub(crate) fn get_json_at(base: &str, path: &str) -> Result<Value, String> {
    request_json_at(base, "GET", path, None)
}

fn request_json_at(
    base: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<Value, String> {
    let url = if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    };

    let a = agent();
    let mut req = match method.to_uppercase().as_str() {
        "GET" => a.get(&url),
        "POST" => a.post(&url),
        "PATCH" => a.patch(&url),
        "PUT" => a.put(&url),
        "DELETE" => a.delete(&url),
        other => return Err(format!("Unsupported method: {other}")),
    };
    if let Some(token) = auth_token() {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }

    let method_upper = method.to_ascii_uppercase();
    let resp = if let Some(b) = body {
        req.set("Content-Type", "application/json").send_string(b)
    } else if matches!(method_upper.as_str(), "POST" | "PATCH" | "PUT") {
        req.set("Content-Type", "application/json")
            .send_string("{}")
    } else {
        req.call()
    };

    let resp = match resp {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            return Err(status_error_message(code, &body));
        }
        Err(ureq::Error::Transport(err)) => {
            return Err(connection_error_hint(
                &format!("Request failed: {err}"),
                base,
                "AGENTDESK_API_URL",
            ));
        }
    };

    resp.into_json().map_err(|e| format!("Parse error: {e}"))
}

/// Why a local-only POST got no usable answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LocalPostError {
    /// The URL is not loopback; nothing was dialled.
    NotLoopback,
    /// A 3xx answer; its target is never followed.
    Redirect(u16),
    Transport(String),
    Body(String),
}

/// How long the CLI waits for the server's answer once the request is sent.
pub(crate) const LOCAL_POST_READ: std::time::Duration = std::time::Duration::from_secs(120);

/// One POST to `base` + `path`, only when that URL is loopback: no redirect, no proxy, bounded
/// waits. A 4xx/5xx JSON body comes back with its status so the caller can show it.
pub(crate) fn post_json_local_at(
    base: &str,
    path: &str,
    body: &Value,
) -> Result<(u16, Value), LocalPostError> {
    post_local(base, path, body, LOCAL_POST_READ)
}

pub(crate) fn post_local(
    base: &str,
    path: &str,
    body: &Value,
    read_timeout: std::time::Duration,
) -> Result<(u16, Value), LocalPostError> {
    let url = format!("{base}{path}");
    if !crate::utils::loopback_url::is_loopback_url(&url, None) {
        return Err(LocalPostError::NotLoopback);
    }
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .try_proxy_from_env(false)
        .timeout_connect(std::time::Duration::from_secs(5))
        .timeout_read(read_timeout)
        .build();
    let mut request = agent.post(&url).set("Content-Type", "application/json");
    if let Some(token) = auth_token().filter(|token| !token.is_empty()) {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = match request.send_string(&body.to_string()) {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(error)) => {
            let hint = format!("Request failed: {error}");
            let hint = connection_error_hint(&hint, base, "AGENTDESK_API_URL");
            return Err(LocalPostError::Transport(hint));
        }
    };
    let status = response.status();
    if (300..400).contains(&status) {
        return Err(LocalPostError::Redirect(status));
    }
    let text = response
        .into_string()
        .map_err(|error| LocalPostError::Body(error.to_string()))?;
    serde_json::from_str(&text)
        .map(|json| (status, json))
        .map_err(|_| LocalPostError::Body(status_error_message(status, &text)))
}
