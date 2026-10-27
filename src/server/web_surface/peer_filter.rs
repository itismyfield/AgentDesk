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
    flagged_total: Arc<AtomicU64>,
    last_warned: Arc<Mutex<HashMap<IpAddr, Instant>>>,
}

impl PeerFilter {
    pub(super) fn new(mode: PeerFilterMode) -> Self {
        Self {
            mode,
            flagged_total: Arc::default(),
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

/// Where a socket peer sits relative to the trust boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerClass {
    Loopback,
    /// 100.64.0.0/10 or fd7a:115c:a1e0::/48.
    Tailscale,
    /// RFC1918, 169.254/16, and IPv6 unique-local (outside Tailscale's /48) or link-local.
    PrivateLan,
    Outside,
}

impl PeerClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Loopback => "loopback",
            Self::Tailscale => "tailscale",
            Self::PrivateLan => "private_lan",
            Self::Outside => "outside",
        }
    }
}

/// IPv4-mapped IPv6 is judged as IPv4.
fn classify_peer(ip: IpAddr) -> PeerClass {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            if v4.is_loopback() {
                PeerClass::Loopback
            } else if a == 100 && (b & 0xc0) == 64 {
                PeerClass::Tailscale
            } else if v4.is_private() || v4.is_link_local() {
                PeerClass::PrivateLan
            } else {
                PeerClass::Outside
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                PeerClass::Loopback
            } else if v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0] {
                PeerClass::Tailscale
            } else if v6.is_unique_local() || v6.is_unicast_link_local() {
                PeerClass::PrivateLan
            } else {
                PeerClass::Outside
            }
        }
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
    let class = classify_peer(peer);
    if matches!(class, PeerClass::Loopback | PeerClass::Tailscale) {
        return next.run(req).await;
    }
    // Private LAN peers are still served; they are logged so callers that would lose access
    // under a Tailscale-only boundary show up before that boundary is enforced.
    let total = filter.flagged_total.fetch_add(1, Ordering::Relaxed) + 1;
    if filter.should_warn(peer, Instant::now()) {
        tracing::warn!(
            peer_ip = %peer,
            peer_class = %class.as_str(),
            method = %req.method(),
            path = req.uri().path(),
            mode = filter.mode.as_str(),
            flagged_total = total,
            "peer filter: request from outside loopback/Tailscale"
        );
    }
    if class == PeerClass::Outside && filter.mode == PeerFilterMode::Enforce {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "forbidden", "code": "peer_not_allowed"})),
        )
            .into_response();
    }
    next.run(req).await
}
