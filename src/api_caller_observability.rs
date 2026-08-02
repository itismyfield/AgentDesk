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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IdentityConsumptionFields {
    endpoint: &'static str,
    auth_strength: &'static str,
    claimed_agent_id: String,
    claimed_channel_id: String,
    consumed_agent_id: String,
    manager_channel_check_relied_on_claimed_header: bool,
}

impl IdentityConsumptionFields {
    /// Return only the fields built by `identity_consumption_fields`.
    ///
    /// This projection lets tests inspect a field set they construct, without
    /// formatted tracing capture. Two things sit outside the guard: it does not
    /// observe the emitted event, so a field added directly to `tracing::info!`
    /// is invisible here; and a caller that builds its own arguments is not
    /// checking what any production call site passed. Closing the first gap
    /// needs emission and projection generated from one declarative field list;
    /// the second needs a seam reporting emitted values back from production.
    /// Both are tracked as sites on umbrella #5003.
    #[cfg(test)]
    pub(crate) fn named_values(&self) -> Vec<(&'static str, String)> {
        vec![
            ("endpoint", self.endpoint.to_string()),
            ("auth_strength", self.auth_strength.to_string()),
            ("claimed_agent_id", self.claimed_agent_id.clone()),
            ("claimed_channel_id", self.claimed_channel_id.clone()),
            ("consumed_agent_id", self.consumed_agent_id.clone()),
            (
                "manager_channel_check_relied_on_claimed_header",
                self.manager_channel_check_relied_on_claimed_header
                    .to_string(),
            ),
        ]
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

pub(crate) fn identity_consumption_fields(
    endpoint: &'static str,
    principal: Option<&RequestPrincipal>,
    consumed_agent_id: Option<&str>,
    manager_channel_check_relied_on_claimed_header: bool,
) -> IdentityConsumptionFields {
    IdentityConsumptionFields {
        endpoint,
        auth_strength: principal
            .map(|principal| principal.auth_strength.as_str())
            .unwrap_or(AuthStrength::None.as_str()),
        claimed_agent_id: principal
            .and_then(|principal| principal.claimed_agent_id.as_deref())
            .unwrap_or("")
            .to_string(),
        claimed_channel_id: principal
            .and_then(|principal| principal.claimed_channel_id.as_deref())
            .unwrap_or("")
            .to_string(),
        consumed_agent_id: consumed_agent_id.unwrap_or("").to_string(),
        manager_channel_check_relied_on_claimed_header,
    }
}

pub fn log_identity_consumption(
    endpoint: &'static str,
    principal: Option<&RequestPrincipal>,
    consumed_agent_id: Option<&str>,
    manager_channel_check_relied_on_claimed_header: bool,
) {
    emit_identity_consumption(identity_consumption_fields(
        endpoint,
        principal,
        consumed_agent_id,
        manager_channel_check_relied_on_claimed_header,
    ));
}

pub(crate) fn emit_identity_consumption(fields: IdentityConsumptionFields) {
    tracing::info!(
        target: LOG_TARGET,
        endpoint = fields.endpoint,
        auth_strength = fields.auth_strength,
        claimed_agent_id = fields.claimed_agent_id.as_str(),
        claimed_channel_id = fields.claimed_channel_id.as_str(),
        consumed_agent_id = fields.consumed_agent_id.as_str(),
        manager_channel_check_relied_on_claimed_header = fields
            .manager_channel_check_relied_on_claimed_header,
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
    fn identity_consumption_fields_projection_excludes_authorization() {
        let principal = RequestPrincipal {
            auth_strength: AuthStrength::ServerAdmin,
            claimed_agent_id: Some("codex".to_string()),
            claimed_channel_id: Some("manager-channel".to_string()),
        };
        let fields = identity_consumption_fields(
            "POST /api/test",
            Some(&principal),
            Some("resolved-codex"),
            true,
        );

        for (name, value) in fields.named_values() {
            let field = format!("{name}={value}");
            assert!(
                !field.to_ascii_lowercase().contains("authorization"),
                "field={field}"
            );
        }
        assert_eq!(
            fields,
            IdentityConsumptionFields {
                endpoint: "POST /api/test",
                auth_strength: "ServerAdmin",
                claimed_agent_id: "codex".to_string(),
                claimed_channel_id: "manager-channel".to_string(),
                consumed_agent_id: "resolved-codex".to_string(),
                manager_channel_check_relied_on_claimed_header: true,
            }
        );

        let no_principal_fields = identity_consumption_fields("GET /api/test", None, None, false);
        for (name, value) in no_principal_fields.named_values() {
            let field = format!("{name}={value}");
            assert!(
                !field.to_ascii_lowercase().contains("authorization"),
                "field={field}"
            );
        }
        assert_eq!(
            no_principal_fields,
            IdentityConsumptionFields {
                endpoint: "GET /api/test",
                auth_strength: "None",
                claimed_agent_id: String::new(),
                claimed_channel_id: String::new(),
                consumed_agent_id: String::new(),
                manager_channel_check_relied_on_claimed_header: false,
            }
        );
    }
}
