//! Discord text-command risk policy (see issue #1005).
//!
//! Commands are classified into risk tiers. High-risk tiers (shell execution,
//! tool grants, runtime control) are gated behind owner identity and — for the
//! most dangerous ones — an explicit opt-in. The goal is to keep
//! `allow_all_users=true` usable for ordinary chat while preventing non-owners
//! from pivoting the bot into a remote shell or runtime kill switch.
//!
//! Surface:
//! - `CommandRisk` — enum of tiers.
//! - `command_risk` — `(command, arg1) → CommandRisk` lookup.
//! - `PolicyDecision` / `evaluate_policy` — authorization outcome helpers.
//! - `high_risk_enabled_via_env` — explicit opt-in via
//!   `AGENTDESK_DISCORD_HIGH_RISK_ENABLED=1`.
//! - `risk_tier_summary_for_help` — string surface for `!help` output.
//!
//! The policy deliberately lives outside `text_commands.rs` so it can be unit
//! tested without standing up Discord wiring.

/// Coarse risk tier for a Discord text command.
///
/// Ordering (low → high) reflects the amount of trust required:
/// `ReadOnly < Mutating < ShellOrToolGrant < CredentialSystem`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CommandRisk {
    /// Inspection-only commands. Safe for any authorized chat user.
    ReadOnly,
    /// Changes per-channel session state but cannot escape the sandbox.
    /// Includes session resets (`clear`, `deletesession`), in-flight turn
    /// cancellation (`stop`), and channel-scoped tmux respawn (`restart`).
    Mutating,
    /// Executes shell commands or grants new tool capabilities to the model.
    /// Equivalent to RCE on the host — owner only, explicit opt-in.
    ShellOrToolGrant,
    /// Modifies who can access the bot or rotates secrets/credentials.
    /// Owner only; always allowed for owner, never for anyone else.
    CredentialSystem,
}

impl CommandRisk {
    /// True for tiers that must go through the owner guard regardless of
    /// `allow_all_users`.
    pub(in crate::services::discord) fn is_high_risk(self) -> bool {
        matches!(
            self,
            CommandRisk::ShellOrToolGrant | CommandRisk::CredentialSystem
        )
    }

    /// True for tiers that additionally require an explicit opt-in (default
    /// disabled) — used for the most dangerous operations (shell, tool grants).
    pub(in crate::services::discord) fn requires_explicit_enable(self) -> bool {
        matches!(self, CommandRisk::ShellOrToolGrant)
    }
}

/// Look up the risk tier for a command name plus first argument.
///
/// `arg1` is consulted for commands that branch on operation (e.g. `!allowed
/// +Bash` is a tool grant, `!allowed -Bash` revokes one; both are
/// `ShellOrToolGrant`). Unknown commands default to `Mutating` — safe because
/// the dispatcher is responsible for rejecting truly unknown names.
pub(in crate::services::discord) fn command_risk(cmd: &str, _arg1: &str) -> CommandRisk {
    match cmd {
        // Pure inspection.
        "!help" | "!pwd" | "!health" | "!status" | "!inflight" | "!queue" | "!metrics"
        | "!allowedtools" | "!sessions" | "!receipt" | "!usage" => CommandRisk::ReadOnly,

        // Session-shaping. All scoped to the current channel: `!clear` /
        // `!deletesession` reset that channel's conversation memory; `!stop`
        // cancels its in-flight turn; `!restart` kills+respawns its tmux
        // session; `!debug` toggles the global debug-logging atomic but the
        // operator trusts every authorized user with that switch.
        "!start" | "!down" | "!cc" | "!skill" | "!meeting" | "!model" | "!fast" | "!goals"
        | "!clear" | "!deletesession" | "!stop" | "!restart" | "!debug" => CommandRisk::Mutating,

        // Shell execution and tool allowlist mutation — equivalent to RCE.
        // Issue #2653 recovery commands also run curated bash pipelines
        // (launchctl + ssh + git push --force-with-lease) so they share the
        // same tier: owner-only AND default-disabled behind
        // AGENTDESK_DISCORD_HIGH_RISK_ENABLED.
        "!shell" | "!allowed" | "!deadlock-recover" | "!machine-flip" | "!stuck-pr-rebase" => {
            CommandRisk::ShellOrToolGrant
        }

        // User/credential/system surface — owner-only, always.
        "!allowall" | "!adduser" | "!removeuser" | "!escalation" => CommandRisk::CredentialSystem,

        // Be conservative with anything unknown — treat as Mutating so the
        // dispatcher's own match arm handles the "unknown command" case.
        _ => CommandRisk::Mutating,
    }
}

/// Outcome of evaluating the command policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum PolicyDecision {
    /// Caller may proceed.
    Allow,
    /// Caller is not the owner and the command is high-risk.
    DenyNotOwner,
    /// Caller is the owner but the command requires explicit opt-in that is
    /// currently not enabled.
    DenyNotEnabled,
}

impl PolicyDecision {
    pub(in crate::services::discord) fn denial_message(self, cmd: &str) -> Option<String> {
        match self {
            PolicyDecision::Allow => None,
            PolicyDecision::DenyNotOwner => Some(format!(
                "`{cmd}` is restricted to the bot owner. `allow_all_users` does not grant access \
                 to high-risk commands."
            )),
            PolicyDecision::DenyNotEnabled => Some(format!(
                "`{cmd}` is disabled by default. Set `AGENTDESK_DISCORD_HIGH_RISK_ENABLED=1` in \
                 the bot environment to enable owner-only high-risk commands."
            )),
        }
    }
}

/// Core policy decision. Pure function so it can be unit tested.
pub(in crate::services::discord) fn evaluate_policy(
    risk: CommandRisk,
    is_owner: bool,
    high_risk_enabled: bool,
) -> PolicyDecision {
    if !risk.is_high_risk() {
        return PolicyDecision::Allow;
    }
    if !is_owner {
        return PolicyDecision::DenyNotOwner;
    }
    if risk.requires_explicit_enable() && !high_risk_enabled {
        return PolicyDecision::DenyNotEnabled;
    }
    PolicyDecision::Allow
}

/// Read the explicit-enable opt-in from the environment.
///
/// Accepts `1`, `true`, `yes`, `on` (case-insensitive). Anything else — or the
/// variable being unset — means high-risk `ShellOrToolGrant` commands stay
/// disabled even for the owner.
pub(in crate::services::discord) fn high_risk_enabled_via_env() -> bool {
    std::env::var("AGENTDESK_DISCORD_HIGH_RISK_ENABLED")
        .map(|raw| {
            let v = raw.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Look up the risk tier for a slash command (e.g. `/shell`, `/clear`).
///
/// Maps the slash form to the same tier as the matching text command so the
/// owner guard applies uniformly across both surfaces. Slash variants that do
/// not exist as text commands are mapped to their nearest text equivalent.
///
/// `arg1` is preserved for symmetry with [`command_risk`] but is not yet
/// consulted; both `/allowed +X` and `/allowed -X` already classify as
/// `ShellOrToolGrant` because the slash command itself implies a grant.
pub(in crate::services::discord) fn slash_command_risk(slash_cmd: &str) -> CommandRisk {
    match slash_cmd {
        // Inspection only.
        "/help" | "/pwd" | "/health" | "/status" | "/inflight" | "/queue" | "/metrics"
        | "/allowedtools" | "/sessions" | "/receipt" | "/usage" | "/adk" | "/cost" | "/context" => {
            CommandRisk::ReadOnly
        }

        // Per-channel session shaping (mirrors text-command tiers).
        "/start" | "/down" | "/cc" | "/skill" | "/meeting" | "/model" | "/node" | "/fast"
        | "/goals" | "/effort" | "/compact" | "/clear" | "/deletesession" | "/stop"
        | "/cancel-queued" | "/restart" | "/steer" | "/debug" => CommandRisk::Mutating,

        // RCE-equivalent surface.
        // `/deadlock-recover`, `/machine-flip`, and `/stuck-pr-rebase` (issue
        // #2653) run launchctl/ssh/git pipelines — owner-only + opt-in.
        "/shell" | "/allowed" | "/deadlock-recover" | "/machine-flip" | "/stuck-pr-rebase" => {
            CommandRisk::ShellOrToolGrant
        }

        // Credential / user-management surface.
        "/allowall" | "/adduser" | "/removeuser" | "/escalation" => CommandRisk::CredentialSystem,

        // Conservative default.
        _ => CommandRisk::Mutating,
    }
}

/// Short multi-line block suitable for `!help`. Documents each tier and its
/// current enable state.
pub(in crate::services::discord) fn risk_tier_summary_for_help(high_risk_enabled: bool) -> String {
    let shell_state = if high_risk_enabled {
        "owner-only, ENABLED"
    } else {
        "owner-only, DISABLED (set AGENTDESK_DISCORD_HIGH_RISK_ENABLED=1)"
    };
    format!(
        "**Command Risk Tiers** (issue #1005)\n\
         `read-only` — help/status/usage/receipt/metrics/allowedtools: any authorized user\n\
         `mutating` — start/down/skill(/cc)/meeting/model/node/fast/goals/effort/compact/clear/deletesession/stop/restart/debug: any authorized user\n\
         `read-only (Claude native)` — cost/context: any authorized user\n\
         `shell/tool-grant` — shell/allowed: {shell_state}\n\
         `credential/system` — allowall/adduser/removeuser/escalation: owner-only"
    )
}

#[cfg(test)]
mod steer_policy_tests {
    use super::*;

    #[test]
    fn steer_is_registered_as_mutating_and_not_high_risk() {
        // Explicit (not catch-all) registration: /steer is channel-scoped
        // mutating, allowed for any authorized user, never owner-gated.
        assert_eq!(slash_command_risk("/steer"), CommandRisk::Mutating);
        assert!(!slash_command_risk("/steer").is_high_risk());
    }

    #[test]
    fn cancel_queued_is_registered_as_mutating_and_not_high_risk() {
        assert_eq!(slash_command_risk("/cancel-queued"), CommandRisk::Mutating);
        assert!(!slash_command_risk("/cancel-queued").is_high_risk());
    }
}
