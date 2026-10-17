use super::routes::{self, BindSecurityDecision};

pub(super) fn log_decision(
    config: &crate::config::Config,
    bind_host: &str,
    decision: &BindSecurityDecision,
) {
    if let BindSecurityDecision::ForcedLoopback { requested_host } = decision {
        tracing::error!(
            requested_host = %requested_host,
            forced_host = %bind_host,
            port = config.server.port,
            "SECURITY (#3870): server.host={requested_host} is non-loopback and \
             server.auth_token is unset — the control-plane auth middleware is fail-open, so \
             this would expose deploy-gate / agent-CRUD / dispatch endpoints to the LAN with no \
             auth. Force-binding to loopback ({bind_host}) instead. The server still serves \
             locally. To expose on the LAN intentionally, set server.auth_token (recommended) \
             or server.allow_insecure_nonloopback_bind=true. See \
             docs/operational-api-trust-boundary.md."
        );
    } else if !routes::is_loopback_host(&config.server.host)
        && config
            .server
            .auth_token
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
        && config.server.allow_insecure_nonloopback_bind
    {
        tracing::info!(
            requested_host = %config.server.host,
            port = config.server.port,
            "SECURITY (#5721): explicitly binding the unauthenticated control plane to a \
             non-loopback interface because server.allow_insecure_nonloopback_bind=true. \
             This is an intentional trust-boundary decision; see \
             docs/operational-api-trust-boundary.md."
        );
    }
}
