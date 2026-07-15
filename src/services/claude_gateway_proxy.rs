use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const ANTHROPIC_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
const GATEWAY_MODEL_DISCOVERY_ENV: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeGatewayProxyEnv {
    base_url: String,
}

impl ClaudeGatewayProxyEnv {
    pub(crate) fn append_shell_exports(&self, output: &mut String) {
        output.push_str(&format!(
            "export {ANTHROPIC_BASE_URL_ENV}='{}'\n",
            self.base_url.replace('\'', "'\\''")
        ));
        output.push_str(&format!("export {GATEWAY_MODEL_DISCOVERY_ENV}=1\n"));
    }

    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        command.env(ANTHROPIC_BASE_URL_ENV, &self.base_url);
        command.env(GATEWAY_MODEL_DISCOVERY_ENV, "1");
    }

    pub(crate) fn append_to_env_vars(&self, env_vars: &mut Vec<(String, String)>) {
        env_vars.push((ANTHROPIC_BASE_URL_ENV.to_string(), self.base_url.clone()));
        env_vars.push((GATEWAY_MODEL_DISCOVERY_ENV.to_string(), "1".to_string()));
    }
}

pub(crate) fn resolve_for_launch() -> Option<ClaudeGatewayProxyEnv> {
    let Some(config) = crate::config_live_reload::current() else {
        return None;
    };
    let enabled = config.runtime.claude_gateway_proxy_enabled;
    let proxy_url = config.runtime.resolved_claude_gateway_proxy_url();
    decide_launch_env(
        enabled,
        proxy_url,
        || proxy_reachable(proxy_url),
        |url| {
            tracing::warn!(
                proxy_url = url,
                "Claude gateway proxy is enabled but unreachable; skipping gateway env injection; Claude will run native"
            );
        },
    )
}

fn decide_launch_env(
    enabled: bool,
    proxy_url: &str,
    probe: impl FnOnce() -> bool,
    warn_unreachable: impl FnOnce(&str),
) -> Option<ClaudeGatewayProxyEnv> {
    if !enabled {
        return None;
    }
    if !probe() {
        warn_unreachable(proxy_url);
        return None;
    }
    Some(ClaudeGatewayProxyEnv {
        base_url: proxy_url.to_string(),
    })
}

fn proxy_reachable(proxy_url: &str) -> bool {
    proxy_reachable_with_hostname_probe(
        proxy_url,
        PROXY_CONNECT_TIMEOUT,
        resolve_hostname_and_connect,
    )
}

fn proxy_reachable_with_hostname_probe(
    proxy_url: &str,
    timeout: Duration,
    hostname_probe: impl FnOnce(String, u16, Duration) -> bool + Send + 'static,
) -> bool {
    let Ok(parsed) = url::Url::parse(proxy_url) else {
        return false;
    };
    let (Some(host), Some(port)) = (parsed.host_str(), parsed.port_or_known_default()) else {
        return false;
    };

    if let Ok(ip) = host.parse::<IpAddr>() {
        return TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout).is_ok();
    }

    let host = host.to_string();
    run_probe_with_deadline(timeout, move || hostname_probe(host, port, timeout))
}

fn resolve_hostname_and_connect(host: String, port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Ok(addresses) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    for address in addresses {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return false;
        };
        if TcpStream::connect_timeout(&address, remaining).is_ok() {
            return true;
        }
    }
    false
}

fn run_probe_with_deadline(
    timeout: Duration,
    probe: impl FnOnce() -> bool + Send + 'static,
) -> bool {
    let (sender, receiver) = mpsc::sync_channel(1);
    if thread::Builder::new()
        .name("claude-gateway-probe".to_string())
        .spawn(move || {
            let _ = sender.send(probe());
        })
        .is_err()
    {
        return false;
    }
    receiver.recv_timeout(timeout).unwrap_or(false)
}

#[cfg(test)]
pub(crate) fn launch_env_for_test(
    enabled: bool,
    proxy_url: &str,
    reachable: bool,
) -> Option<ClaudeGatewayProxyEnv> {
    decide_launch_env(enabled, proxy_url, || reachable, |_| {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::net::TcpListener;

    fn rendered_env(env: Option<&ClaudeGatewayProxyEnv>) -> String {
        let mut rendered = String::new();
        if let Some(env) = env {
            env.append_shell_exports(&mut rendered);
        }
        rendered
    }

    #[test]
    fn enabled_and_reachable_injects_both_proxy_vars() {
        let warned = Cell::new(false);
        let env = decide_launch_env(
            true,
            "http://127.0.0.1:10100",
            || true,
            |_| warned.set(true),
        );
        let rendered = rendered_env(env.as_ref());

        assert!(rendered.contains(ANTHROPIC_BASE_URL_ENV));
        assert!(rendered.contains(GATEWAY_MODEL_DISCOVERY_ENV));
        assert!(!warned.get());
    }

    #[test]
    fn enabled_and_unreachable_skips_both_proxy_vars_and_warns() {
        let warned = Cell::new(false);
        let env = decide_launch_env(
            true,
            "http://127.0.0.1:10100",
            || false,
            |_| warned.set(true),
        );
        let rendered = rendered_env(env.as_ref());

        assert!(!rendered.contains(ANTHROPIC_BASE_URL_ENV));
        assert!(!rendered.contains(GATEWAY_MODEL_DISCOVERY_ENV));
        assert!(warned.get());
    }

    #[test]
    fn disabled_skips_proxy_vars_without_probing_or_warning() {
        for reachable in [false, true] {
            let probed = Cell::new(false);
            let warned = Cell::new(false);
            let env = decide_launch_env(
                false,
                "http://127.0.0.1:10100",
                || {
                    probed.set(true);
                    reachable
                },
                |_| warned.set(true),
            );
            let rendered = rendered_env(env.as_ref());

            assert!(!rendered.contains(ANTHROPIC_BASE_URL_ENV));
            assert!(!rendered.contains(GATEWAY_MODEL_DISCOVERY_ENV));
            assert!(!probed.get());
            assert!(!warned.get());
        }
    }

    #[test]
    fn ip_literal_uses_direct_connect_fast_path() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let reachable = proxy_reachable_with_hostname_probe(
            &format!("http://{address}"),
            Duration::from_millis(200),
            |_, _, _| panic!("IP literals must not invoke hostname resolution"),
        );

        assert!(reachable);
    }

    #[test]
    fn hostname_resolution_and_connect_obey_outer_deadline() {
        let timeout = Duration::from_millis(20);
        let started = Instant::now();

        let reachable = proxy_reachable_with_hostname_probe(
            "http://bad-hostname.invalid:10100",
            timeout,
            |_, _, _| {
                thread::sleep(Duration::from_millis(200));
                false
            },
        );

        assert!(!reachable);
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "hostname probe exceeded its outer deadline: {:?}",
            started.elapsed()
        );
    }
}
