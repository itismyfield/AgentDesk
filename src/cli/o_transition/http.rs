//! The one POST `agentdesk o transition` makes: to a loopback base only, carrying that base's own
//! Origin, never following a redirect or an environment proxy, and without any token.

use std::time::Duration;

use serde_json::Value;

const PATH: &str = "/api/o/transition";
/// How long the answer may take once sent; a job's probes end within a minute.
pub(super) const READ: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(super) enum PostError {
    /// The base is not a loopback URL; nothing was dialled.
    NotLoopback,
    /// A 3xx answer, whose target is never followed.
    Redirect(u16),
    Transport(String),
    Body(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLoopback => f.write_str("the API base is not loopback; nothing was sent"),
            Self::Redirect(status) => write!(f, "HTTP {status} redirect, not followed"),
            Self::Transport(error) | Self::Body(error) => f.write_str(error),
        }
    }
}

pub(super) fn post(base: &str, body: &Value, read: Duration) -> Result<(u16, Value), PostError> {
    let base = base.trim_end_matches('/');
    if !crate::utils::loopback_url::is_loopback_url(base, None) {
        return Err(PostError::NotLoopback);
    }
    let origin = url::Url::parse(base).map_err(|_| PostError::NotLoopback)?;
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .try_proxy_from_env(false)
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(read)
        .build();
    let request = agent.post(&format!("{base}{PATH}"));
    let request = request.set("Content-Type", "application/json");
    let request = request.set("Origin", &origin.origin().ascii_serialization());
    let response = match request.send_string(&body.to_string()) {
        Ok(response) | Err(ureq::Error::Status(_, response)) => response,
        Err(ureq::Error::Transport(error)) => return Err(PostError::Transport(error.to_string())),
    };
    let status = response.status();
    if (300..400).contains(&status) {
        return Err(PostError::Redirect(status));
    }
    let text = (response.into_string()).map_err(|error| PostError::Body(error.to_string()))?;
    let json = serde_json::from_str(&text);
    json.map(|json| (status, json))
        .map_err(|_| PostError::Body(format!("HTTP {status}: {text}")))
}
