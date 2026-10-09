//! Source filter on the socket peer address for the whole HTTP surface.
//! Forwarded headers are never consulted; a Tailscale serve proxy arrives as loopback.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::config::PeerFilterMode;

const WARN_INTERVAL: Duration = Duration::from_secs(60);
// Bounds the per-peer warn throttle; overflow only means extra warn lines.
const MAX_WARNED_PEERS: usize = 1024;

#[derive(Clone)]
pub(super) struct PeerFilter {
    mode: PeerFilterMode,
    disallowed_total: Arc<AtomicU64>,
    last_warned: Arc<Mutex<HashMap<IpAddr, Instant>>>,
}

impl PeerFilter {
    pub(super) fn new(mode: PeerFilterMode) -> Self {
        Self {
            mode,
            disallowed_total: Arc::default(),
            last_warned: Arc::default(),
        }
    }

    pub(super) fn wrap(self, router: Router) -> Router {
        if self.mode == PeerFilterMode::Off {
            return router;
        }
        router.layer(axum::middleware::from_fn_with_state(self, filter))
    }

    fn should_warn(&self, peer: IpAddr, now: Instant) -> bool {
        let Ok(mut last_warned) = self.last_warned.lock() else {
            return true;
        };
        if last_warned
            .get(&peer)
            .is_some_and(|at| now.duration_since(*at) < WARN_INTERVAL)
        {
            return false;
        }
        if last_warned.len() >= MAX_WARNED_PEERS {
            last_warned.clear();
        }
        last_warned.insert(peer, now);
        true
    }
}

/// Loopback, Tailscale (100.64.0.0/10; its IPv6 range sits inside fc00::/7),
/// RFC1918, IPv6 unique-local and link-local peers. IPv4-mapped IPv6 is judged as IPv4.
pub(super) fn is_allowed_peer(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && (b & 0xc0) == 64)
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

async fn filter(State(filter): State<PeerFilter>, req: Request, next: Next) -> Response {
    // In-process requests have no socket; the served listener always supplies one.
    let Some(peer) = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip())
    else {
        return next.run(req).await;
    };
    if is_allowed_peer(peer) {
        return next.run(req).await;
    }
    let total = filter.disallowed_total.fetch_add(1, Ordering::Relaxed) + 1;
    if filter.should_warn(peer, Instant::now()) {
        tracing::warn!(
            peer_ip = %peer,
            method = %req.method(),
            path = req.uri().path(),
            mode = filter.mode.as_str(),
            disallowed_total = total,
            "peer filter: request from outside loopback/Tailscale/private LAN"
        );
    }
    if filter.mode == PeerFilterMode::Enforce {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "forbidden", "code": "peer_not_allowed"})),
        )
            .into_response();
    }
    next.run(req).await
}
