pub mod agents;
mod agents_crud;
mod agents_setup;
pub mod analytics;
pub mod auth;
pub mod auto_queue;
pub mod automation_candidates;
pub mod claude_accounts_api;
pub mod cluster;
pub mod cron_api;
pub mod departments;
pub mod discord;
pub mod dispatched_sessions;
pub mod dispatches;
pub mod dm_reply;
pub mod docs;
mod domains;
pub mod escalation;
pub mod github;
pub mod github_dashboard;
pub mod health_api;
pub mod home_metrics;
pub mod hooks;
pub mod idle_recap;
pub mod kanban;
pub mod kanban_repos;
mod maintenance;
pub mod meetings;
pub mod memory_api;
pub mod messages;
pub mod monitoring;
pub mod offices;
pub mod onboarding;
pub mod pipeline;
pub mod pr_summary;
pub mod prompt_manifest_retention;
pub mod provider_cli_api;
mod queue_api;
pub mod receipt;
pub mod resume;
pub mod review_verdict;
pub mod reviews;
pub mod routines;
pub(crate) mod session_activity;
pub mod settings;
mod skill_usage_analytics;
pub mod skills_api;
#[path = "../state.rs"]
pub mod state;
pub mod stats;
pub mod termination_events;
pub mod v1;
pub mod voice_config;

#[cfg(test)]
#[path = "tests/auto_queue_preflight_harness_tests.rs"]
mod auto_queue_preflight_harness_tests;

use axum::{
    Router,
    http::header::CONTENT_TYPE,
    response::{IntoResponse, Response},
};

use std::sync::Arc;

use crate::engine::PolicyEngine;
use crate::error::{AppError, ErrorCode};
use crate::services::discord::health::HealthRegistry;

/// Shared application state passed to all route handlers.
///
/// Defined in `crate::app_state` (a crate-root module below both `server` and
/// `services`) and re-exported here so existing `crate::server::routes::AppState`
/// call sites resolve unchanged while service-layer handlers reference it without
/// a service→server backflow (#3037).
pub use crate::app_state::AppState;

pub(crate) type ApiRouter = Router<AppState>;

/// Mutation routes that gate themselves with `require_explicit_bearer_token`.
/// Kept in one place so the boot-time audit emits a complete inventory.
/// Order matches code-grep order for stable log output.
/// (#2257 concern 1 — operators need to see at startup which write
/// endpoints are mounted on a fail-open auth config.)
pub const EXPLICIT_AUTH_MUTATION_ROUTES: &[&str] = &[
    "kanban: rereview",
    "kanban: batch rereview",
    "kanban: reopen",
    "kanban: batch-transition",
    "kanban: force-transition",
    "auto-queue: submit_order",
];

/// Mutation routes that fail closed unless an operator auth mechanism is
/// configured. These routes are still useful in the boot audit because they
/// explain why an endpoint may reject all callers on auth-less installs.
pub const FAIL_CLOSED_OPERATOR_MUTATION_ROUTES: &[&str] = &[];

/// Returns true when `host` resolves to a loopback interface (so the bound
/// control-plane is only reachable from the same machine). Accepts:
/// - the literal `localhost`, case-insensitively and tolerating the
///   trailing-dot FQDN form (`localhost.`, `LOCALHOST` — DNS-equivalent);
/// - bracketed/unbracketed IPv4 / IPv6 literals whose `is_loopback()` holds
///   (`127.0.0.0/8`, `::1`);
/// - IPv4-mapped IPv6 loopback (`::ffff:127.0.0.0/104`, i.e. the mapped
///   `127.0.0.0/8`), which `Ipv6Addr::is_loopback()` alone does NOT cover.
///
/// Anything else — `0.0.0.0`, `::` (all-interfaces), a LAN address, or an
/// unparseable string — is treated as non-loopback (fail-safe: unknown hosts
/// are assumed LAN-exposed, so the guard fires). (#3870)
pub fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    // Hostname path: tolerate case and the DNS trailing-dot (`localhost.`).
    let hostname = host.strip_suffix('.').unwrap_or(host);
    if hostname.eq_ignore_ascii_case("localhost") {
        return true;
    }
    // IP-literal path: strip optional brackets around an IPv6 literal.
    let stripped = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    match stripped.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        // `Ipv6Addr::is_loopback()` is only `::1`; also accept an IPv4-mapped
        // loopback (`::ffff:127.0.0.0/8`) by checking the embedded IPv4.
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

/// Startup bind-security decision (#3870). The control-plane auth middleware is
/// fail-open when `server.auth_token` is unset (`auth.rs` — no token means
/// pass-through), so binding a token-less server to a non-loopback interface
/// exposes the entire mutating control-plane (deploy gate, agent CRUD, dispatch
/// create) to the LAN with no auth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindSecurityDecision {
    /// Bind to the requested host unchanged — it is either loopback, a token is
    /// configured, or the operator explicitly opted into insecure LAN exposure.
    BindRequested,
    /// Requested host is non-loopback AND no auth token is configured AND the
    /// operator did not opt in: downgrade the bind to loopback so the
    /// unauthenticated control-plane stays off the LAN. The server still boots
    /// and serves locally — this is graceful degradation, not a hard stop.
    ForcedLoopback { requested_host: String },
}

/// Resolves the host the HTTP control-plane should actually bind to, applying
/// the #3870 fail-closed guard. Returns `(effective_host, decision)`.
///
/// Force-loopback fires only for the precise dangerous combination
/// `non-loopback host + auth_token unset + no opt-in`. Loopback hosts, any
/// configured `auth_token`, or `allow_insecure_nonloopback_bind = true` all
/// bind the requested host unchanged. Pure + side-effect free so the startup
/// path can log/act on the decision and tests can assert it directly.
pub fn resolve_secure_bind_host(config: &crate::config::Config) -> (String, BindSecurityDecision) {
    let token_set = config
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .is_some_and(|token| !token.is_empty());

    if is_loopback_host(&config.server.host)
        || token_set
        || config.server.allow_insecure_nonloopback_bind
    {
        return (
            config.server.host.clone(),
            BindSecurityDecision::BindRequested,
        );
    }

    (
        crate::config::ServerConfig::loopback(),
        BindSecurityDecision::ForcedLoopback {
            requested_host: config.server.host.clone(),
        },
    )
}

/// Emits a structured boot-time audit identifying whether the explicit-auth
/// mutation routes will fail-open with the current configuration. Called
/// once from `server::run` after the listener binds. Does NOT change
/// behavior — operators choose whether to add a token or restrict the
/// host/port; this only guarantees they get a clear signal in the logs.
///
/// Policy decision intentionally deferred: agentdesk control plane today
/// runs on a private loopback in single-operator deployments where neither
/// `server.auth_token` nor `kanban.manager_channel_id` is configured. A
/// hard-require would block those installs. See #2257.
pub fn audit_explicit_auth_routes_on_boot(config: &crate::config::Config) {
    let token_set = config
        .server
        .auth_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    let channel_set = config
        .kanban
        .manager_channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some();
    if token_set || channel_set {
        tracing::info!(
            auth_token_configured = token_set,
            manager_channel_configured = channel_set,
            mutation_routes = EXPLICIT_AUTH_MUTATION_ROUTES.len(),
            fail_closed_operator_routes = ?FAIL_CLOSED_OPERATOR_MUTATION_ROUTES,
            "explicit-auth mutation routes will require Bearer token and/or x-channel-id"
        );
        return;
    }
    tracing::warn!(
        auth_token_configured = false,
        manager_channel_configured = false,
        mutation_routes = ?EXPLICIT_AUTH_MUTATION_ROUTES,
        fail_closed_operator_routes = ?FAIL_CLOSED_OPERATOR_MUTATION_ROUTES,
        host = %config.server.host,
        port = config.server.port,
        "FAIL-OPEN: neither server.auth_token nor kanban.manager_channel_id is configured — \
         the mutation_routes endpoints accept any caller that can reach the bind address; \
         fail_closed_operator_routes reject all callers until auth is configured. \
         Restrict the bind host (e.g. 127.0.0.1) or configure server.auth_token before exposing to untrusted clients. (#2257)"
    );
}

#[cfg(test)]
mod audit_explicit_auth_routes_tests {
    use super::*;

    fn test_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.server.host = "127.0.0.1".to_string();
        config.server.port = 8791;
        config
    }

    #[test]
    fn route_inventory_is_non_empty_and_named_uniquely() {
        assert!(!EXPLICIT_AUTH_MUTATION_ROUTES.is_empty());
        let mut sorted = EXPLICIT_AUTH_MUTATION_ROUTES.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            EXPLICIT_AUTH_MUTATION_ROUTES.len(),
            "duplicate label in EXPLICIT_AUTH_MUTATION_ROUTES — audit log will report misleading counts"
        );
        let mut sorted = FAIL_CLOSED_OPERATOR_MUTATION_ROUTES.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            FAIL_CLOSED_OPERATOR_MUTATION_ROUTES.len(),
            "duplicate label in FAIL_CLOSED_OPERATOR_MUTATION_ROUTES — audit log will report misleading counts"
        );
    }

    #[test]
    fn audit_runs_without_panic_for_all_config_combos() {
        // We can't observe tracing output without a test subscriber, but the
        // function must remain panic-free across every combination so the
        // boot path stays robust. (#2257)
        let mut none_set = test_config();
        none_set.server.auth_token = None;
        none_set.kanban.manager_channel_id = None;
        audit_explicit_auth_routes_on_boot(&none_set);

        let mut token_only = test_config();
        token_only.server.auth_token = Some("secret".to_string());
        token_only.kanban.manager_channel_id = None;
        audit_explicit_auth_routes_on_boot(&token_only);

        let mut channel_only = test_config();
        channel_only.server.auth_token = None;
        channel_only.kanban.manager_channel_id = Some("123".to_string());
        audit_explicit_auth_routes_on_boot(&channel_only);

        let mut both = test_config();
        both.server.auth_token = Some("secret".to_string());
        both.kanban.manager_channel_id = Some("123".to_string());
        audit_explicit_auth_routes_on_boot(&both);
    }

    #[test]
    fn empty_strings_are_treated_as_unset() {
        // #2257: defense against config files that ship empty strings
        // (e.g., `auth_token: ""`) — those must NOT count as "configured".
        let mut config = test_config();
        config.server.auth_token = Some("   ".to_string());
        config.kanban.manager_channel_id = Some("".to_string());
        audit_explicit_auth_routes_on_boot(&config);
    }
}

#[cfg(test)]
mod bind_security_tests {
    //! #3870 — startup must fail closed (force-loopback) for the precise
    //! combination of a non-loopback bind host with no `server.auth_token`,
    //! and must NOT downgrade loopback hosts or hosts that have a token (or an
    //! explicit insecure opt-in). Acceptance criterion: the guard fires for
    //! `(non-loopback host, auth_token = None)` and does NOT fire for loopback
    //! or for `non-loopback + token-set`.
    use super::*;

    fn config_with(host: &str, token: Option<&str>, allow_insecure: bool) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.server.host = host.to_string();
        config.server.port = 8791;
        config.server.auth_token = token.map(str::to_string);
        config.server.allow_insecure_nonloopback_bind = allow_insecure;
        config
    }

    #[test]
    fn loopback_host_classification() {
        for host in [
            "127.0.0.1",
            "127.0.0.53",
            "127.255.255.254", // 127.0.0.0/8 upper edge
            "localhost",
            "LOCALHOST",
            "localhost.",  // trailing-dot FQDN form (DNS-equivalent)
            "LocalHost.",  // case + trailing dot together
            " localhost ", // surrounding whitespace
            "::1",
            "[::1]",
            "::ffff:127.0.0.1", // IPv4-mapped loopback
            "::ffff:127.1.2.3", // IPv4-mapped, still inside 127.0.0.0/8
            "[::ffff:127.0.0.1]",
        ] {
            assert!(is_loopback_host(host), "{host} should be loopback");
        }
        for host in [
            "0.0.0.0",
            "::",                 // all-interfaces IPv6 — must stay non-loopback
            "::ffff:0.0.0.0",     // IPv4-mapped all-interfaces
            "::ffff:192.168.1.1", // IPv4-mapped LAN address
            "192.168.1.10",
            "10.0.0.5",
            "example.com",
            "localhostx", // not localhost
            "",
            "  ",
        ] {
            assert!(!is_loopback_host(host), "{host} should be non-loopback");
        }
    }

    #[test]
    fn nonloopback_without_token_is_forced_to_loopback() {
        // The dangerous combination — the guard MUST fire.
        for host in ["0.0.0.0", "::", "192.168.1.10"] {
            let config = config_with(host, None, false);
            let (effective, decision) = resolve_secure_bind_host(&config);
            assert_eq!(
                decision,
                BindSecurityDecision::ForcedLoopback {
                    requested_host: host.to_string(),
                },
                "{host} + no token must force loopback"
            );
            assert!(
                is_loopback_host(&effective),
                "{host} downgrade must bind a loopback address, got {effective}"
            );
        }
    }

    #[test]
    fn empty_token_string_counts_as_unset_and_forces_loopback() {
        // `auth_token: ""`/whitespace must NOT count as configured (mirrors the
        // audit's empty-string handling) — still the dangerous combination.
        let config = config_with("0.0.0.0", Some("   "), false);
        let (effective, decision) = resolve_secure_bind_host(&config);
        assert_eq!(
            decision,
            BindSecurityDecision::ForcedLoopback {
                requested_host: "0.0.0.0".to_string(),
            }
        );
        assert!(is_loopback_host(&effective));
    }

    #[test]
    fn loopback_host_is_never_downgraded() {
        // No token, but already loopback — guard must NOT fire (no false alarm).
        for host in ["127.0.0.1", "localhost", "::1"] {
            let config = config_with(host, None, false);
            let (effective, decision) = resolve_secure_bind_host(&config);
            assert_eq!(
                decision,
                BindSecurityDecision::BindRequested,
                "{host} is loopback — guard must not fire"
            );
            assert_eq!(effective, host, "loopback host must bind unchanged");
        }
    }

    #[test]
    fn loopback_notation_edges_are_not_force_downgraded() {
        // Codex review (#3870): non-`is_loopback()` loopback notations must not
        // over-fire the guard onto an already-loopback config (no token set).
        // `localhost.` (trailing-dot FQDN) and IPv4-mapped IPv6 loopback both
        // resolve to loopback and must bind unchanged.
        for host in [
            "localhost.",
            "LocalHost.",
            "::ffff:127.0.0.1",
            "[::ffff:127.0.0.1]",
        ] {
            let config = config_with(host, None, false);
            let (effective, decision) = resolve_secure_bind_host(&config);
            assert_eq!(
                decision,
                BindSecurityDecision::BindRequested,
                "{host} is a loopback notation — guard must NOT fire"
            );
            assert_eq!(effective, host, "{host} must bind unchanged");
        }
    }

    #[test]
    fn all_interfaces_hosts_still_force_loopback() {
        // Regression guard for the edge fix above: the IPv4/IPv6 all-interfaces
        // wildcards and IPv4-mapped non-loopback must STILL force loopback when
        // no token is set — the security behavior is unchanged.
        for host in ["0.0.0.0", "::", "::ffff:192.168.1.1"] {
            let config = config_with(host, None, false);
            let (effective, decision) = resolve_secure_bind_host(&config);
            assert_eq!(
                decision,
                BindSecurityDecision::ForcedLoopback {
                    requested_host: host.to_string(),
                },
                "{host} is non-loopback — guard must still fire"
            );
            assert!(is_loopback_host(&effective));
        }
    }

    #[test]
    fn nonloopback_with_token_binds_requested_host() {
        // A configured token closes the fail-open, so LAN exposure is allowed.
        let config = config_with("0.0.0.0", Some("s3cret"), false);
        let (effective, decision) = resolve_secure_bind_host(&config);
        assert_eq!(decision, BindSecurityDecision::BindRequested);
        assert_eq!(
            effective, "0.0.0.0",
            "token-set non-loopback must bind as requested"
        );
    }

    #[test]
    fn explicit_insecure_optin_binds_requested_host() {
        // Escape hatch: operator knowingly exposes a token-less control-plane.
        let config = config_with("0.0.0.0", None, true);
        let (effective, decision) = resolve_secure_bind_host(&config);
        assert_eq!(decision, BindSecurityDecision::BindRequested);
        assert_eq!(effective, "0.0.0.0");
    }
}

// reason: PG-pool router constructor used only by the `#[cfg(test)]` router
// builders in health_api/route tests; the lib build sees no caller. See #3034.
#[allow(dead_code)]
pub fn api_router_with_pg(
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<sqlx::PgPool>,
) -> Router {
    api_router_with_pg_and_cluster(
        engine,
        config,
        broadcast_tx,
        batch_buffer,
        health_registry,
        pg_pool,
        None,
    )
}

pub fn api_router_with_pg_and_cluster(
    engine: PolicyEngine,
    config: crate::config::Config,
    broadcast_tx: crate::server::ws::BroadcastTx,
    batch_buffer: crate::server::ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<sqlx::PgPool>,
    cluster_instance_id: Option<String>,
) -> Router {
    let state = AppState {
        pg_pool,
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry,
        cluster_instance_id,
    };

    crate::services::discord::monitoring_status::spawn_expiry_sweeper(
        state::global_monitoring_store(),
        state.health_registry.clone(),
    );

    compose_api_router(state.clone()).with_state(state)
}

fn compose_api_router(state: AppState) -> ApiRouter {
    Router::new()
        .merge(domains::access::router())
        .merge(domains::onboarding::router(state.clone()))
        .merge(domains::agents::router(state.clone()))
        .merge(domains::kanban::router(state.clone()))
        .merge(domains::reviews::router(state.clone()))
        .merge(domains::ops::router(state.clone()))
        .merge(domains::integrations::router(state.clone()))
        .merge(v1::router(state.clone()))
        .merge(domains::admin::router(state))
}

pub(super) fn public_api_domain(router: ApiRouter) -> ApiRouter {
    router.layer(axum::middleware::map_response(error_envelope_middleware))
}

pub(super) fn protected_api_domain(router: ApiRouter, state: AppState) -> ApiRouter {
    router
        .layer(axum::middleware::from_fn_with_state(
            state,
            auth::auth_middleware,
        ))
        .layer(axum::middleware::map_response(error_envelope_middleware))
}

async fn error_envelope_middleware(response: Response) -> Response {
    if response.status().is_server_error() && !response_is_json(&response) {
        let message = response
            .status()
            .canonical_reason()
            .unwrap_or("internal server error")
            .to_ascii_lowercase();
        return AppError::new(response.status(), ErrorCode::Internal, message).into_response();
    }

    response
}

fn response_is_json(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("application/json"))
        .unwrap_or(false)
}
