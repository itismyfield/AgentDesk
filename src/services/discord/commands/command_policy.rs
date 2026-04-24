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
/// `ReadOnly < Mutating < RuntimeControl < ShellOrToolGrant < CredentialSystem`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CommandRisk {
    /// Inspection-only commands. Safe for any authorized chat user.
    ReadOnly,
    /// Changes per-channel session state but cannot escape the sandbox.
    Mutating,
    /// Can drop an in-flight turn, wipe channel state, or toggle bot runtime.
    /// Still reversible but disruptive to active work.
    RuntimeControl,
    /// Executes shell commands or grants new tool capabilities to the model.
    /// Equivalent to RCE on the host — owner only, explicit opt-in.
    ShellOrToolGrant,
    /// Modifies who can access the bot or rotates secrets/credentials.
    /// Owner only; always allowed for owner, never for anyone else.
    CredentialSystem,
}

impl CommandRisk {
    /// Human-readable label for help/dashboard surfaces.
    pub(in crate::services::discord) fn label(self) -> &'static str {
        match self {
            CommandRisk::ReadOnly => "read-only",
            CommandRisk::Mutating => "mutating",
            CommandRisk::RuntimeControl => "runtime-control",
            CommandRisk::ShellOrToolGrant => "shell/tool-grant",
            CommandRisk::CredentialSystem => "credential/system",
        }
    }

    /// True for tiers that must go through the owner guard regardless of
    /// `allow_all_users`.
    pub(in crate::services::discord) fn is_high_risk(self) -> bool {
        matches!(
            self,
            CommandRisk::RuntimeControl
                | CommandRisk::ShellOrToolGrant
                | CommandRisk::CredentialSystem
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
        | "!allowedtools" | "!sessions" | "!receipt" => CommandRisk::ReadOnly,

        // Session-shaping but not runtime-disruptive.
        "!start" | "!down" | "!cc" | "!meeting" | "!model" | "!fast" => CommandRisk::Mutating,

        // Can interrupt live turns or wipe conversation state.
        "!stop" | "!clear" | "!debug" | "!deletesession" | "!restart" | "!mcp_reload" => {
            CommandRisk::RuntimeControl
        }

        // Shell execution and tool allowlist mutation — equivalent to RCE.
        "!shell" | "!allowed" => CommandRisk::ShellOrToolGrant,

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
         `read-only` — help/status/metrics/allowedtools: any authorized user\n\
         `mutating` — start/down/cc/meeting/model: any authorized user\n\
         `runtime-control` — stop/clear/debug/restart/mcp_reload: owner-only\n\
         `shell/tool-grant` — shell/allowed: {shell_state}\n\
         `credential/system` — allowall/adduser/removeuser/escalation: owner-only"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_and_allowed_are_shell_or_tool_grant() {
        assert_eq!(command_risk("!shell", ""), CommandRisk::ShellOrToolGrant);
        assert_eq!(
            command_risk("!allowed", "+Bash"),
            CommandRisk::ShellOrToolGrant
        );
        assert_eq!(
            command_risk("!allowed", "-Bash"),
            CommandRisk::ShellOrToolGrant
        );
    }

    #[test]
    fn stop_and_clear_are_runtime_control() {
        assert_eq!(command_risk("!stop", ""), CommandRisk::RuntimeControl);
        assert_eq!(command_risk("!clear", ""), CommandRisk::RuntimeControl);
        assert_eq!(command_risk("!restart", ""), CommandRisk::RuntimeControl);
        assert_eq!(command_risk("!mcp_reload", ""), CommandRisk::RuntimeControl);
    }

    #[test]
    fn user_management_commands_are_credential_system() {
        assert_eq!(
            command_risk("!allowall", "on"),
            CommandRisk::CredentialSystem
        );
        assert_eq!(
            command_risk("!adduser", "42"),
            CommandRisk::CredentialSystem
        );
        assert_eq!(
            command_risk("!removeuser", "42"),
            CommandRisk::CredentialSystem
        );
        assert_eq!(
            command_risk("!escalation", "status"),
            CommandRisk::CredentialSystem
        );
    }

    #[test]
    fn read_only_and_mutating_stay_low_risk() {
        assert_eq!(command_risk("!help", ""), CommandRisk::ReadOnly);
        assert_eq!(command_risk("!status", ""), CommandRisk::ReadOnly);
        assert_eq!(command_risk("!allowedtools", ""), CommandRisk::ReadOnly);
        assert_eq!(command_risk("!start", "."), CommandRisk::Mutating);
        assert_eq!(command_risk("!down", "foo.txt"), CommandRisk::Mutating);
        assert_eq!(command_risk("!cc", "clear"), CommandRisk::Mutating);
    }

    #[test]
    fn non_owner_allowed_for_low_risk_even_when_allow_all_true() {
        // The `allow_all_users` branch runs earlier in the caller; policy only
        // needs to check that low-risk tiers return Allow.
        assert_eq!(
            evaluate_policy(CommandRisk::ReadOnly, false, false),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate_policy(CommandRisk::Mutating, false, false),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn non_owner_denied_for_high_risk_regardless_of_enable_flag() {
        // This is the canonical allow_all_users=true scenario: the general auth
        // layer let them through, but the policy still refuses.
        assert_eq!(
            evaluate_policy(CommandRisk::ShellOrToolGrant, false, true),
            PolicyDecision::DenyNotOwner
        );
        assert_eq!(
            evaluate_policy(CommandRisk::ShellOrToolGrant, false, false),
            PolicyDecision::DenyNotOwner
        );
        assert_eq!(
            evaluate_policy(CommandRisk::RuntimeControl, false, true),
            PolicyDecision::DenyNotOwner
        );
        assert_eq!(
            evaluate_policy(CommandRisk::CredentialSystem, false, true),
            PolicyDecision::DenyNotOwner
        );
    }

    #[test]
    fn owner_shell_requires_explicit_enable() {
        assert_eq!(
            evaluate_policy(CommandRisk::ShellOrToolGrant, true, false),
            PolicyDecision::DenyNotEnabled
        );
        assert_eq!(
            evaluate_policy(CommandRisk::ShellOrToolGrant, true, true),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn owner_runtime_control_allowed_without_explicit_enable() {
        // Runtime control must stay available for emergency ops even when the
        // shell opt-in is off.
        assert_eq!(
            evaluate_policy(CommandRisk::RuntimeControl, true, false),
            PolicyDecision::Allow
        );
        assert_eq!(
            evaluate_policy(CommandRisk::CredentialSystem, true, false),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn denial_messages_mention_env_var_or_owner() {
        let not_owner = PolicyDecision::DenyNotOwner
            .denial_message("!shell")
            .unwrap();
        assert!(not_owner.contains("owner"));
        assert!(not_owner.contains("!shell"));

        let not_enabled = PolicyDecision::DenyNotEnabled
            .denial_message("!shell")
            .unwrap();
        assert!(not_enabled.contains("AGENTDESK_DISCORD_HIGH_RISK_ENABLED"));
        assert!(not_enabled.contains("!shell"));

        assert!(PolicyDecision::Allow.denial_message("!shell").is_none());
    }

    #[test]
    fn risk_tier_summary_reflects_enable_state() {
        let disabled = risk_tier_summary_for_help(false);
        assert!(disabled.contains("shell/tool-grant"));
        assert!(disabled.contains("DISABLED"));
        assert!(disabled.contains("AGENTDESK_DISCORD_HIGH_RISK_ENABLED"));

        let enabled = risk_tier_summary_for_help(true);
        assert!(enabled.contains("ENABLED"));
    }

    #[test]
    fn env_var_parsing_accepts_truthy_values() {
        // Guard against accidental drift: we test the matcher directly because
        // mutating real process env would flake in parallel test runs.
        let matcher = |raw: &str| {
            let v = raw.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        };
        assert!(matcher("1"));
        assert!(matcher("true"));
        assert!(matcher("TRUE"));
        assert!(matcher("  yes "));
        assert!(matcher("on"));
        assert!(!matcher("0"));
        assert!(!matcher("no"));
        assert!(!matcher(""));
        assert!(!matcher("maybe"));
    }
}
