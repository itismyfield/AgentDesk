//! Which nodes may run TUI intake once the O writer owns output: only the gateway posts to Discord.

/// Build switch for the O writer. False keeps every intake worker and health reason as before.
pub(crate) const O_TUI_WRITER: bool = false;

/// Degraded reason a non-gateway node carries for each provider whose intake it refuses.
pub(crate) const TUI_OUTPUT_REQUIRES_GATEWAY: &str = "tui_output_requires_gateway";

/// How this process holds the Discord gateway for one provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostRole {
    Gateway,
    Runner,
    Standby,
}

/// Providers whose TUI output the O writer delivers; other providers keep their intake everywhere.
fn writes_tui_output(provider: &str) -> bool {
    let provider = provider.trim();
    provider.eq_ignore_ascii_case("claude") || provider.eq_ignore_ascii_case("codex")
}

pub(crate) fn intake_worker_allowed(o_tui_writer: bool, provider: &str, role: HostRole) -> bool {
    !o_tui_writer || role == HostRole::Gateway || !writes_tui_output(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [HostRole; 3] = [HostRole::Gateway, HostRole::Runner, HostRole::Standby];
    const PROVIDERS: [&str; 5] = ["claude", "codex", "gemini", "opencode", "Claude "];

    #[test]
    fn switch_off_keeps_every_intake_worker() {
        for role in ROLES {
            for provider in PROVIDERS {
                assert!(
                    intake_worker_allowed(false, provider, role),
                    "{provider} {role:?}"
                );
            }
        }
    }

    #[test]
    fn switch_on_refuses_tui_intake_off_the_gateway_only() {
        for provider in PROVIDERS {
            assert!(intake_worker_allowed(true, provider, HostRole::Gateway));
        }
        for role in [HostRole::Runner, HostRole::Standby] {
            for provider in ["claude", "codex", "Claude "] {
                assert!(
                    !intake_worker_allowed(true, provider, role),
                    "{provider} {role:?}"
                );
            }
            for provider in ["gemini", "opencode"] {
                assert!(
                    intake_worker_allowed(true, provider, role),
                    "{provider} {role:?}"
                );
            }
        }
    }
}
