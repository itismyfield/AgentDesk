use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PROXY_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const ANTHROPIC_BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";
const GATEWAY_MODEL_DISCOVERY_ENV: &str = "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaudeGatewayProxyEnv {
    Inject { base_url: String },
    Scrub,
}

impl ClaudeGatewayProxyEnv {
    pub(crate) fn append_shell_env(&self, output: &mut String) {
        match self {
            Self::Inject { base_url } => {
                output.push_str(&format!(
                    "export {ANTHROPIC_BASE_URL_ENV}='{}'\n",
                    base_url.replace('\'', "'\\''")
                ));
                output.push_str(&format!("export {GATEWAY_MODEL_DISCOVERY_ENV}=1\n"));
            }
            Self::Scrub => {
                output.push_str(&format!("unset {ANTHROPIC_BASE_URL_ENV}\n"));
                output.push_str(&format!("unset {GATEWAY_MODEL_DISCOVERY_ENV}\n"));
            }
        }
    }

    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        match self {
            Self::Inject { base_url } => {
                command.env(ANTHROPIC_BASE_URL_ENV, base_url);
                command.env(GATEWAY_MODEL_DISCOVERY_ENV, "1");
            }
            Self::Scrub => {
                command.env_remove(ANTHROPIC_BASE_URL_ENV);
                command.env_remove(GATEWAY_MODEL_DISCOVERY_ENV);
            }
        }
    }
}

pub(crate) fn resolve_for_launch() -> ClaudeGatewayProxyEnv {
    let Some(config) = crate::config_live_reload::current() else {
        return ClaudeGatewayProxyEnv::Scrub;
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
                "Claude gateway proxy is enabled but unreachable; scrubbing gateway env; Claude will run native"
            );
        },
    )
}

/// Reconstruct the launch gateway decision from this process's environment.
///
/// Used by the `agentdesk tmux-wrapper` process, which has no installed config
/// and therefore cannot run [`resolve_for_launch`]. Its managed dcserver parent
/// already resolved the decision (with config) and applied it to the wrapper's
/// environment, so reconstructing that decision here and re-applying it to the
/// Claude child is idempotent: Inject with the inherited base URL (the parent
/// injected it), or Scrub when the base URL is absent (the parent scrubbed it).
pub(crate) fn reconstruct_launch_env_from_process() -> ClaudeGatewayProxyEnv {
    reconstruct_launch_env(std::env::var(ANTHROPIC_BASE_URL_ENV).ok())
}

fn reconstruct_launch_env(base_url: Option<String>) -> ClaudeGatewayProxyEnv {
    match base_url {
        Some(url) if !url.trim().is_empty() => ClaudeGatewayProxyEnv::Inject { base_url: url },
        _ => ClaudeGatewayProxyEnv::Scrub,
    }
}

fn decide_launch_env(
    enabled: bool,
    proxy_url: &str,
    probe: impl FnOnce() -> bool,
    warn_unreachable: impl FnOnce(&str),
) -> ClaudeGatewayProxyEnv {
    if !enabled {
        return ClaudeGatewayProxyEnv::Scrub;
    }
    if !probe() {
        warn_unreachable(proxy_url);
        return ClaudeGatewayProxyEnv::Scrub;
    }
    ClaudeGatewayProxyEnv::Inject {
        base_url: proxy_url.to_string(),
    }
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
    let (Some(host), Some(port)) = (parsed.host(), parsed.port_or_known_default()) else {
        return false;
    };

    match host {
        url::Host::Ipv4(address) => connect_ip_literal(IpAddr::V4(address), port, timeout),
        url::Host::Ipv6(address) => connect_ip_literal(IpAddr::V6(address), port, timeout),
        url::Host::Domain(host) => {
            let host = host.to_string();
            run_probe_with_deadline(timeout, move || hostname_probe(host, port, timeout))
        }
    }
}

fn connect_ip_literal(ip: IpAddr, port: u16, timeout: Duration) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), timeout).is_ok()
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
) -> ClaudeGatewayProxyEnv {
    decide_launch_env(enabled, proxy_url, || reachable, |_| {})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn rendered_env(env: &ClaudeGatewayProxyEnv) -> String {
        let mut rendered = String::new();
        env.append_shell_env(&mut rendered);
        rendered
    }

    fn command_env(
        env: &ClaudeGatewayProxyEnv,
    ) -> std::collections::HashMap<String, Option<String>> {
        let mut command = Command::new("claude");
        command
            .env(ANTHROPIC_BASE_URL_ENV, "http://inherited.example:9999")
            .env(GATEWAY_MODEL_DISCOVERY_ENV, "foreign-value");
        env.apply_to_command(&mut command);
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn reconstruct_launch_env_maps_inherited_base_url_to_inject_or_scrub() {
        // A present, non-empty base URL means the managed parent injected the
        // proxy → reconstruct Inject with that exact URL (idempotent re-apply).
        assert_eq!(
            reconstruct_launch_env(Some("http://127.0.0.1:10100".to_string())),
            ClaudeGatewayProxyEnv::Inject {
                base_url: "http://127.0.0.1:10100".to_string()
            }
        );
        // Absent or blank base URL means the parent scrubbed (or never set) it →
        // reconstruct Scrub. This keeps the wrapper's re-application idempotent
        // with the parent's Scrub decision instead of leaking a stale value.
        assert_eq!(reconstruct_launch_env(None), ClaudeGatewayProxyEnv::Scrub);
        assert_eq!(
            reconstruct_launch_env(Some("   ".to_string())),
            ClaudeGatewayProxyEnv::Scrub
        );
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
        let rendered = rendered_env(&env);
        let command_env = command_env(&env);

        assert!(rendered.contains(ANTHROPIC_BASE_URL_ENV));
        assert!(rendered.contains(GATEWAY_MODEL_DISCOVERY_ENV));
        assert_eq!(
            command_env.get(ANTHROPIC_BASE_URL_ENV),
            Some(&Some("http://127.0.0.1:10100".to_string()))
        );
        assert_eq!(
            command_env.get(GATEWAY_MODEL_DISCOVERY_ENV),
            Some(&Some("1".to_string()))
        );
        assert!(!warned.get());
    }

    #[test]
    fn enabled_and_unreachable_scrubs_both_proxy_vars_and_warns() {
        let warned = Cell::new(false);
        let env = decide_launch_env(
            true,
            "http://127.0.0.1:10100",
            || false,
            |_| warned.set(true),
        );
        let rendered = rendered_env(&env);
        let command_env = command_env(&env);

        assert!(rendered.contains(&format!("unset {ANTHROPIC_BASE_URL_ENV}\n")));
        assert!(rendered.contains(&format!("unset {GATEWAY_MODEL_DISCOVERY_ENV}\n")));
        assert_eq!(command_env.get(ANTHROPIC_BASE_URL_ENV), Some(&None));
        assert_eq!(command_env.get(GATEWAY_MODEL_DISCOVERY_ENV), Some(&None));
        assert!(warned.get());
    }

    #[test]
    fn disabled_scrubs_pre_set_proxy_vars_without_probing_or_warning() {
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
            let rendered = rendered_env(&env);
            let command_env = command_env(&env);

            assert!(rendered.contains(&format!("unset {ANTHROPIC_BASE_URL_ENV}\n")));
            assert!(rendered.contains(&format!("unset {GATEWAY_MODEL_DISCOVERY_ENV}\n")));
            assert_eq!(command_env.get(ANTHROPIC_BASE_URL_ENV), Some(&None));
            assert_eq!(command_env.get(GATEWAY_MODEL_DISCOVERY_ENV), Some(&None));
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
    fn ipv6_literal_uses_direct_connect_fast_path() {
        let listener = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let reachable = proxy_reachable_with_hostname_probe(
            &format!("http://[::1]:{port}"),
            Duration::from_millis(200),
            |_, _, _| panic!("IPv6 literals must not invoke hostname resolution"),
        );

        assert!(reachable);
    }

    #[test]
    fn hostname_resolution_and_connect_obey_outer_deadline() {
        // The worker "slow operation" sleeps far longer than the outer deadline so
        // the test proves the deadline cut it off. The upper bound is kept generously
        // above the deadline (not a tight margin) so it stays robust under heavy CI /
        // build load where thread scheduling can add >100ms of wakeup latency; the
        // production deadline is ~1s, for which this overhead is negligible.
        let timeout = Duration::from_millis(20);
        let worker_sleep = Duration::from_millis(2000);
        let robust_upper_bound = Duration::from_millis(1000);
        let probe_invoked = Arc::new(AtomicBool::new(false));
        let probe_invoked_by_worker = Arc::clone(&probe_invoked);
        let started = Instant::now();

        let reachable = proxy_reachable_with_hostname_probe(
            "http://bad-hostname.invalid:10100",
            timeout,
            move |_, _, _| {
                probe_invoked_by_worker.store(true, Ordering::SeqCst);
                thread::sleep(worker_sleep);
                false
            },
        );
        let returned_after = started.elapsed();

        assert!(!reachable);
        assert!(
            returned_after < robust_upper_bound,
            "hostname probe did not obey its outer deadline (returned before the {:?} worker sleep expected): {:?}",
            worker_sleep,
            returned_after
        );
        let invocation_deadline = Instant::now() + robust_upper_bound;
        while !probe_invoked.load(Ordering::SeqCst) && Instant::now() < invocation_deadline {
            thread::yield_now();
        }
        assert!(
            probe_invoked.load(Ordering::SeqCst),
            "domain branch returned without invoking the hostname probe closure"
        );
    }
}
