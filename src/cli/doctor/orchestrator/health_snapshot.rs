//! Resolve startup diagnostics against the local process listener.
use super::*;

pub(super) fn fetch_health_snapshot(options: &DoctorOptions) -> HealthSnapshot {
    let cfg = config::load_graceful();
    // Startup diagnostics describe this process, even when the user's CLI
    // target is a remote leader. Manual doctor keeps its remote opt-in gate.
    let base = if options.run_context == RunContext::StartupOnce {
        cfg.server.local_base_url()
    } else {
        crate::cli::client::api_base()
    };
    if cfg
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|token| !token.is_empty())
        && !options.allow_remote
        && !health::is_loopback_base_url(&base)
    {
        return HealthSnapshot {
            base,
            body: None,
            error: Some(
                "non-loopback AGENTDESK_API_URL with configured auth token requires --allow-remote"
                    .to_string(),
            ),
        };
    }

    match crate::cli::client::get_json_at(&base, "/api/health/detail").or_else(|detail_error| {
        if detail_error.contains("(404)") {
            crate::cli::client::get_json_at(&base, "/api/health")
        } else {
            Err(detail_error)
        }
    }) {
        Ok(body) => HealthSnapshot {
            base,
            body: Some(body),
            error: None,
        },
        Err(e) => HealthSnapshot {
            base,
            body: None,
            error: Some(e.to_string()),
        },
    }
}
