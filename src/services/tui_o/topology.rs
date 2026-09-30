//! Which nodes may run TUI intake once the O writer owns output: only the gateway posts to Discord.

use super::channel_policy::BootChannels;
use crate::services::agent_protocol::RuntimeHandoffKind;

/// Build switch for the O writer. False keeps every intake worker and health reason as before.
pub(crate) const O_TUI_WRITER: bool = false;

/// Degraded reason a non-gateway node carries for each provider whose writer channels only the
/// gateway may serve, or whose intake it refuses.
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

/// How many of `provider`'s boot writer channels this node leaves to the gateway; `None` when
/// the boot list is unknown, which health must not read as zero.
pub(crate) fn gateway_only_channels(
    o_tui_writer: bool,
    provider: &str,
    role: HostRole,
    boot: Option<&BootChannels>,
) -> Option<usize> {
    let kind = match provider.trim().to_ascii_lowercase().as_str() {
        "claude" => RuntimeHandoffKind::ClaudeTui,
        "codex" => RuntimeHandoffKind::CodexTui,
        _ => return Some(0),
    };
    if !o_tui_writer || role == HostRole::Gateway {
        return Some(0);
    }
    let boot = boot?;
    let owned = boot
        .channels()
        .iter()
        .filter(|id| boot.kind(**id) == Some(kind));
    Some(owned.count())
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

    #[test]
    fn only_a_non_gateway_role_leaves_writer_channels_to_the_gateway() {
        use crate::services::tui_o::cutover::test_override;
        use RuntimeHandoffKind::{ClaudeTui, CodexTui};
        let _o = test_override::force_channels(&[(1, ClaudeTui), (2, ClaudeTui), (3, CodexTui)]);
        test_override::with_channels(|boot| {
            let count = |on, provider, role| gateway_only_channels(on, provider, role, boot);
            assert_eq!(count(true, "claude", HostRole::Runner), Some(2));
            assert_eq!(count(true, "Codex", HostRole::Standby), Some(1));
            assert_eq!(count(true, "claude", HostRole::Gateway), Some(0));
            assert_eq!(count(true, "gemini", HostRole::Runner), Some(0));
            assert_eq!(count(false, "claude", HostRole::Runner), Some(0));
        });
        // An unknown list is not an empty one: health must keep the reason without evidence.
        assert_eq!(
            gateway_only_channels(true, "claude", HostRole::Runner, None),
            None
        );
    }
}
