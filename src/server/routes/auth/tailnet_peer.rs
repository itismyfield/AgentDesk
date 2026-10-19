use axum::extract::{OriginalUri, Request};
use axum::http::Method;
use std::net::{IpAddr, SocketAddr};

// These ranges assume direct tailnet transport; request headers cannot establish membership.
pub(super) fn allows(req: &Request, peer: Option<SocketAddr>) -> bool {
    let Some(peer) = peer else {
        return false;
    };
    if matches!(peer, SocketAddr::V6(address) if address.scope_id() != 0) {
        return false;
    }
    let trusted_ip = match peer.ip().to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            a == 100 && (64..=127).contains(&b)
        }
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    };
    if !trusted_ip {
        return false;
    }
    let Some(original) = req.extensions().get::<OriginalUri>() else {
        return false;
    };
    match (req.uri().path(), original.0.path()) {
        ("/internal/node-probe", "/api/internal/node-probe") => req.method() == Method::GET,
        ("/internal/home-stop/v1", "/api/internal/home-stop/v1") => req.method() == Method::POST,
        _ => false,
    }
}

#[cfg(test)]
#[path = "tailnet_peer_tests.rs"]
mod tests;
