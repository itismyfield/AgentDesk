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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TextCommandSurface {
    Implemented,
    Unavailable,
}

/// Text command registration: name, dispatch ID and `!help` line. Unavailable
/// names have no help line.
pub(super) const TEXT_COMMANDS: &[(&str, TextCommandId, &str)] = {
    use TextCommandId::*;
    &[
        ("!help", Help, "`!help` — Show this help"),
        ("!pwd", Pwd, "`!pwd` — Show current working directory"),
        ("!health", Health, "`!health` — Show runtime health summary"),
        (
            "!status",
            Status,
            "`!status` — Show this channel session status",
        ),
        (
            "!inflight",
            Inflight,
            "`!inflight` — Show saved inflight turn state",
        ),
        ("!queue", Queue, "`!queue [all]` — Show pending queue"),
        ("!metrics", Metrics, "`!metrics [date]` — Show turn metrics"),
        (
            "!allowedtools",
            AllowedTools,
            "`!allowedtools` — Show currently allowed tools",
        ),
        ("!sessions", Sessions, ""),
        ("!receipt", Receipt, ""),
        ("!usage", Usage, ""),
        (
            "!start",
            Start,
            "`!start <path>` — Start session at directory",
        ),
        ("!down", Down, "`!down <file>` — Download file from server"),
        ("!cc", Cc, "`!cc <skill>` — Legacy alias for `!skill`"),
        ("!skill", Skill, "`!skill <skill>` — Run a provider skill"),
        (
            "!meeting",
            Meeting,
            "`!meeting [start] <agenda>|stop|status` — Run a round-table meeting",
        ),
        ("!model", Model, ""),
        ("!fast", Fast, ""),
        ("!goals", Goals, ""),
        ("!clear", Clear, "`!clear` — Clear AI conversation history"),
        ("!deletesession", DeleteSession, ""),
        ("!stop", Stop, "`!stop` — Stop current AI request"),
        ("!restart", Restart, ""),
        ("!debug", Debug, "`!debug` — Toggle debug logging"),
        (
            "!vc",
            Vc,
            "`!vc join|leave` — Join or leave your voice channel",
        ),
        (
            "!shell",
            Shell,
            "`!shell <command>` — Run shell command directly",
        ),
        (
            "!allowed",
            Allowed,
            "`!allowed +name|-name` — Add or remove a tool (e.g. `!allowed +Bash`)",
        ),
        (
            "!deadlock-recover",
            DeadlockRecover,
            "`!deadlock-recover [env]` — Restart the local dcserver and reap orphans",
        ),
        (
            "!machine-flip",
            MachineFlip,
            "`!machine-flip <peer>` — Redeploy locally, then deploy the peer",
        ),
        (
            "!stuck-pr-rebase",
            StuckPrRebase,
            "`!stuck-pr-rebase [label]` — Rebuild and push open PRs with the label",
        ),
        (
            "!allowall",
            AllowAll,
            "`!allowall on|off|status` — Allow everyone or restrict to authorized users",
        ),
        (
            "!adduser",
            AddUser,
            "`!adduser <user_id>` — Allow a user to use the bot",
        ),
        (
            "!removeuser",
            RemoveUser,
            "`!removeuser <user_id>` — Remove a user's access",
        ),
    ]
};

impl TextCommandId {
    pub(super) fn from_str(name: &str) -> Option<Self> {
        TEXT_COMMANDS
            .iter()
            .find(|(registered, ..)| *registered == name)
            .map(|(_, id, _)| *id)
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
            AllowAll | AddUser | RemoveUser => CommandRisk::CredentialSystem,
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
            | DeadlockRecover | MachineFlip | StuckPrRebase | AllowAll | AddUser | RemoveUser => {
                TextCommandSurface::Implemented
            }
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

/// Risk tier of every registered slash command, mirroring the text tiers; the
/// recovery commands run launchctl/ssh/git pipelines, so they are opt-in.
pub(super) const SLASH_COMMAND_RISKS: &[(&str, CommandRisk)] = {
    use CommandRisk::*;
    &[
        ("/help", ReadOnly),
        ("/pwd", ReadOnly),
        ("/health", ReadOnly),
        ("/status", ReadOnly),
        ("/inflight", ReadOnly),
        ("/queue", ReadOnly),
        ("/metrics", ReadOnly),
        ("/allowedtools", ReadOnly),
        ("/sessions", ReadOnly),
        ("/receipt", ReadOnly),
        ("/usage", ReadOnly),
        ("/adk", ReadOnly),
        ("/cost", ReadOnly),
        ("/context", ReadOnly),
        ("/system-prompt", ReadOnly),
        ("/start", Mutating),
        ("/resume", Mutating),
        ("/down", Mutating),
        ("/cc", Mutating),
        ("/skill", Mutating),
        ("/meeting", Mutating),
        ("/model", Mutating),
        ("/node", Mutating),
        ("/fast", Mutating),
        ("/tool-call", Mutating),
        ("/goals", Mutating),
        ("/effort", Mutating),
        ("/compact", Mutating),
        ("/clear", Mutating),
        ("/deletesession", Mutating),
        ("/stop", Mutating),
        ("/cancel-queued", Mutating),
        ("/restart", Mutating),
        ("/debug", Mutating),
        ("/sidecar", Mutating),
        ("/voice", Mutating),
        ("/vc_join", Mutating),
        ("/vc_leave", Mutating),
        ("/adk-phase", Mutating),
        ("/shell", ShellOrToolGrant),
        ("/allowed", ShellOrToolGrant),
        ("/deadlock-recover", ShellOrToolGrant),
        ("/machine-flip", ShellOrToolGrant),
        ("/stuck-pr-rebase", ShellOrToolGrant),
        ("/allowall", CredentialSystem),
        ("/adduser", CredentialSystem),
        ("/removeuser", CredentialSystem),
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

/// Short multi-line block for `!help` and `/help`, listing each tier's slash
/// commands from [`SLASH_COMMAND_RISKS`] and the high-risk enable state.
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
    let mut lines = vec![
        "**Command Risk Tiers** (a `!` text command shares the tier of its `/` name)".to_string(),
    ];
    for (risk, label, access) in tiers {
        let names = SLASH_COMMAND_RISKS
            .iter()
            .filter(|(_, tier)| *tier == risk)
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(format!("`{label}` ({access}): {names}"));
    }
    lines.join("\n")
}

/// `!help` body generated from [`TEXT_COMMANDS`], grouped by risk tier.
/// The handler still owns provider/env reads and message delivery.
pub(super) fn text_command_help(provider_name: &str, risk_block: &str) -> String {
    let mut lines = vec![
        "**AgentDesk Discord Bot**".to_string(),
        format!("Manage server files & chat with {provider_name}."),
        format!("Each channel gets its own independent {provider_name} session."),
        format!("Any other message is sent to {provider_name}; send a file/photo to upload it."),
        "Slash commands are listed by `/help`.".to_string(),
    ];
    let sections = [
        (CommandRisk::ReadOnly, "**Inspect**"),
        (CommandRisk::Mutating, "**Session**"),
        (
            CommandRisk::ShellOrToolGrant,
            "**Shell & Tools** (owner only, opt-in)",
        ),
        (
            CommandRisk::CredentialSystem,
            "**User Management** (owner only)",
        ),
    ];
    for (risk, title) in sections {
        lines.push(String::new());
        lines.push(title.to_string());
        for (_, id, help) in TEXT_COMMANDS {
            if id.risk() == risk && id.text_surface() == TextCommandSurface::Implemented {
                lines.push((*help).to_string());
            }
        }
    }
    lines.push(String::new());
    lines.push(risk_block.to_string());
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
                "!allowall !adduser !removeuser",
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
        assert_eq!(ids.len(), 33);
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
                // Voice uses separate slash names.
                if name == "!vc" {
                    continue;
                }
                assert_eq!(slash_command_risk(&format!("/{}", &name[1..])), risk);
                compared += 1;
            }
        }
        assert_eq!(compared, 32);
    }

    #[test]
    fn help_formatter_preserves_provider_and_risk_placeholders() {
        let help = text_command_help("fixture-provider", "fixture-risk");
        assert_eq!(help.matches("fixture-provider").count(), 3);
        assert!(help.ends_with("fixture-risk"));
    }

    /// Every implemented text command, and nothing else, has a `!help` line;
    /// unavailable names stay out of the help.
    #[test]
    fn text_registration_and_help_name_the_same_commands() {
        let help = text_command_help("fixture-provider", "");
        let listed: std::collections::BTreeSet<&str> = help
            .lines()
            .filter_map(|line| line.strip_prefix('`'))
            .filter(|rest| rest.starts_with('!'))
            .map(|rest| &rest[..rest.find([' ', '`']).unwrap_or(rest.len())])
            .collect();
        let mut implemented = std::collections::BTreeSet::new();
        for (name, id, line) in TEXT_COMMANDS {
            assert_eq!(TextCommandId::from_str(name), Some(*id), "{name}");
            let available = id.text_surface() == TextCommandSurface::Implemented;
            assert_eq!(!line.is_empty(), available, "{name} help line");
            if available {
                assert!(line.starts_with(&format!("`{name}")), "{name}: {line}");
                implemented.insert(*name);
            }
        }
        assert_eq!(listed, implemented);
        assert_eq!(implemented.len(), 25);
    }
}
