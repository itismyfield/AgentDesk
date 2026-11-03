//! Flags of `Commands` variants, kept in `Args` structs so clap builds each set in its own function.
//! Inlined, they share one debug stack frame that outgrows the 1 MiB Windows main-thread stack.

use clap::Args;

use super::{
    AgentHandoffChannelKindArg, DoctorProfileArg, FastModeStateArg, FeatureStateArg, InputModeArg,
    PhaseAction, QueryAction, ReportProvider,
};

#[derive(Args)]
pub(crate) struct DcserverArgs {
    /// Bot token (defaults to configured Discord bots or AGENTDESK_TOKEN env)
    pub(crate) token: Option<String>,
}

#[derive(Args)]
pub(crate) struct RestartDcserverArgs {
    /// Discord channel ID for restart completion report
    #[arg(long)]
    pub(crate) report_channel_id: Option<u64>,
    /// Provider for restart report (claude, codex, gemini, opencode, or qwen)
    #[arg(long, value_enum)]
    pub(crate) report_provider: Option<ReportProvider>,
    /// Existing message ID to edit for restart report
    #[arg(long)]
    pub(crate) report_message_id: Option<u64>,
}

#[derive(Args)]
pub(crate) struct DiscordSendfileArgs {
    /// File path to send
    pub(crate) path: String,
    /// Discord channel ID
    #[arg(long)]
    pub(crate) channel: u64,
    /// Authentication key hash
    #[arg(long)]
    pub(crate) key: String,
}

#[derive(Args)]
pub(crate) struct DiscordSendmessageArgs {
    /// Discord channel ID
    #[arg(long)]
    pub(crate) channel: u64,
    /// Message text
    #[arg(long)]
    pub(crate) message: String,
    /// Authentication key hash (optional; without it, configured bot tokens are tried sequentially until one succeeds)
    #[arg(long)]
    pub(crate) key: Option<String>,
}

#[derive(Args)]
pub(crate) struct DiscordSenddmArgs {
    /// Discord user ID
    #[arg(long)]
    pub(crate) user: u64,
    /// Message text
    #[arg(long)]
    pub(crate) message: String,
    /// Authentication key hash (optional; without it, configured bot tokens are tried sequentially until one succeeds)
    #[arg(long)]
    pub(crate) key: Option<String>,
}

#[derive(Args)]
pub(crate) struct SendArgs {
    #[arg(
        long,
        help = crate::services::discord::outbound::send_target::SEND_TARGET_CONTRACT
    )]
    pub(crate) target: String,
    /// Source label recorded in the send envelope
    #[arg(long)]
    pub(crate) source: Option<String>,
    /// Preferred Discord bot name
    #[arg(long)]
    pub(crate) bot: Option<String>,
    /// Message body
    #[arg(long)]
    pub(crate) content: String,
}

#[derive(Args)]
pub(crate) struct SendToAgentArgs {
    /// Source agent id recorded in the message envelope
    #[arg(long = "from")]
    pub(crate) from_agent_id: String,
    /// Target agent id whose Discord channel binding is used
    #[arg(long = "to")]
    pub(crate) to_agent_id: String,
    /// Message body
    #[arg(long)]
    pub(crate) message: String,
    /// Target channel binding kind
    #[arg(long = "channel-kind", value_enum, default_value = "cc")]
    pub(crate) channel_kind: AgentHandoffChannelKindArg,
    /// Send the body without the automatic handoff prefix
    #[arg(long = "no-prefix")]
    pub(crate) no_prefix: bool,
    /// Required: `true` → 회신 필수, `false` → 회신 불필요.
    #[arg(long = "expect-reply", action = clap::ArgAction::Set, required = true)]
    pub(crate) expect_reply: bool,
    /// #3556 — reserve a headless turn on the target mailbox instead of
    /// posting an announce message. Authoritative wake-up: exits non-zero
    /// on 409 (mailbox busy). Mutually exclusive with the default
    /// announce-post behavior.
    #[arg(long = "start-turn")]
    pub(crate) start_turn: bool,
}

#[derive(Args)]
pub(crate) struct ReviewVerdictArgs {
    /// Review dispatch ID
    #[arg(long = "dispatch")]
    pub(crate) dispatch_id: String,
    /// Verdict: pass, improve, rework, reject, approved
    #[arg(long = "verdict")]
    pub(crate) verdict: String,
    /// Optional verdict notes
    #[arg(long)]
    pub(crate) notes: Option<String>,
    /// Optional detailed feedback
    #[arg(long)]
    pub(crate) feedback: Option<String>,
    /// Reviewer provider id for counter-model reviews
    #[arg(long)]
    pub(crate) provider: Option<String>,
    /// Reviewed commit SHA
    #[arg(long)]
    pub(crate) commit: Option<String>,
}

#[derive(Args)]
pub(crate) struct ReviewDecisionArgs {
    /// Kanban card ID
    #[arg(long = "card")]
    pub(crate) card_id: String,
    /// Decision: approve, rework, escalate, accept, dispute, dismiss, requeue, resume
    #[arg(long = "decision")]
    pub(crate) decision: String,
    /// Optional decision comment
    #[arg(long)]
    pub(crate) comment: Option<String>,
    /// Optional dispatch scope for agent reply decisions
    #[arg(long = "dispatch")]
    pub(crate) dispatch_id: Option<String>,
}

#[derive(Args)]
pub(crate) struct ReviewRecoverTargetArgs {
    /// Review dispatch ID
    #[arg(long = "dispatch")]
    pub(crate) dispatch_id: Option<String>,
    /// Kanban card ID, used to resolve the latest review dispatch when --dispatch is omitted
    #[arg(long = "card")]
    pub(crate) card_id: Option<String>,
    /// Reviewed commit SHA to pin
    #[arg(long = "commit")]
    pub(crate) target_commit: Option<String>,
    /// Worktree path whose HEAD must match the reviewed commit
    #[arg(long = "worktree")]
    pub(crate) worktree_path: Option<String>,
    /// Operator reason stored in the audit event
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Args)]
pub(crate) struct DocsArgs {
    /// Optional docs category
    pub(crate) category: Option<String>,
    /// Emit flat endpoint list
    #[arg(long)]
    pub(crate) flat: bool,
}

#[derive(Args)]
pub(crate) struct ForceKillArgs {
    /// Session key
    #[arg(long = "session-key")]
    pub(crate) session_key: String,
    /// Requeue card after force-kill
    #[arg(long)]
    pub(crate) retry: bool,
}

#[derive(Args)]
pub(crate) struct GithubSyncArgs {
    /// Repository in owner/repo form. Omit to sync all registered sync-enabled repos.
    #[arg(long)]
    pub(crate) repo: Option<String>,
}

#[derive(Args)]
pub(crate) struct CherryMergeArgs {
    /// Source branch to merge into main
    pub(crate) branch: String,
    /// Close the linked GitHub issue when it can be inferred uniquely
    #[arg(long)]
    pub(crate) close_issue: bool,
}

#[derive(Args)]
pub(crate) struct TmuxWrapperArgs {
    /// Path to the output capture file
    #[arg(long)]
    pub(crate) output_file: String,
    /// Path to the input FIFO
    #[arg(long)]
    pub(crate) input_fifo: String,
    /// Path to the prompt file
    #[arg(long)]
    pub(crate) prompt_file: String,
    /// Working directory (defaults to ".")
    #[arg(long, default_value = ".")]
    pub(crate) cwd: String,
    /// Input mode: fifo on Unix or pipe on all platforms
    #[arg(long, value_enum, default_value_t = InputModeArg::default())]
    pub(crate) input_mode: InputModeArg,
    /// Claude command and arguments (after --)
    #[arg(last = true)]
    pub(crate) claude_cmd: Vec<String>,
}

#[derive(Args)]
pub(crate) struct CodexTmuxWrapperArgs {
    /// Path to the output capture file
    #[arg(long)]
    pub(crate) output_file: String,
    /// Path to the input FIFO
    #[arg(long)]
    pub(crate) input_fifo: String,
    /// Path to the prompt file
    #[arg(long)]
    pub(crate) prompt_file: String,
    /// Path to codex binary
    #[arg(long)]
    pub(crate) codex_bin: String,
    /// Optional codex model override
    #[arg(long)]
    pub(crate) codex_model: Option<String>,
    /// Optional reasoning effort (low, normal, high, xhigh)
    #[arg(long)]
    pub(crate) reasoning_effort: Option<String>,
    /// Optional developer instructions passed through Codex config
    #[arg(long)]
    pub(crate) developer_instructions: Option<String>,
    /// Optional resume session id for the first turn
    #[arg(long)]
    pub(crate) resume_session_id: Option<String>,
    /// Override Codex native fast mode for every turn in this wrapper session
    #[arg(long, value_enum)]
    pub(crate) fast_mode_state: Option<FastModeStateArg>,
    /// Override Codex goals feature flag for every turn in this wrapper session
    #[arg(long, value_enum)]
    pub(crate) goals_state: Option<FeatureStateArg>,
    /// Working directory (defaults to ".")
    #[arg(long, default_value = ".")]
    pub(crate) cwd: String,
    /// Additional directory writable alongside the primary workspace
    #[arg(long = "add-dir")]
    pub(crate) add_dirs: Vec<String>,
    /// Input mode: fifo on Unix or pipe on all platforms
    #[arg(long, value_enum, default_value_t = InputModeArg::default())]
    pub(crate) input_mode: InputModeArg,
    /// Auto-compact token limit (absolute token count)
    #[arg(long)]
    pub(crate) compact_token_limit: Option<u64>,
}

#[derive(Args)]
pub(crate) struct QwenTmuxWrapperArgs {
    /// Path to the output capture file
    #[arg(long)]
    pub(crate) output_file: String,
    /// Path to the input FIFO
    #[arg(long)]
    pub(crate) input_fifo: String,
    /// Path to the prompt file
    #[arg(long)]
    pub(crate) prompt_file: String,
    /// Path to qwen binary
    #[arg(long)]
    pub(crate) qwen_bin: String,
    /// Optional qwen model override
    #[arg(long)]
    pub(crate) qwen_model: Option<String>,
    /// Qwen built-in core tool allowlist entry
    #[arg(long = "qwen-core-tool")]
    pub(crate) qwen_core_tools: Vec<String>,
    /// Optional resume session id for the first turn
    #[arg(long)]
    pub(crate) resume_session_id: Option<String>,
    /// Force the first turn to start without reusing provider session state
    #[arg(long)]
    pub(crate) fresh_session: bool,
    /// Working directory (defaults to ".")
    #[arg(long, default_value = ".")]
    pub(crate) cwd: String,
    /// Input mode: fifo on Unix or pipe on all platforms
    #[arg(long, value_enum, default_value_t = InputModeArg::default())]
    pub(crate) input_mode: InputModeArg,
}

#[derive(Args)]
pub(crate) struct ClaudeHookRelayArgs {
    /// Hook receiver base endpoint, e.g. http://127.0.0.1:49152
    #[arg(long)]
    pub(crate) endpoint: String,
    /// Provider id, e.g. claude
    #[arg(long)]
    pub(crate) provider: String,
    /// Claude hook event name, e.g. Stop or PreToolUse
    #[arg(long)]
    pub(crate) event: String,
    /// TUI session id assigned by AgentDesk
    #[arg(long = "session-id")]
    pub(crate) session_id: String,
}

#[derive(Args)]
pub(crate) struct CodexHookRelayArgs {
    /// Hook receiver base endpoint, e.g. http://127.0.0.1:49152
    #[arg(long)]
    pub(crate) endpoint: String,
    /// Provider id, e.g. codex
    #[arg(long)]
    pub(crate) provider: String,
    /// Codex hook event name, e.g. Stop or PreToolUse
    #[arg(long)]
    pub(crate) event: String,
    /// TUI session id assigned by AgentDesk
    #[arg(long = "session-id")]
    pub(crate) session_id: String,
}

#[derive(Args)]
pub(crate) struct IsmcptoolArgs {
    /// Tool names to check
    #[arg(required = true)]
    pub(crate) tools: Vec<String>,
}

#[derive(Args)]
pub(crate) struct AddmcptoolArgs {
    /// Tool names to add
    #[arg(required = true)]
    pub(crate) tools: Vec<String>,
}

#[derive(Args)]
pub(crate) struct CardsArgs {
    /// Filter by status (e.g. ready, in_progress, done)
    #[arg(long)]
    pub(crate) status: Option<String>,
}

#[derive(Args)]
pub(crate) struct ResumeArgs {
    /// Card ID or GitHub issue number
    pub(crate) card_id: String,
    /// Force resume (bypass guards for manual-intervention states)
    #[arg(long)]
    pub(crate) force: bool,
    /// Reason for audit log
    #[arg(long)]
    pub(crate) reason: Option<String>,
}

#[derive(Args)]
pub(crate) struct AdvanceArgs {
    /// GitHub issue number
    pub(crate) issue_number: String,
}

#[derive(Args)]
pub(crate) struct QueryArgs {
    #[command(subcommand)]
    pub(crate) action: Option<QueryAction>,
    /// Filter result rows. Repeatable. Format: `key:value`
    /// (e.g. `--filter status:pending`, `--filter dispatch_type:review`).
    #[arg(long = "filter", global = true)]
    pub(crate) filters: Vec<String>,
    /// Restrict to a specific agent_id when applicable.
    #[arg(long, global = true)]
    pub(crate) agent: Option<String>,
    /// Maximum rows to render per section (0 = unlimited).
    #[arg(long, global = true, default_value_t = 0)]
    pub(crate) limit: usize,
}

#[derive(Args)]
pub(crate) struct PhaseArgs {
    #[command(subcommand)]
    pub(crate) action: Option<PhaseAction>,
    /// Show full per-entry detail.
    #[arg(long, global = true)]
    pub(crate) detailed: bool,
}

#[derive(Args)]
pub(crate) struct DiagArgs {
    /// Agent ID or Discord channel ID
    pub(crate) identifier: String,
}

#[derive(Args)]
pub(crate) struct ApiArgs {
    /// HTTP method (GET, POST, PATCH, PUT, DELETE)
    pub(crate) method: String,
    /// API path (e.g. /api/health)
    pub(crate) path: String,
    /// Optional JSON body
    pub(crate) body: Option<String>,
}

#[derive(Args)]
pub(crate) struct TerminationsArgs {
    /// Filter by kanban card ID
    #[arg(long)]
    pub(crate) card_id: Option<String>,
    /// Filter by dispatch ID
    #[arg(long)]
    pub(crate) dispatch_id: Option<String>,
    /// Filter by session key
    #[arg(long)]
    pub(crate) session: Option<String>,
    /// Max number of events to show
    #[arg(long, default_value = "50")]
    pub(crate) limit: u32,
}

#[derive(Args)]
pub(crate) struct DoctorArgs {
    /// Apply safe local repairs before running diagnostics
    #[arg(long)]
    pub(crate) fix: bool,
    /// Allow doctor --fix to restart the dcserver service
    #[arg(long)]
    pub(crate) allow_restart: bool,
    /// Allow doctor --fix to run SQLite schema repair and remove stale SQLite cache files
    #[arg(long)]
    pub(crate) repair_sqlite_cache: bool,
    /// Allow sending configured auth tokens to a non-loopback AGENTDESK_API_URL
    #[arg(long)]
    pub(crate) allow_remote: bool,
    /// Restrict checks to a profile
    #[arg(long, value_enum)]
    pub(crate) profile: Option<DoctorProfileArg>,
}

#[derive(Args)]
pub(crate) struct ActivityArgs {
    /// Window start. Accepts RFC3339 (`2026-05-19T23:10:00+09:00`),
    /// duration suffix (`24h`, `90m`, `7d`), or `since:<sha>` to anchor
    /// on a commit (commit time becomes the start).
    #[arg(long)]
    pub(crate) since: String,
    /// Optional window end (RFC3339). Defaults to `now` when omitted.
    #[arg(long)]
    pub(crate) until: Option<String>,
    /// Repository in owner/repo form. Defaults to the current
    /// repo's `origin` remote when omitted.
    #[arg(long)]
    pub(crate) repo: Option<String>,
    /// Skip the AgentDesk-side deploy / incident lookup (useful when
    /// the local API is offline). Pure git + gh report.
    #[arg(long = "no-agentdesk")]
    pub(crate) no_agentdesk: bool,
}
