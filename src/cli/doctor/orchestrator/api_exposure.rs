use super::{Check, CheckGroup, CheckStatus, SecurityExposure, Severity};
use crate::config;

pub(super) fn check_server_api_exposure(cfg: &config::Config) -> Check {
    let token_configured = cfg
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|token| !token.is_empty());
    let non_loopback = !crate::server::routes::is_loopback_host(&cfg.server.host);

    let (detail, next_steps) = if !non_loopback || token_configured {
        (
            "The configured server bind is loopback or has a server.auth_token.".to_string(),
            Vec::new(),
        )
    } else if cfg.server.allow_insecure_nonloopback_bind {
        (
            format!(
                "INFO: server.host={} is intentionally exposed without server.auth_token by server.allow_insecure_nonloopback_bind=true.",
                cfg.server.host
            ),
            vec![
                "See docs/operational-api-trust-boundary.md for the accepted trust boundary."
                    .into(),
            ],
        )
    } else {
        (
            format!(
                "INFO: server.host={} has no server.auth_token; the startup guard will bind to loopback.",
                cfg.server.host
            ),
            vec!["See docs/operational-api-trust-boundary.md for the bind policy.".into()],
        )
    };

    Check::ok(
        "server_api_exposure",
        CheckGroup::Core,
        "API bind exposure",
        detail,
    )
    .with_path("docs/operational-api-trust-boundary.md")
    .with_next_steps(next_steps)
    .with_security_exposure(SecurityExposure::PublicSurface)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenless_non_loopback_config(opted_in: bool) -> config::Config {
        let mut cfg = config::Config::default();
        cfg.server.host = "0.0.0.0".into();
        cfg.server.auth_token = None;
        cfg.server.allow_insecure_nonloopback_bind = opted_in;
        cfg
    }

    #[test]
    fn intentional_tokenless_non_loopback_bind_is_informational() {
        let check = check_server_api_exposure(&tokenless_non_loopback_config(true));
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.severity, Severity::Info);
        assert_eq!(
            check.path.as_deref(),
            Some("docs/operational-api-trust-boundary.md")
        );
        assert_eq!(check.next_steps.len(), 1);
        assert!(check.detail.contains("intentionally exposed"));
    }

    #[test]
    fn rejected_tokenless_non_loopback_bind_is_informational_too() {
        let check = check_server_api_exposure(&tokenless_non_loopback_config(false));
        assert_eq!(check.status, CheckStatus::Pass);
        assert_eq!(check.severity, Severity::Info);
        assert!(check.detail.contains("will bind to loopback"));
    }
}
