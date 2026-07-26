use std::collections::BTreeSet;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Method, RequestBuilder};
use url::Url;

use crate::config::{ClusterConfig, ClusterNodeConfig};

const FORWARD_TIMEOUT_SECS: u64 = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TrustedTargetError {
    MissingConfig,
    InvalidConfiguredOrigin,
    InvalidAdvertisedOrigin,
    OriginMismatch,
    MissingCapability,
    DnsResolutionFailed,
    UnsafeAddress,
    EmptyDnsAnswer,
    InsecureTransport,
    ClientBuildFailed,
    InvalidEndpoint,
}

impl TrustedTargetError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::MissingConfig => "trusted_forward_origin_missing",
            Self::InvalidConfiguredOrigin => "trusted_forward_origin_invalid",
            Self::InvalidAdvertisedOrigin => "worker_api_base_url_invalid",
            Self::OriginMismatch => "trusted_forward_origin_mismatch",
            Self::MissingCapability => "session_forwarding_capability_missing",
            Self::DnsResolutionFailed => "trusted_forward_dns_failed",
            Self::UnsafeAddress => "trusted_forward_address_unsafe",
            Self::EmptyDnsAnswer => "trusted_forward_dns_empty",
            Self::InsecureTransport => "trusted_forward_insecure_transport",
            Self::ClientBuildFailed => "trusted_forward_client_failed",
            Self::InvalidEndpoint => "trusted_forward_endpoint_invalid",
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::MissingConfig => "trusted forwarding origin is not configured for session owner",
            Self::InvalidConfiguredOrigin => "trusted forwarding origin configuration is invalid",
            Self::InvalidAdvertisedOrigin => "session owner API origin advertisement is invalid",
            Self::OriginMismatch => "session owner API origin does not match trusted configuration",
            Self::MissingCapability => {
                "session owner does not advertise required forwarding support"
            }
            Self::DnsResolutionFailed => "trusted forwarding origin DNS resolution failed",
            Self::UnsafeAddress => "trusted forwarding origin resolved to a prohibited address",
            Self::EmptyDnsAnswer => {
                "trusted forwarding origin DNS resolution returned no addresses"
            }
            Self::InsecureTransport => {
                "cleartext forwarding requires explicit private-address transport consent"
            }
            Self::ClientBuildFailed => "trusted forwarding HTTP client construction failed",
            Self::InvalidEndpoint => "trusted forwarding endpoint is invalid",
        }
    }
}

#[derive(Clone)]
pub(crate) struct TrustedForwardTarget {
    owner_instance_id: String,
    origin: Url,
    client: reqwest::Client,
}

impl fmt::Debug for TrustedForwardTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustedForwardTarget")
            .field("owner_instance_id", &self.owner_instance_id)
            .field("scheme", &self.origin.scheme())
            .field("host_kind", &host_kind(&self.origin))
            .field("effective_port", &self.origin.port_or_known_default())
            .finish_non_exhaustive()
    }
}

impl PartialEq for TrustedForwardTarget {
    fn eq(&self, other: &Self) -> bool {
        self.owner_instance_id == other.owner_instance_id && self.origin == other.origin
    }
}

impl Eq for TrustedForwardTarget {}

impl TrustedForwardTarget {
    pub(crate) fn owner_instance_id(&self) -> &str {
        &self.owner_instance_id
    }

    pub(crate) fn request(
        &self,
        method: Method,
        relative_endpoint: &str,
    ) -> Result<RequestBuilder, TrustedTargetError> {
        let endpoint = self.endpoint(relative_endpoint)?;
        Ok(self.client.request(method, endpoint))
    }

    fn endpoint(&self, relative_endpoint: &str) -> Result<Url, TrustedTargetError> {
        if !relative_endpoint.starts_with('/') || relative_endpoint.starts_with("//") {
            return Err(TrustedTargetError::InvalidEndpoint);
        }
        let endpoint = self
            .origin
            .join(relative_endpoint)
            .map_err(|_| TrustedTargetError::InvalidEndpoint)?;
        if canonical_origin(&endpoint) != canonical_origin(&self.origin) {
            return Err(TrustedTargetError::InvalidEndpoint);
        }
        Ok(endpoint)
    }

    #[cfg(test)]
    pub(super) fn endpoint_for_test(
        &self,
        relative_endpoint: &str,
    ) -> Result<Url, TrustedTargetError> {
        self.endpoint(relative_endpoint)
    }

    #[cfg(test)]
    pub(super) fn for_test(
        owner_instance_id: &str,
        origin: &str,
    ) -> Result<Self, TrustedTargetError> {
        let origin = parse_origin(origin, TrustedTargetError::InvalidConfiguredOrigin)?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(FORWARD_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|_| TrustedTargetError::ClientBuildFailed)?;
        Ok(Self {
            owner_instance_id: owner_instance_id.to_string(),
            origin,
            client,
        })
    }
}

#[async_trait]
pub(crate) trait ForwardDnsResolver: Send + Sync {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, ()>;
}

pub(crate) struct SystemForwardDnsResolver;

#[async_trait]
impl ForwardDnsResolver for SystemForwardDnsResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, ()> {
        tokio::net::lookup_host((host, port))
            .await
            .map(|addresses| addresses.collect())
            .map_err(|_| ())
    }
}

pub(crate) async fn build_trusted_target(
    cluster: &ClusterConfig,
    owner_instance_id: &str,
    advertised_origin: &str,
    required_capability: &str,
    capabilities: &serde_json::Value,
) -> Result<TrustedForwardTarget, TrustedTargetError> {
    build_trusted_target_with_resolver(
        cluster,
        owner_instance_id,
        advertised_origin,
        required_capability,
        capabilities,
        &SystemForwardDnsResolver,
    )
    .await
}

async fn build_trusted_target_with_resolver(
    cluster: &ClusterConfig,
    owner_instance_id: &str,
    advertised_origin: &str,
    required_capability: &str,
    capabilities: &serde_json::Value,
    resolver: &dyn ForwardDnsResolver,
) -> Result<TrustedForwardTarget, TrustedTargetError> {
    if capabilities
        .get("agentdesk_api")
        .and_then(|api| api.get(required_capability))
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err(TrustedTargetError::MissingCapability);
    }
    let node_config = cluster
        .nodes
        .get(owner_instance_id)
        .ok_or(TrustedTargetError::MissingConfig)?;
    let configured = node_config
        .trusted_forward_origin
        .as_deref()
        .ok_or(TrustedTargetError::MissingConfig)
        .and_then(|raw| parse_origin(raw, TrustedTargetError::InvalidConfiguredOrigin))?;
    let advertised = parse_origin(
        advertised_origin,
        TrustedTargetError::InvalidAdvertisedOrigin,
    )?;
    if canonical_origin(&configured) != canonical_origin(&advertised) {
        return Err(TrustedTargetError::OriginMismatch);
    }

    let port = configured
        .port_or_known_default()
        .ok_or(TrustedTargetError::InvalidConfiguredOrigin)?;
    let host = configured
        .host_str()
        .ok_or(TrustedTargetError::InvalidConfiguredOrigin)?;
    let resolved = match configured.host() {
        Some(url::Host::Ipv4(address)) => vec![SocketAddr::new(IpAddr::V4(address), port)],
        Some(url::Host::Ipv6(address)) => vec![SocketAddr::new(IpAddr::V6(address), port)],
        Some(url::Host::Domain(_)) => resolver
            .resolve(host, port)
            .await
            .map_err(|_| TrustedTargetError::DnsResolutionFailed)?,
        None => return Err(TrustedTargetError::InvalidConfiguredOrigin),
    };
    let pinned = validate_addresses(resolved, node_config.allow_private_forwarding)?;
    validate_transport(&configured, node_config, &pinned)?;
    let client = build_pinned_client(host, &pinned)?;

    Ok(TrustedForwardTarget {
        owner_instance_id: owner_instance_id.to_string(),
        origin: configured,
        client,
    })
}

fn validate_transport(
    origin: &Url,
    node_config: &ClusterNodeConfig,
    pinned: &[SocketAddr],
) -> Result<(), TrustedTargetError> {
    if origin.scheme() == "https" {
        return Ok(());
    }
    if !node_config.allow_private_forwarding
        || !node_config.allow_insecure_http_forwarding
        || !pinned
            .iter()
            .all(|address| address_is_configured_private(address.ip()))
    {
        return Err(TrustedTargetError::InsecureTransport);
    }
    Ok(())
}

fn build_pinned_client(
    host: &str,
    pinned: &[SocketAddr],
) -> Result<reqwest::Client, TrustedTargetError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(FORWARD_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(host, pinned)
        .build()
        .map_err(|_| TrustedTargetError::ClientBuildFailed)
}

fn parse_origin(raw: &str, error: TrustedTargetError) -> Result<Url, TrustedTargetError> {
    let url = Url::parse(raw.trim()).map_err(|_| error.clone())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host().is_none()
        || url.port_or_known_default().is_none()
    {
        return Err(error);
    }
    Ok(url)
}

fn canonical_origin(url: &Url) -> Option<(String, String, u16)> {
    Some((
        url.scheme().to_ascii_lowercase(),
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?,
    ))
}

fn host_kind(url: &Url) -> &'static str {
    match url.host() {
        Some(url::Host::Domain(_)) => "dns",
        Some(url::Host::Ipv4(_)) => "ipv4",
        Some(url::Host::Ipv6(_)) => "ipv6",
        None => "missing",
    }
}

fn validate_addresses(
    addresses: Vec<SocketAddr>,
    allow_private: bool,
) -> Result<Vec<SocketAddr>, TrustedTargetError> {
    if addresses.is_empty() {
        return Err(TrustedTargetError::EmptyDnsAnswer);
    }
    let mut unique = BTreeSet::new();
    for address in addresses {
        if !address_is_allowed(address.ip(), allow_private) {
            return Err(TrustedTargetError::UnsafeAddress);
        }
        unique.insert(address);
    }
    Ok(unique.into_iter().collect())
}

fn address_is_allowed(address: IpAddr, allow_private: bool) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_is_allowed(address, allow_private),
        IpAddr::V6(address) => {
            if ipv6_uses_local_nat64_prefix(address) {
                return false;
            }
            ipv4_embedded_ipv6(address).map_or_else(
                || ipv6_is_allowed(address, allow_private),
                |address| ipv4_is_allowed(address, allow_private),
            )
        }
    }
}

fn address_is_configured_private(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => ipv4_is_configured_private(address),
        IpAddr::V6(address) => {
            if ipv6_uses_local_nat64_prefix(address) || ipv6_is_deprecated_site_local(address) {
                return false;
            }
            ipv4_embedded_ipv6(address).map_or_else(
                || (address.segments()[0] & 0xfe00) == 0xfc00,
                ipv4_is_configured_private,
            )
        }
    }
}

fn ipv6_uses_local_nat64_prefix(address: Ipv6Addr) -> bool {
    address.octets()[..6] == [0x00, 0x64, 0xff, 0x9b, 0, 1]
}

fn ipv6_is_deprecated_site_local(address: Ipv6Addr) -> bool {
    (address.segments()[0] & 0xffc0) == 0xfec0
}

fn ipv4_embedded_ipv6(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let octets = address.octets();
    if address.to_ipv4_mapped().is_some()
        || octets[..12] == [0; 12] && !address.is_unspecified() && !address.is_loopback()
        || octets[..12] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0, 0, 0, 0, 0]
    {
        return Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    None
}

fn ipv4_is_configured_private(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private() || octets[0] == 100 && (64..=127).contains(&octets[1])
}

fn ipv4_is_allowed(address: Ipv4Addr, allow_private: bool) -> bool {
    let octets = address.octets();
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_multicast()
        || address == Ipv4Addr::BROADCAST
        || octets == [169, 254, 169, 254]
        || octets == [100, 100, 100, 200]
        || octets == [192, 0, 0, 192]
        || octets[0] == 0
    {
        return false;
    }
    if address.is_private() || octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return allow_private;
    }
    true
}

fn ipv6_is_allowed(address: Ipv6Addr, allow_private: bool) -> bool {
    const AWS_IPV6_METADATA: Ipv6Addr = Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254);
    let segments = address.segments();
    if address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xffc0) == 0xfe80
        || ipv6_is_deprecated_site_local(address)
        || address == AWS_IPV6_METADATA
    {
        return false;
    }
    if (segments[0] & 0xfe00) == 0xfc00 {
        return allow_private;
    }
    true
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use super::*;
    use serde_json::json;

    struct FakeResolver {
        answers: Vec<SocketAddr>,
        lookups: Arc<Mutex<Vec<(String, u16)>>>,
    }

    #[async_trait]
    impl ForwardDnsResolver for FakeResolver {
        async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>, ()> {
            self.lookups
                .lock()
                .expect("lookup lock")
                .push((host.to_string(), port));
            Ok(self.answers.clone())
        }
    }

    fn cluster(origin: Option<&str>, allow_private: bool) -> ClusterConfig {
        cluster_with_transport(origin, allow_private, false)
    }

    fn cluster_with_transport(
        origin: Option<&str>,
        allow_private: bool,
        allow_insecure_http: bool,
    ) -> ClusterConfig {
        let mut nodes = BTreeMap::new();
        nodes.insert(
            "worker-a".to_string(),
            ClusterNodeConfig {
                max_concurrent_dispatches: None,
                trusted_forward_origin: origin.map(str::to_string),
                allow_private_forwarding: allow_private,
                allow_insecure_http_forwarding: allow_insecure_http,
            },
        );
        ClusterConfig {
            nodes,
            ..ClusterConfig::default()
        }
    }

    fn capabilities() -> serde_json::Value {
        json!({"agentdesk_api": {"session_forwarding": true, "cancel_forwarding_v1": true}})
    }

    async fn resolve(
        cluster: &ClusterConfig,
        advertised: &str,
        answers: Vec<SocketAddr>,
    ) -> Result<TrustedForwardTarget, TrustedTargetError> {
        let resolver = FakeResolver {
            answers,
            lookups: Arc::new(Mutex::new(Vec::new())),
        };
        build_trusted_target_with_resolver(
            cluster,
            "worker-a",
            advertised,
            "session_forwarding",
            &capabilities(),
            &resolver,
        )
        .await
    }

    #[tokio::test]
    async fn missing_config_and_self_advertised_mismatch_fail_closed() {
        assert_eq!(
            resolve(
                &ClusterConfig::default(),
                "https://worker.example:8791",
                vec!["203.0.113.10:8791".parse().unwrap()],
            )
            .await
            .unwrap_err(),
            TrustedTargetError::MissingConfig
        );
        assert_eq!(
            resolve(
                &cluster(Some("https://trusted.example:8791"), false),
                "https://attacker.example:8791",
                vec!["203.0.113.10:8791".parse().unwrap()],
            )
            .await
            .unwrap_err(),
            TrustedTargetError::OriginMismatch
        );
    }

    #[tokio::test]
    async fn origin_parser_rejects_ambiguous_components() {
        for origin in [
            "file:///tmp/api",
            "https://user@worker.example:8791/",
            "https://worker.example:8791/?x=1",
            "https://worker.example:8791/#fragment",
            "https://worker.example:8791/api",
        ] {
            assert_eq!(
                resolve(
                    &cluster(Some(origin), false),
                    origin,
                    vec!["203.0.113.10:8791".parse().unwrap()],
                )
                .await
                .unwrap_err(),
                TrustedTargetError::InvalidConfiguredOrigin,
                "origin={origin}"
            );
        }
    }

    #[test]
    fn address_policy_rejects_unsafe_ipv4_ipv6_and_mixed_dns() {
        for address in [
            "127.0.0.1:8791",
            "0.0.0.0:8791",
            "169.254.169.254:80",
            "100.100.100.200:80",
            "192.0.0.192:80",
            "224.0.0.1:8791",
            "[::1]:8791",
            "[::]:8791",
            "[::ffff:127.0.0.1]:8791",
            "[::ffff:169.254.169.254]:80",
            "[::127.0.0.1]:8791",
            "[::169.254.169.254]:80",
            "[64:ff9b::127.0.0.1]:8791",
            "[64:ff9b::169.254.169.254]:80",
            "[64:ff9b:1::cb00:710a]:8791",
            "[fd00:ec2::254]:80",
            "[fe80::1]:8791",
            "[fec0::]:8791",
            "[feff::]:8791",
            "[ff02::1]:8791",
        ] {
            assert_eq!(
                validate_addresses(vec![address.parse().unwrap()], false).unwrap_err(),
                TrustedTargetError::UnsafeAddress,
                "address={address}"
            );
        }
        assert_eq!(
            validate_addresses(
                vec![
                    "203.0.113.10:8791".parse().unwrap(),
                    "127.0.0.1:8791".parse().unwrap(),
                ],
                false,
            )
            .unwrap_err(),
            TrustedTargetError::UnsafeAddress
        );
    }

    #[test]
    fn private_and_tailscale_addresses_require_operator_opt_in() {
        for address in ["10.0.0.2:8791", "100.64.1.2:8791", "[fd00::2]:8791"] {
            let parsed = address.parse().unwrap();
            assert_eq!(
                validate_addresses(vec![parsed], false).unwrap_err(),
                TrustedTargetError::UnsafeAddress
            );
            assert_eq!(
                validate_addresses(vec![parsed], true).unwrap(),
                vec![parsed]
            );
        }
        for address in [
            "127.0.0.1:8791",
            "169.254.169.254:80",
            "100.100.100.200:80",
            "192.0.0.192:80",
            "[fd00:ec2::254]:80",
            "[fec0::]:8791",
            "[feff::]:8791",
        ] {
            assert_eq!(
                validate_addresses(vec![address.parse().unwrap()], true).unwrap_err(),
                TrustedTargetError::UnsafeAddress,
                "address={address}"
            );
        }
    }

    #[tokio::test]
    async fn cleartext_transport_requires_private_addresses_and_both_consents() {
        let public = vec!["203.0.113.10:8791".parse().unwrap()];
        assert_eq!(
            resolve(
                &cluster_with_transport(Some("http://worker.example:8791"), true, true),
                "http://worker.example:8791",
                public,
            )
            .await
            .unwrap_err(),
            TrustedTargetError::InsecureTransport
        );

        let mixed = vec![
            "10.0.0.2:8791".parse().unwrap(),
            "203.0.113.10:8791".parse().unwrap(),
        ];
        assert_eq!(
            resolve(
                &cluster_with_transport(Some("http://worker.example:8791"), true, true),
                "http://worker.example:8791",
                mixed,
            )
            .await
            .unwrap_err(),
            TrustedTargetError::InsecureTransport
        );

        let private = vec!["10.0.0.2:8791".parse().unwrap()];
        for config in [
            cluster_with_transport(Some("http://worker.example:8791"), false, false),
            cluster_with_transport(Some("http://worker.example:8791"), true, false),
            cluster_with_transport(Some("http://worker.example:8791"), false, true),
        ] {
            assert_eq!(
                resolve(&config, "http://worker.example:8791", private.clone())
                    .await
                    .unwrap_err(),
                if config.nodes["worker-a"].allow_private_forwarding {
                    TrustedTargetError::InsecureTransport
                } else {
                    TrustedTargetError::UnsafeAddress
                }
            );
        }
        assert!(
            resolve(
                &cluster_with_transport(Some("http://worker.example:8791"), true, true),
                "http://worker.example:8791",
                private,
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn https_transport_keeps_public_and_private_address_semantics() {
        assert!(
            resolve(
                &cluster(Some("https://worker.example:8791"), false),
                "https://worker.example:8791",
                vec!["203.0.113.10:8791".parse().unwrap()],
            )
            .await
            .is_ok()
        );
        assert!(
            resolve(
                &cluster(Some("https://worker.example:8791"), true),
                "https://worker.example:8791",
                vec!["10.0.0.2:8791".parse().unwrap()],
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn canonical_origin_and_endpoint_preserve_authority() {
        let target = resolve(
            &cluster(Some("https://Worker.Example:443/"), false),
            "https://worker.example/",
            vec!["203.0.113.10:443".parse().unwrap()],
        )
        .await
        .unwrap();
        assert_eq!(
            target
                .endpoint_for_test("/api/sessions/42/tmux-output")
                .unwrap()
                .as_str(),
            "https://worker.example/api/sessions/42/tmux-output"
        );
        assert_eq!(
            target
                .endpoint_for_test("//attacker.example/steal")
                .unwrap_err(),
            TrustedTargetError::InvalidEndpoint
        );
    }

    #[tokio::test]
    async fn resolver_answer_is_pinned_once_before_request_builder_exists() {
        let lookups = Arc::new(Mutex::new(Vec::new()));
        let resolver = FakeResolver {
            answers: vec!["203.0.113.10:8791".parse().unwrap()],
            lookups: lookups.clone(),
        };
        let target = build_trusted_target_with_resolver(
            &cluster(Some("https://worker.example:8791"), false),
            "worker-a",
            "https://worker.example:8791/",
            "session_forwarding",
            &capabilities(),
            &resolver,
        )
        .await
        .unwrap();
        assert_eq!(
            lookups.lock().unwrap().as_slice(),
            &[("worker.example".to_string(), 8791)]
        );
        let request = target
            .request(Method::GET, "/api/sessions/42/tmux-output")
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(request.url().host_str(), Some("worker.example"));
        assert_eq!(lookups.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pinned_client_connects_to_validated_address_without_reresolving_hostname() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind pinned listener");
        let address = listener.local_addr().expect("pinned listener address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept pinned request");
            let mut buffer = [0_u8; 1024];
            let read = socket.read(&mut buffer).await.expect("read pinned request");
            let request = String::from_utf8_lossy(&buffer[..read]).to_ascii_lowercase();
            assert!(request.contains("host: rebinding.invalid"));
            let body = r#"{"ok":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write pinned response");
        });
        let client = build_pinned_client("rebinding.invalid", &[address]).expect("pinned client");
        let response = client
            .get(format!("http://rebinding.invalid:{}/probe", address.port()))
            .send()
            .await
            .expect("pinned request succeeds");
        server.await.expect("pinned server task");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
    }

    #[tokio::test]
    async fn capability_is_validated_before_dns_or_request_construction() {
        let lookups = Arc::new(Mutex::new(Vec::new()));
        let resolver = FakeResolver {
            answers: vec!["203.0.113.10:8791".parse().unwrap()],
            lookups: lookups.clone(),
        };
        assert_eq!(
            build_trusted_target_with_resolver(
                &cluster(Some("https://worker.example:8791"), false),
                "worker-a",
                "https://worker.example:8791/",
                "cancel_forwarding_v1",
                &json!({"agentdesk_api": {"session_forwarding": true}}),
                &resolver,
            )
            .await
            .unwrap_err(),
            TrustedTargetError::MissingCapability
        );
        assert!(lookups.lock().unwrap().is_empty());
    }
}
