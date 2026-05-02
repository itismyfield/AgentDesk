use std::net::{Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

pub fn is_loopback_url(raw: &str, required_port: Option<u16>) -> bool {
    let Ok(parsed) = Url::parse(raw.trim()) else {
        return false;
    };

    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }

    let is_loopback_host = match parsed.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(addr)) => addr == Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(addr)) => addr == Ipv6Addr::LOCALHOST,
        None => false,
    };
    if !is_loopback_host {
        return false;
    }

    required_port
        .map(|port| parsed.port_or_known_default() == Some(port))
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::is_loopback_url;

    #[test]
    fn accepts_http_and_https_loopback_hosts() {
        assert!(is_loopback_url("http://127.0.0.1:8791", None));
        assert!(is_loopback_url("http://localhost:8791", None));
        assert!(is_loopback_url("https://localhost:8791", None));
        assert!(is_loopback_url("http://[::1]:8791", None));
        assert!(is_loopback_url(" https://LOCALHOST/api/health ", None));
    }

    #[test]
    fn rejects_prefix_spoofed_remote_and_unsupported_urls() {
        assert!(!is_loopback_url("http://localhost.evil.example:8791", None));
        assert!(!is_loopback_url("http://127.0.0.1.evil.example:8791", None));
        assert!(!is_loopback_url("http://[::2]:8791", None));
        assert!(!is_loopback_url("ftp://localhost:8791", None));
        assert!(!is_loopback_url("not a url", None));
    }

    #[test]
    fn required_port_must_match_explicit_or_known_default_port() {
        assert!(is_loopback_url("http://localhost:8791", Some(8791)));
        assert!(is_loopback_url("https://localhost", Some(443)));
        assert!(!is_loopback_url("http://localhost:8792", Some(8791)));
        assert!(!is_loopback_url("http://localhost", Some(8791)));
    }
}
