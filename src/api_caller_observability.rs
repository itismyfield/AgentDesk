use axum::http::HeaderMap;

pub const LOG_TARGET: &str = "agentdesk::api_caller_observability";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStrength {
    None,
    Loopback,
    ServerAdmin,
}

impl AuthStrength {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Loopback => "Loopback",
            Self::ServerAdmin => "ServerAdmin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPrincipal {
    pub auth_strength: AuthStrength,
    pub claimed_agent_id: Option<String>,
    pub claimed_channel_id: Option<String>,
}

impl RequestPrincipal {
    pub fn from_headers(headers: &HeaderMap, auth_strength: AuthStrength) -> Self {
        Self {
            auth_strength,
            claimed_agent_id: trimmed_header_value(headers, "x-agent-id"),
            claimed_channel_id: trimmed_header_value(headers, "x-channel-id"),
        }
    }
}

pub fn manager_channel_check_relied_on_claimed_header(
    headers: &HeaderMap,
    expected_channel_id: Option<&str>,
) -> bool {
    let Some(expected_channel_id) = expected_channel_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    trimmed_header_value(headers, "x-channel-id").as_deref() == Some(expected_channel_id)
}

pub fn log_identity_consumption(
    endpoint: &'static str,
    principal: Option<&RequestPrincipal>,
    consumed_agent_id: Option<&str>,
    manager_channel_check_relied_on_claimed_header: bool,
) {
    let auth_strength = principal
        .map(|principal| principal.auth_strength.as_str())
        .unwrap_or(AuthStrength::None.as_str());
    let claimed_agent_id = principal
        .and_then(|principal| principal.claimed_agent_id.as_deref())
        .unwrap_or("");
    let claimed_channel_id = principal
        .and_then(|principal| principal.claimed_channel_id.as_deref())
        .unwrap_or("");
    let consumed_agent_id = consumed_agent_id.unwrap_or("");

    tracing::info!(
        target: LOG_TARGET,
        endpoint = endpoint,
        auth_strength = auth_strength,
        claimed_agent_id = claimed_agent_id,
        claimed_channel_id = claimed_channel_id,
        consumed_agent_id = consumed_agent_id,
        manager_channel_check_relied_on_claimed_header =
            manager_channel_check_relied_on_claimed_header,
        "api caller identity consumed"
    );
}

fn trimmed_header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::writer::MakeWriter;

    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn request_principal_classifies_loopback_bearer_and_none() {
        let mut headers = HeaderMap::new();
        headers.insert("x-agent-id", HeaderValue::from_static("codex"));
        headers.insert("x-channel-id", HeaderValue::from_static("channel-1"));
        headers.insert("authorization", HeaderValue::from_static("Bearer secret"));

        let loopback = RequestPrincipal::from_headers(&headers, AuthStrength::Loopback);
        assert_eq!(loopback.auth_strength, AuthStrength::Loopback);
        assert_eq!(loopback.claimed_agent_id.as_deref(), Some("codex"));
        assert_eq!(loopback.claimed_channel_id.as_deref(), Some("channel-1"));

        let bearer = RequestPrincipal::from_headers(&headers, AuthStrength::ServerAdmin);
        assert_eq!(bearer.auth_strength, AuthStrength::ServerAdmin);

        let none = RequestPrincipal::from_headers(&headers, AuthStrength::None);
        assert_eq!(none.auth_strength, AuthStrength::None);
    }

    #[test]
    fn manager_channel_flag_requires_matching_claimed_header() {
        let mut headers = HeaderMap::new();
        assert!(!manager_channel_check_relied_on_claimed_header(
            &headers,
            Some("manager-channel")
        ));

        headers.insert("x-channel-id", HeaderValue::from_static("other-channel"));
        assert!(!manager_channel_check_relied_on_claimed_header(
            &headers,
            Some("manager-channel")
        ));

        headers.insert("x-channel-id", HeaderValue::from_static("manager-channel"));
        assert!(manager_channel_check_relied_on_claimed_header(
            &headers,
            Some("manager-channel")
        ));
        assert!(!manager_channel_check_relied_on_claimed_header(
            &headers, None
        ));
    }

    #[test]
    fn log_identity_consumption_emits_expected_fields_without_authorization() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturingWriter {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_target(true)
            .with_writer(writer)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let principal = RequestPrincipal {
            auth_strength: AuthStrength::ServerAdmin,
            claimed_agent_id: Some("codex".to_string()),
            claimed_channel_id: Some("manager-channel".to_string()),
        };
        log_identity_consumption(
            "POST /api/test",
            Some(&principal),
            Some("resolved-codex"),
            true,
        );
        drop(_guard);

        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(logs.contains(LOG_TARGET), "logs={logs}");
        assert!(logs.contains("endpoint=\"POST /api/test\""), "logs={logs}");
        assert!(
            logs.contains("auth_strength=\"ServerAdmin\""),
            "logs={logs}"
        );
        assert!(logs.contains("claimed_agent_id=\"codex\""), "logs={logs}");
        assert!(
            logs.contains("claimed_channel_id=\"manager-channel\""),
            "logs={logs}"
        );
        assert!(
            logs.contains("consumed_agent_id=\"resolved-codex\""),
            "logs={logs}"
        );
        assert!(
            logs.contains("manager_channel_check_relied_on_claimed_header=true"),
            "logs={logs}"
        );
        assert!(
            !logs.to_ascii_lowercase().contains("authorization"),
            "logs={logs}"
        );
    }
}
