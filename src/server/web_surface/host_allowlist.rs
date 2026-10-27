//! Hosts this server answers to, built without operator input: loopback, this
//! machine's names and interface IPs, configured cluster origins, and the
//! Tailscale MagicDNS name when the `tailscale` CLI can report it.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::config::Config;

const IP_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const TAILSCALE_REQUERY_INTERVAL: Duration = Duration::from_secs(60);
const TAILSCALE_TIMEOUT: Duration = Duration::from_secs(3);
// launchd and systemd PATHs often miss the CLI, so the usual install paths follow.
const TAILSCALE_CLI: &[&str] = &[
    "tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
];

pub(crate) struct HostAllowlist {
    names: RwLock<HashSet<String>>,
    // First label of `<label>.<tailnet>.ts.net` when the CLI cannot be queried.
    ts_net_label: Option<String>,
    configured_ips: HashSet<IpAddr>,
    local_ips: RwLock<HashSet<IpAddr>>,
    ips_refreshed: Mutex<Instant>,
    tailscale_queried: Mutex<Option<Instant>>,
    discover: bool,
    #[cfg(test)]
    tailscale_responses: Mutex<Option<std::collections::VecDeque<Option<serde_json::Value>>>>,
}

impl HostAllowlist {
    pub(crate) fn from_config(config: &Config) -> Arc<Self> {
        let mut names: HashSet<String> = ["localhost"].into_iter().map(String::from).collect();
        let os_name = sysinfo::System::host_name().map(|name| normalize_name(&name));
        let short = os_name
            .as_deref()
            .and_then(|name| name.split('.').next())
            .filter(|label| !label.is_empty())
            .map(str::to_string);
        if let Some(name) = os_name.filter(|name| !name.is_empty()) {
            names.insert(name);
        }
        if let Some(short) = &short {
            names.insert(short.clone());
            names.insert(format!("{short}.local"));
        }
        let (configured_names, configured_ips) = configured_hosts(config);
        names.extend(configured_names);
        let allowlist = Arc::new(Self {
            names: RwLock::new(names),
            ts_net_label: short.map(|label| magicdns_label(&label)),
            configured_ips,
            local_ips: RwLock::new(interface_ips()),
            ips_refreshed: Mutex::new(Instant::now()),
            tailscale_queried: Mutex::new(None),
            discover: true,
            #[cfg(test)]
            tailscale_responses: Mutex::new(None),
        });
        allowlist.query_tailscale();
        allowlist
    }

    #[cfg(test)]
    pub(crate) fn fixed(names: &[&str], ips: &[IpAddr], ts_net_label: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            names: RwLock::new(names.iter().map(|name| normalize_name(name)).collect()),
            ts_net_label: ts_net_label.map(str::to_string),
            configured_ips: HashSet::new(),
            local_ips: RwLock::new(ips.iter().copied().collect()),
            ips_refreshed: Mutex::new(Instant::now()),
            tailscale_queried: Mutex::new(None),
            discover: false,
            tailscale_responses: Mutex::new(None),
        })
    }

    /// `host` is a Host header value or URI authority (`name[:port]`, `[v6][:port]`).
    pub(crate) fn host_allowed(self: &Arc<Self>, host: &str) -> bool {
        let Some((name, _)) = split_authority(host) else {
            return false;
        };
        match name.parse::<IpAddr>() {
            Ok(ip) => self.ip_allowed(ip.to_canonical()),
            Err(_) => self.name_allowed(&name),
        }
    }

    /// Same origin as the validated Host, or an exact allowlisted origin host. The
    /// `*.ts.net` fallback is not used here: it only guards Host against rebinding.
    pub(crate) fn origin_allowed(self: &Arc<Self>, origin: &str, host: Option<&str>) -> bool {
        let Ok(url) = url::Url::parse(origin) else {
            return false;
        };
        let default_port = match url.scheme() {
            "http" => 80,
            "https" => 443,
            _ => return false,
        };
        let Some(origin_host) = url.host_str().map(normalize_name) else {
            return false;
        };
        let origin_host = origin_host.trim_start_matches('[').trim_end_matches(']');
        let origin_port = url.port().unwrap_or(default_port);
        if let Some((name, port)) = host.and_then(split_authority)
            && name == origin_host
            && port.unwrap_or(default_port) == origin_port
        {
            return true;
        }
        match origin_host.parse::<IpAddr>() {
            Ok(ip) => self.ip_allowed(ip.to_canonical()),
            Err(_) => {
                if self.names_contains(origin_host) {
                    return true;
                }
                self.query_tailscale();
                false
            }
        }
    }

    fn name_allowed(self: &Arc<Self>, name: &str) -> bool {
        if self.names_contains(name) {
            return true;
        }
        self.query_tailscale();
        if name.ends_with(".ts.net") {
            return self.ts_net_label.as_deref().is_some_and(|label| {
                name.strip_prefix(label)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .and_then(|rest| rest.strip_suffix(".ts.net"))
                    .is_some_and(|tailnet| !tailnet.is_empty() && !tailnet.contains('.'))
            });
        }
        false
    }

    fn names_contains(&self, name: &str) -> bool {
        self.names
            .read()
            .map(|names| names.contains(name))
            .unwrap_or(false)
    }

    fn ip_allowed(&self, ip: IpAddr) -> bool {
        if ip.is_loopback() || self.configured_ips.contains(&ip) || self.has_local_ip(ip) {
            return true;
        }
        // An address can appear after boot (VPN, DHCP); re-read interfaces on a miss.
        let refresh = self.discover
            && self.ips_refreshed.lock().is_ok_and(|mut at| {
                let due = at.elapsed() >= IP_REFRESH_INTERVAL;
                if due {
                    *at = Instant::now();
                }
                due
            });
        if refresh && let Ok(mut ips) = self.local_ips.write() {
            *ips = interface_ips();
        }
        refresh && self.has_local_ip(ip)
    }

    fn has_local_ip(&self, ip: IpAddr) -> bool {
        self.local_ips
            .read()
            .map(|ips| ips.contains(&ip))
            .unwrap_or(false)
    }

    /// Best effort and rate limited; a later request sees the learned name.
    fn query_tailscale(self: &Arc<Self>) {
        if !self.discover {
            return;
        }
        let Ok(mut queried) = self.tailscale_queried.lock() else {
            return;
        };
        if queried.is_some_and(|at| at.elapsed() < TAILSCALE_REQUERY_INTERVAL) {
            return;
        }
        *queried = Some(Instant::now());
        drop(queried);
        let this = Arc::clone(self);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let Some(status) = this.tailscale_status().await else {
                return;
            };
            let (names, ips) = parse_tailscale_self(&status);
            if let Ok(mut known) = this.names.write() {
                known.extend(names);
            }
            if let Ok(mut known) = this.local_ips.write() {
                known.extend(ips);
            }
        });
    }

    async fn tailscale_status(&self) -> Option<serde_json::Value> {
        #[cfg(test)]
        if let Some(status) = tests::tailscale_status(self) {
            return status;
        }
        tailscale_status().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn tailscale_status(hosts: &HostAllowlist) -> Option<Option<serde_json::Value>> {
        hosts
            .tailscale_responses
            .lock()
            .unwrap()
            .as_mut()
            .map(|responses| responses.pop_front().expect("unexpected Tailscale query"))
    }

    #[test]
    fn advertised_environment_url_is_allowed_with_yaml_precedence() {
        let _env = crate::config::TestEnvVarGuard::set_path(
            "AGENTDESK_CLUSTER_API_BASE_URL",
            std::path::Path::new(" https://env-worker.example.test:8791/ "),
        );
        let mut config = Config::default();
        let hosts = HostAllowlist::from_config(&config);
        assert!(hosts.host_allowed("env-worker.example.test:8791"));
        assert!(hosts.origin_allowed("https://env-worker.example.test:8791", Some("localhost")));
        config.cluster.api_base_url = Some("http://yaml-worker.example.test:8791/".into());
        let hosts = HostAllowlist::from_config(&config);
        assert!(hosts.host_allowed("yaml-worker.example.test:8791"));
        assert!(!hosts.host_allowed("env-worker.example.test:8791"));
    }

    #[test]
    fn configured_ip_literals_survive_interface_refresh() {
        let mut config = Config::default();
        config.server.host = "192.0.2.40".into();
        config.cluster.api_base_url = Some("http://[2001:db8::40]:8791".into());
        config.cluster.nodes.insert(
            "proxy".into(),
            crate::config::ClusterNodeConfig {
                trusted_forward_origin: Some("http://192.0.2.41:8791".into()),
                ..Default::default()
            },
        );
        let hosts = HostAllowlist::from_config(&config);
        for _ in 0..2 {
            for host in ["192.0.2.40:8791", "[2001:db8::40]:8791", "192.0.2.41"] {
                assert!(hosts.host_allowed(host), "{host}");
                assert!(hosts.origin_allowed(&format!("http://{host}"), Some("localhost")));
            }
            *hosts.ips_refreshed.lock().unwrap() = Instant::now() - IP_REFRESH_INTERVAL;
            assert!(!hosts.host_allowed("192.0.2.250"));
            assert!(hosts.ips_refreshed.lock().unwrap().elapsed() < IP_REFRESH_INTERVAL);
        }
        config.server.host = "0.0.0.0".into();
        assert!(!HostAllowlist::from_config(&config).host_allowed("0.0.0.0"));
        config.server.host = "::".into();
        assert!(!HostAllowlist::from_config(&config).host_allowed("[::]"));
    }

    async fn wait_for_queries(hosts: &HostAllowlist, remaining: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if hosts
                    .tailscale_responses
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .len()
                    == remaining
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Tailscale query did not run");
    }

    async fn recovery_after_initial_failure(origin_only: bool) {
        let mut hosts = HostAllowlist::fixed(&["localhost"], &[], None);
        let inner = Arc::get_mut(&mut hosts).unwrap();
        inner.discover = true;
        *inner.tailscale_responses.lock().unwrap() = Some(
            [
                None,
                Some(serde_json::json!({"Self": {"DNSName": "renamed.tail1234.ts.net."}})),
            ]
            .into(),
        );
        hosts.query_tailscale();
        wait_for_queries(&hosts, 1).await;
        let check = || {
            if origin_only {
                hosts.origin_allowed("https://renamed:8791", Some("localhost:8791"))
            } else {
                hosts.host_allowed("renamed:8791")
            }
        };
        assert!(!check());
        tokio::task::yield_now().await;
        assert_eq!(
            hosts
                .tailscale_responses
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        *hosts.tailscale_queried.lock().unwrap() =
            Some(Instant::now() - TAILSCALE_REQUERY_INTERVAL);
        assert!(!check(), "unconfirmed name must stay denied");
        wait_for_queries(&hosts, 0).await;
        assert!(
            check(),
            "learned short name must recover without an FQDN request"
        );
        assert!(!hosts.host_allowed("unrelated"));
        assert!(!hosts.origin_allowed("https://unrelated", Some("localhost")));
    }

    #[tokio::test]
    async fn short_host_requeries_after_initial_tailscale_failure() {
        recovery_after_initial_failure(false).await;
    }

    #[tokio::test]
    async fn origin_only_requeries_after_initial_tailscale_failure() {
        recovery_after_initial_failure(true).await;
    }
}

pub(crate) fn configured_hosts(config: &Config) -> (Vec<String>, HashSet<IpAddr>) {
    let mut names = Vec::new();
    let mut ips = HashSet::new();
    let host = config.server.host.trim();
    match host.parse::<IpAddr>() {
        Ok(ip) if !ip.is_unspecified() => {
            ips.insert(ip.to_canonical());
        }
        Err(_) if !host.is_empty() => names.push(normalize_name(host)),
        _ => {}
    }
    let api_base_url =
        crate::services::cluster::session_routing::resolve_worker_api_base_url(&config.cluster);
    let origins = api_base_url.iter().chain(
        config
            .cluster
            .nodes
            .values()
            .filter_map(|node| node.trusted_forward_origin.as_ref()),
    );
    for origin in origins {
        if let Ok(url) = url::Url::parse(origin) {
            match url.host() {
                Some(url::Host::Domain(domain)) => names.push(normalize_name(domain)),
                Some(url::Host::Ipv4(ip)) => {
                    ips.insert(IpAddr::V4(ip));
                }
                Some(url::Host::Ipv6(ip)) => {
                    ips.insert(IpAddr::V6(ip).to_canonical());
                }
                None => {}
            }
        }
    }
    ips.retain(|ip| !ip.is_unspecified());
    (names, ips)
}

/// `(name, port)` with the name lowercased and any trailing dot removed.
fn split_authority(value: &str) -> Option<(String, Option<u16>)> {
    let authority: axum::http::uri::Authority = value.trim().parse().ok()?;
    if authority.as_str().contains('@') {
        return None;
    }
    let name = normalize_name(authority.host());
    let name = name
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    (!name.is_empty()).then_some((name, authority.port_u16()))
}

fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

// Tailscale derives the default machine name this way from the OS hostname.
fn magicdns_label(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn interface_ips() -> HashSet<IpAddr> {
    sysinfo::Networks::new_with_refreshed_list()
        .iter()
        .flat_map(|(_, data)| data.ip_networks().iter().map(|net| net.addr.to_canonical()))
        .collect()
}

async fn tailscale_status() -> Option<serde_json::Value> {
    for program in TAILSCALE_CLI {
        let mut command = tokio::process::Command::new(program);
        command.args(["status", "--json"]).kill_on_drop(true);
        let Ok(Ok(output)) = tokio::time::timeout(TAILSCALE_TIMEOUT, command.output()).await else {
            continue;
        };
        if output.status.success() {
            return serde_json::from_slice(&output.stdout).ok();
        }
    }
    None
}

pub(crate) fn parse_tailscale_self(status: &serde_json::Value) -> (Vec<String>, Vec<IpAddr>) {
    let own = &status["Self"];
    let mut names = Vec::new();
    if let Some(dns) = own["DNSName"]
        .as_str()
        .map(normalize_name)
        .filter(|n| !n.is_empty())
    {
        if let Some(label) = dns.split('.').next() {
            names.push(label.to_string());
        }
        names.push(dns);
    }
    let ips = own["TailscaleIPs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|ip| ip.as_str()?.parse::<IpAddr>().ok())
        .collect();
    (names, ips)
}
