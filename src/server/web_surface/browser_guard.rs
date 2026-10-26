//! Browser-borne request guard for the whole HTTP surface: a Host allowlist
//! against DNS rebinding, and a cross-site rule for state-changing methods.
//! CLI, MCP and node-to-node callers send no Origin or Sec-Fetch-Site, so only
//! the Host rule applies to them.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::HttpBody,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::host_allowlist::HostAllowlist;
use crate::config::{BrowserGuardMode, Config};

const WARN_INTERVAL: Duration = Duration::from_secs(60);
// Bounds the warn throttle; overflow only means extra warn lines.
const MAX_WARN_KEYS: usize = 1024;

/// Handlers that take the body as raw `Bytes` and parse JSON themselves, so no
/// `Json` extractor checks the media type. `{}` matches one path segment.
pub(crate) const RAW_JSON_ROUTES: &[&str] = &[
    "/api/agents/{}/turn/deliver",
    "/api/inflight/rebind",
    "/api/doctor/stale-mailbox/repair",
    "/api/channels/{}/relay-recovery",
    "/api/turns/{}/cancel",
    "/api/discord/send",
    "/api/discord/send-to-agent",
    "/api/discord/send-dm",
    "/api/queue/runs/{}/phase-gates/repair",
    "/api/queue/reset",
    "/api/queue/reset-global",
    "/api/queue/pause",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Violation {
    HostNotAllowed,
    CrossSite,
    OriginNotAllowed,
    JsonBodyRequired,
}

impl Violation {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::HostNotAllowed => "host_not_allowed",
            Self::CrossSite => "cross_site_request",
            Self::OriginNotAllowed => "origin_not_allowed",
            Self::JsonBodyRequired => "json_content_type_required",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::JsonBodyRequired => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            _ => StatusCode::FORBIDDEN,
        }
    }
}

#[derive(Clone)]
pub(crate) struct BrowserGuard {
    mode: BrowserGuardMode,
    hosts: Arc<HostAllowlist>,
    flagged_total: Arc<AtomicU64>,
    last_warned: Arc<Mutex<HashMap<String, Instant>>>,
}

impl BrowserGuard {
    pub(crate) fn new(config: &Config) -> Self {
        Self::with_hosts(
            config.server.browser_guard,
            HostAllowlist::from_config(config),
        )
    }

    pub(super) fn with_hosts(mode: BrowserGuardMode, hosts: Arc<HostAllowlist>) -> Self {
        Self {
            mode,
            hosts,
            flagged_total: Arc::default(),
            last_warned: Arc::default(),
        }
    }

    pub(super) fn wrap(self, router: Router) -> Router {
        router.layer(axum::middleware::from_fn_with_state(self, guard))
    }

    /// For the WebSocket admission check, which already requires Origin == Host.
    pub(crate) fn rejects_host(&self, headers: &HeaderMap) -> bool {
        self.mode == BrowserGuardMode::Enforce
            && !request_host(headers, None).is_some_and(|host| self.hosts.host_allowed(host))
    }

    pub(crate) fn check(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        uri_authority: Option<&str>,
        may_have_body: bool,
    ) -> Option<Violation> {
        let host = request_host(headers, uri_authority);
        if !host.is_some_and(|host| self.hosts.host_allowed(host)) {
            return Some(Violation::HostNotAllowed);
        }
        if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
            return None;
        }
        if header_str(headers, "sec-fetch-site")
            .is_some_and(|site| site.eq_ignore_ascii_case("cross-site"))
        {
            return Some(Violation::CrossSite);
        }
        if let Some(origin) = header_str(headers, header::ORIGIN.as_str())
            && !self.hosts.origin_allowed(origin, host)
        {
            return Some(Violation::OriginNotAllowed);
        }
        // An empty body carries nothing to parse; CLI cancels send none.
        if may_have_body && is_raw_json_route(path) && !is_json(headers) {
            return Some(Violation::JsonBodyRequired);
        }
        None
    }

    fn should_warn(&self, key: String, now: Instant) -> bool {
        let Ok(mut last_warned) = self.last_warned.lock() else {
            return true;
        };
        if last_warned
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < WARN_INTERVAL)
        {
            return false;
        }
        if last_warned.len() >= MAX_WARN_KEYS {
            last_warned.clear();
        }
        last_warned.insert(key, now);
        true
    }

    fn report(&self, violation: Violation, req: &Request, peer: IpAddr) {
        let total = self.flagged_total.fetch_add(1, Ordering::Relaxed) + 1;
        let headers = req.headers();
        let host = request_host(headers, req.uri().authority().map(|a| a.as_str())).unwrap_or("");
        let origin = header_str(headers, header::ORIGIN.as_str()).unwrap_or("");
        let site = header_str(headers, "sec-fetch-site").unwrap_or("");
        let key = format!(
            "{}|{peer}|{host}|{origin}|{site}|{}|{}",
            violation.code(),
            req.method(),
            req.uri().path()
        );
        if !self.should_warn(key, Instant::now()) {
            return;
        }
        tracing::warn!(
            reason = violation.code(),
            host,
            origin,
            sec_fetch_site = site,
            method = %req.method(),
            path = req.uri().path(),
            peer_ip = %peer,
            mode = self.mode.as_str(),
            flagged_total = total,
            "browser guard: request fails the host/origin rules"
        );
    }
}

async fn guard(State(guard): State<BrowserGuard>, req: Request, next: Next) -> Response {
    // In-process requests have no socket; the served listener always supplies one.
    let Some(peer) = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
    else {
        return next.run(req).await;
    };
    let may_have_body = req.body().size_hint().upper() != Some(0);
    let Some(violation) = guard.check(
        req.method(),
        req.uri().path(),
        req.headers(),
        req.uri().authority().map(|a| a.as_str()),
        may_have_body,
    ) else {
        return next.run(req).await;
    };
    guard.report(violation, &req, peer);
    if guard.mode == BrowserGuardMode::Enforce {
        return (
            violation.status(),
            Json(serde_json::json!({"error": "forbidden", "code": violation.code()})),
        )
            .into_response();
    }
    next.run(req).await
}

// Forwarded headers are never consulted; a proxy must preserve the public Host.
fn request_host<'a>(headers: &'a HeaderMap, uri_authority: Option<&'a str>) -> Option<&'a str> {
    header_str(headers, header::HOST.as_str()).or(uri_authority)
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn is_json(headers: &HeaderMap) -> bool {
    header_str(headers, header::CONTENT_TYPE.as_str())
        .and_then(|value| value.split(';').next())
        .map(|media| media.trim().to_ascii_lowercase())
        .is_some_and(|media| media == "application/json" || media.ends_with("+json"))
}

pub(crate) fn is_raw_json_route(path: &str) -> bool {
    RAW_JSON_ROUTES.iter().any(|template| {
        let mut want = template.split('/');
        let mut got = path.split('/');
        loop {
            match (want.next(), got.next()) {
                (None, None) => return true,
                (Some("{}"), Some(segment)) if !segment.is_empty() => {}
                (Some(a), Some(b)) if a == b => {}
                _ => return false,
            }
        }
    })
}
