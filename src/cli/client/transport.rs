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
