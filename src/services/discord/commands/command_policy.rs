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
    TextCommandId::from_str(cmd).map_or(CommandRisk::Mutating, TextCommandId::risk)
}

/// Known text names, including policy-only names whose text surface is unavailable.
/// The actual dispatcher exhaustively matches these IDs, never arbitrary text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextCommandId {
    Help,
    Pwd,
    Health,
    Status,
    Inflight,
    Queue,
    Metrics,
    AllowedTools,
    Sessions,
    Receipt,
    Usage,
    Start,
    Down,
    Cc,
    Skill,
    Meeting,
    Model,
    Fast,
    Goals,
    Clear,
    DeleteSession,
    Stop,
    Restart,
    Debug,
    Vc,
    Shell,
    Allowed,
    DeadlockRecover,
    MachineFlip,
    StuckPrRebase,
    AllowAll,
    AddUser,
    RemoveUser,
    Escalation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextCommandSurface {
    Implemented,
    Unavailable,
}

impl TextCommandId {
    pub(super) fn from_str(name: &str) -> Option<Self> {
        Some(match name {
            "!help" => Self::Help,
            "!pwd" => Self::Pwd,
            "!health" => Self::Health,
            "!status" => Self::Status,
            "!inflight" => Self::Inflight,
            "!queue" => Self::Queue,
            "!metrics" => Self::Metrics,
            "!allowedtools" => Self::AllowedTools,
            "!sessions" => Self::Sessions,
            "!receipt" => Self::Receipt,
            "!usage" => Self::Usage,
            "!start" => Self::Start,
            "!down" => Self::Down,
            "!cc" => Self::Cc,
            "!skill" => Self::Skill,
            "!meeting" => Self::Meeting,
            "!model" => Self::Model,
            "!fast" => Self::Fast,
            "!goals" => Self::Goals,
            "!clear" => Self::Clear,
            "!deletesession" => Self::DeleteSession,
            "!stop" => Self::Stop,
            "!restart" => Self::Restart,
            "!debug" => Self::Debug,
            "!vc" => Self::Vc,
            "!shell" => Self::Shell,
            "!allowed" => Self::Allowed,
            "!deadlock-recover" => Self::DeadlockRecover,
            "!machine-flip" => Self::MachineFlip,
            "!stuck-pr-rebase" => Self::StuckPrRebase,
            "!allowall" => Self::AllowAll,
            "!adduser" => Self::AddUser,
            "!removeuser" => Self::RemoveUser,
            "!escalation" => Self::Escalation,
            _ => return None,
        })
    }

    pub(super) fn risk(self) -> CommandRisk {
        use TextCommandId::*;
        match self {
            Help | Pwd | Health | Status | Inflight | Queue | Metrics | AllowedTools | Sessions
            | Receipt | Usage => CommandRisk::ReadOnly,
            Start | Down | Cc | Skill | Meeting | Model | Fast | Goals | Clear | DeleteSession
            | Stop | Restart | Debug | Vc => CommandRisk::Mutating,
            Shell | Allowed | DeadlockRecover | MachineFlip | StuckPrRebase => {
                CommandRisk::ShellOrToolGrant
            }
            AllowAll | AddUser | RemoveUser | Escalation => CommandRisk::CredentialSystem,
        }
    }

    pub(super) fn text_surface(self) -> TextCommandSurface {
        use TextCommandId::*;
        match self {
            Sessions | Receipt | Usage | Model | Fast | Goals | DeleteSession | Restart => {
                TextCommandSurface::Unavailable
            }
            Help | Pwd | Health | Status | Inflight | Queue | Metrics | AllowedTools | Start
            | Down | Cc | Skill | Meeting | Clear | Stop | Debug | Vc | Shell | Allowed
            | DeadlockRecover | MachineFlip | StuckPrRebase | AllowAll | AddUser | RemoveUser
            | Escalation => TextCommandSurface::Implemented,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextCommandSelection {
    Dispatch(TextCommandId),
    Unknown,
    Unavailable,
    PolicyDenied(PolicyDecision),
}

impl TextCommandSelection {
    pub(super) fn denial_message(self, cmd: &str) -> Option<String> {
        match self {
            Self::Dispatch(_) => None,
            Self::Unknown | Self::Unavailable => Some(
                "Unknown or unavailable text command. Use `!help` for supported commands.".into(),
            ),
            Self::PolicyDenied(decision) => decision.denial_message(cmd),
        }
    }
}

/// Pure production selection: availability precedes authorization. No I/O or env reads.
pub(super) fn select_text_command(
    classified: Option<TextCommandId>,
    is_owner: bool,
    high_risk_enabled: bool,
) -> TextCommandSelection {
    let Some(id) = classified else {
        return TextCommandSelection::Unknown;
    };
    if id.text_surface() == TextCommandSurface::Unavailable {
        return TextCommandSelection::Unavailable;
    }
    match evaluate_policy(id.risk(), is_owner, high_risk_enabled) {
        PolicyDecision::Allow => TextCommandSelection::Dispatch(id),
        denial => TextCommandSelection::PolicyDenied(denial),
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
        "/start" | "/resume" | "/down" | "/cc" | "/skill" | "/meeting" | "/model" | "/node"
        | "/fast" | "/goals" | "/effort" | "/compact" | "/clear" | "/deletesession" | "/stop"
        | "/cancel-queued" | "/restart" | "/debug" => CommandRisk::Mutating,

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

/// Pure help formatting; the handler still owns provider/env reads and message delivery.
pub(super) fn text_command_help(
    provider_name: &str,
    claude_tui_settings: &str,
    risk_block: &str,
) -> String {
    format!(
        "\
**AgentDesk Discord Bot**
Manage server files & chat with {p}.
Each channel gets its own independent {p} session.

**Session**
`!start <path>` — Start session at directory
`!pwd` — Show current working directory
`!health` — Show runtime health summary
`!status` — Show this channel session status
`!inflight` — Show saved inflight turn state
`!clear` — Clear AI conversation history
`!stop` — Stop current AI request

**File Transfer**
`!down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!shell <command>` — Run shell command directly

**AI Chat**
Any other message is sent to {p}.

**Tool Management**
`!allowedtools` — Show currently allowed tools
`!allowed +name` — Add tool (e.g. `!allowed +Bash`)
`!allowed -name` — Remove tool

**Skills**
`!skill <skill>` — Run a provider skill
`!cc <skill>` — Legacy alias for `!skill`

**Settings**
`/model` — Open the interactive model picker
{claude_tui_settings}
`!debug` — Toggle debug logging
`!metrics [date]` — Show turn metrics
`!queue [all]` — Show pending queue
`!escalation status` — Show escalation routing mode

**User Management** (owner only)
`!allowall on|off|status` — Allow everyone or restrict to authorized users
`!adduser <user_id>` — Allow a user to use the bot
`!removeuser <user_id>` — Remove a user's access
`!escalation pm|user|scheduled` — Change escalation routing mode
`!escalation schedule <HH:MM-HH:MM>` — Set PM hours and switch to scheduled mode
`!escalation timezone <IANA>` — Set scheduled timezone
`!escalation owner <user_id>` — Override fallback owner user id
`!escalation pm-channel <channel_id>` — Override PM channel
`!help` — Show this help

{risk_block}",
        p = provider_name,
    )
}

#[cfg(test)]
mod command_policy_tests {
    use super::*;

    #[test]
    fn cancel_queued_is_registered_as_mutating_and_not_high_risk() {
        assert_eq!(slash_command_risk("/cancel-queued"), CommandRisk::Mutating);
        assert!(!slash_command_risk("/cancel-queued").is_high_risk());
    }

    fn registered_cases() -> [(CommandRisk, TextCommandSurface, &'static str); 6] {
        use CommandRisk::*;
        use TextCommandSurface::*;
        [
            (
                ReadOnly,
                Implemented,
                "!help !pwd !health !status !inflight !queue !metrics !allowedtools",
            ),
            (ReadOnly, Unavailable, "!sessions !receipt !usage"),
            (
                Mutating,
                Implemented,
                "!start !down !cc !skill !meeting !clear !stop !debug !vc",
            ),
            (
                Mutating,
                Unavailable,
                "!model !fast !goals !deletesession !restart",
            ),
            (
                ShellOrToolGrant,
                Implemented,
                "!shell !allowed !deadlock-recover !machine-flip !stuck-pr-rebase",
            ),
            (
                CredentialSystem,
                Implemented,
                "!allowall !adduser !removeuser !escalation",
            ),
        ]
    }

    #[test]
    fn registered_surface_and_principal_matrix_preserves_exact_ids() {
        use CommandRisk::*;
        use TextCommandSelection::*;
        let mut ids = Vec::new();
        let mut unavailable = 0;
        for (risk, surface, names) in registered_cases() {
            for name in names.split_whitespace() {
                let id = TextCommandId::from_str(name).expect("registered text name");
                assert!(!ids.contains(&id), "each name has its own dispatch ID");
                ids.push(id);
                assert_eq!(id.risk(), risk, "{name}");
                assert_eq!(command_risk(name, ""), risk);
                assert_eq!(id.text_surface(), surface, "{name}");
                unavailable += usize::from(surface == TextCommandSurface::Unavailable);
                for owner in [false, true] {
                    for enabled in [false, true] {
                        let expected = if surface == TextCommandSurface::Unavailable {
                            Unavailable
                        } else {
                            match (risk, owner, enabled) {
                                (ShellOrToolGrant | CredentialSystem, false, _) => {
                                    PolicyDenied(PolicyDecision::DenyNotOwner)
                                }
                                (ShellOrToolGrant, true, false) => {
                                    PolicyDenied(PolicyDecision::DenyNotEnabled)
                                }
                                _ => Dispatch(id),
                            }
                        };
                        assert_eq!(select_text_command(Some(id), owner, enabled), expected);
                    }
                }
            }
        }
        assert_eq!(ids.len(), 34);
        assert_eq!(unavailable, 8);
    }

    #[test]
    fn unknown_selection_is_denied_for_every_principal() {
        for name in ["", "neutral text", "!not-registered"] {
            assert_eq!(TextCommandId::from_str(name), None);
            for owner in [false, true] {
                for enabled in [false, true] {
                    let result = select_text_command(None, owner, enabled);
                    assert_eq!(result, TextCommandSelection::Unknown);
                    assert!(result.denial_message(name).is_some());
                }
            }
        }
    }

    #[test]
    fn common_registered_slash_surfaces_keep_the_same_risk() {
        let mut compared = 0;
        for (risk, _, names) in registered_cases() {
            for name in names.split_whitespace() {
                // Voice uses separate slash names; escalation has no slash registration.
                if matches!(name, "!vc" | "!escalation") {
                    continue;
                }
                assert_eq!(slash_command_risk(&format!("/{}", &name[1..])), risk);
                compared += 1;
            }
        }
        assert_eq!(compared, 32);
    }

    #[test]
    fn help_formatter_preserves_provider_settings_and_risk_placeholders() {
        let help = text_command_help("fixture-provider", "fixture-settings", "fixture-risk");
        assert_eq!(help.matches("fixture-provider").count(), 3);
        assert_eq!(help.matches("fixture-settings").count(), 1);
        assert!(help.ends_with("fixture-risk"));
    }
}
