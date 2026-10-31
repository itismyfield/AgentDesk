//! Discord command risk tiers and the agent text commands.
//!
//! High-risk tiers need the bot owner even with `allow_all_users=true`, plus an opt-in for shells.
//! `match_text_command` reads `!clear`, `!restart`, `!stop` and the removed text names.

/// Coarse risk tier for a Discord command.
///
/// Ordering (low → high) reflects the amount of trust required:
/// `ReadOnly < Mutating < ShellOrToolGrant < CredentialSystem`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum CommandRisk {
    /// Inspection-only commands. Safe for any authorized chat user.
    ReadOnly,
    /// Changes per-channel session state but cannot escape the sandbox.
    /// Includes session resets (`clear`), in-flight turn cancellation
    /// (`stop`), and channel-scoped tmux respawn (`restart`).
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

/// Session controls kept as text because agents cannot run slash commands; each
/// runs the same handler as its slash twin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TextCommand {
    Clear,
    Restart,
    Stop,
}

/// Agent text commands with their `/help` line.
pub(super) const TEXT_COMMANDS: [(&str, TextCommand, &str); 3] = [
    (
        "!clear",
        TextCommand::Clear,
        "`!clear` — Clear AI conversation history",
    ),
    (
        "!restart",
        TextCommand::Restart,
        "`!restart` — Restart the provider session",
    ),
    (
        "!stop",
        TextCommand::Stop,
        "`!stop` — Stop current AI request",
    ),
];

/// Removed text command names, each with the registered slash command that replaces it.
const RETIRED_TEXT_COMMANDS: [(&str, Option<&str>); 30] = [
    ("!help", Some("/help")),
    ("!pwd", Some("/pwd")),
    ("!health", Some("/health")),
    ("!status", Some("/status")),
    ("!inflight", Some("/inflight")),
    ("!queue", Some("/queue")),
    ("!usage", Some("/usage")),
    ("!receipt", Some("/usage")),
    ("!skill", Some("/skill")),
    ("!cc", Some("/skill")),
    ("!model", Some("/model")),
    ("!fast", Some("/fast")),
    ("!goals", Some("/goals")),
    ("!allowall", Some("/allowall")),
    ("!vc", None),
    ("!metrics", None),
    ("!allowedtools", None),
    ("!sessions", None),
    ("!start", None),
    ("!down", None),
    ("!meeting", None),
    ("!deletesession", None),
    ("!debug", None),
    ("!shell", None),
    ("!allowed", None),
    ("!deadlock-recover", None),
    ("!machine-flip", None),
    ("!stuck-pr-rebase", None),
    ("!adduser", None),
    ("!removeuser", None),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TextCommandMatch {
    Kept(TextCommand),
    /// A removed name and its replacement slash command, if any.
    Retired(&'static str, Option<&'static str>),
    /// The router's owner-only `!herdr consent retry` and `!herdr consent return`, in the
    /// exact forms it accepts.
    HerdrConsent,
}

/// Reads a message, leading bot mention already stripped, as a text command by its
/// first word; anything else is ordinary chat.
pub(in crate::services::discord) fn match_text_command(text: &str) -> Option<TextCommandMatch> {
    let mut words = text.split_whitespace();
    let first = words.next()?;
    if let Some((_, command, _)) = TEXT_COMMANDS.iter().find(|(name, ..)| *name == first) {
        return Some(TextCommandMatch::Kept(*command));
    }
    if let Some((name, replacement)) = RETIRED_TEXT_COMMANDS
        .iter()
        .find(|(name, _)| *name == first)
    {
        return Some(TextCommandMatch::Retired(name, *replacement));
    }
    let rest: Vec<&str> = words.collect();
    (first == "!herdr" && matches!(rest.as_slice(), ["consent", "retry" | "return"]))
        .then_some(TextCommandMatch::HerdrConsent)
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

/// Risk tier of every registered slash command.
pub(super) const SLASH_COMMAND_RISKS: &[(&str, CommandRisk)] = {
    use CommandRisk::*;
    &[
        ("/help", ReadOnly),
        ("/pwd", ReadOnly),
        ("/health", ReadOnly),
        ("/status", ReadOnly),
        ("/inflight", ReadOnly),
        ("/queue", ReadOnly),
        ("/usage", ReadOnly),
        ("/adk", ReadOnly),
        ("/system-prompt", ReadOnly),
        ("/resume", Mutating),
        ("/skill", Mutating),
        ("/model", Mutating),
        ("/node", Mutating),
        ("/fast", Mutating),
        ("/tool-call", Mutating),
        ("/goals", Mutating),
        ("/effort", Mutating),
        ("/compact", Mutating),
        ("/clear", Mutating),
        ("/stop", Mutating),
        ("/cancel-queued", Mutating),
        ("/restart", Mutating),
        ("/sidecar", Mutating),
        ("/allowall", CredentialSystem),
    ]
};

/// Look up the risk tier for a slash command (e.g. `/shell`, `/clear`).
/// Names outside [`SLASH_COMMAND_RISKS`] get the conservative `Mutating` tier.
pub(in crate::services::discord) fn slash_command_risk(slash_cmd: &str) -> CommandRisk {
    SLASH_COMMAND_RISKS
        .iter()
        .find(|(name, _)| *name == slash_cmd)
        .map_or(CommandRisk::Mutating, |(_, risk)| *risk)
}

/// Short multi-line block for `/help`, listing the slash commands of
/// each tier in [`SLASH_COMMAND_RISKS`]; tiers without one are left out.
pub(in crate::services::discord) fn risk_tier_summary_for_help(high_risk_enabled: bool) -> String {
    let shell_state = if high_risk_enabled {
        "owner-only, ENABLED"
    } else {
        "owner-only, DISABLED (set AGENTDESK_DISCORD_HIGH_RISK_ENABLED=1)"
    };
    let tiers = [
        (CommandRisk::ReadOnly, "read-only", "any authorized user"),
        (CommandRisk::Mutating, "mutating", "any authorized user"),
        (
            CommandRisk::ShellOrToolGrant,
            "shell/tool-grant",
            shell_state,
        ),
        (
            CommandRisk::CredentialSystem,
            "credential/system",
            "owner-only",
        ),
    ];
    let mut lines = vec!["**Command Risk Tiers** (slash commands)".to_string()];
    for (risk, label, access) in tiers {
        let names = SLASH_COMMAND_RISKS
            .iter()
            .filter(|(_, tier)| *tier == risk)
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(" ");
        if names.is_empty() {
            continue;
        }
        lines.push(format!("`{label}` ({access}): {names}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod command_policy_tests {
    use super::*;

    #[test]
    fn cancel_queued_is_registered_as_mutating_and_not_high_risk() {
        assert_eq!(slash_command_risk("/cancel-queued"), CommandRisk::Mutating);
        assert!(!slash_command_risk("/cancel-queued").is_high_risk());
    }

    /// A removed text name points only at a slash command the bot still registers.
    #[test]
    fn retired_text_names_point_at_registered_slash_commands() {
        let registered: std::collections::BTreeSet<String> =
            crate::services::discord::runtime_bootstrap::run_bot_build_slash_commands()
                .iter()
                .map(|command| format!("/{}", command.name))
                .collect();
        for (name, replacement) in RETIRED_TEXT_COMMANDS {
            if let Some(slash) = replacement {
                assert!(registered.contains(slash), "{name} -> {slash}");
            }
        }
    }

    /// The first word decides: an exact kept or removed name is a command, and
    /// any other `!` text, near misses included, is chat.
    #[test]
    fn only_exact_command_words_are_text_commands() {
        use TextCommandMatch::*;
        let cases = [
            ("!clear", Some(Kept(TextCommand::Clear))),
            ("!restart now", Some(Kept(TextCommand::Restart))),
            ("!stop\nplease", Some(Kept(TextCommand::Stop))),
            ("!pwd", Some(Retired("!pwd", Some("/pwd")))),
            ("!shell ls", Some(Retired("!shell", None))),
            ("!herdr consent retry", Some(HerdrConsent)),
            ("!herdr consent retry now", None),
            ("!herdr consent return", Some(HerdrConsent)),
            ("!herdr consent return now", None),
            ("!herdr", None),
            ("!clearall", None),
            ("!Clear", None),
            ("! clear", None),
            ("!", None),
            ("!안녕", None),
            ("hello !clear", None),
            ("", None),
        ];
        for (text, expected) in cases {
            assert_eq!(match_text_command(text), expected, "{text:?}");
        }
        // The live intake routes by this test; a consent return read as chat would reach the
        // provider instead of the return hook.
        assert!(
            crate::services::discord::catch_up::handled_command::is_text_command(
                "<@1> !herdr consent return"
            )
        );
    }
}
