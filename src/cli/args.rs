use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "agentdesk", version = env!("CARGO_PKG_VERSION"), about = "AI agent orchestration platform")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    /// Start Discord bot server(s)
    Dcserver {
        /// Bot token (defaults to configured Discord bots or AGENTDESK_TOKEN env)
        token: Option<String>,
    },
    /// Run the initial setup wizard
    Init,
    /// Re-run the configuration wizard
    Reconfigure,
    /// Restart Discord bot server(s)
    RestartDcserver {
        /// Discord channel ID for restart completion report
        #[arg(long)]
        report_channel_id: Option<u64>,
        /// Provider for restart report (claude, codex, gemini, or qwen)
        #[arg(long, value_enum)]
        report_provider: Option<ReportProvider>,
        /// Existing message ID to edit for restart report
        #[arg(long)]
        report_message_id: Option<u64>,
    },
    /// Send a file to a Discord channel
    DiscordSendfile {
        /// File path to send
        path: String,
        /// Discord channel ID
        #[arg(long)]
        channel: u64,
        /// Authentication key hash
        #[arg(long)]
        key: String,
    },
    /// Send a message to a Discord channel
    DiscordSendmessage {
        /// Discord channel ID
        #[arg(long)]
        channel: u64,
        /// Message text
        #[arg(long)]
        message: String,
        /// Authentication key hash (optional; falls back to AGENTDESK_TOKEN env or configured Discord bots)
        #[arg(long)]
        key: Option<String>,
    },
    /// Send a direct message to a Discord user
    DiscordSenddm {
        /// Discord user ID
        #[arg(long)]
        user: u64,
        /// Message text
        #[arg(long)]
        message: String,
        /// Authentication key hash (optional; falls back to AGENTDESK_TOKEN env or configured Discord bots)
        #[arg(long)]
        key: Option<String>,
    },
    /// Send a routed Discord message without HTTP server dependency
    Send {
        /// Target route such as channel:<id>, user:<id>, role:<name>
        #[arg(long)]
        target: String,
        /// Source label recorded in the send envelope
        #[arg(long)]
        source: Option<String>,
        /// Preferred Discord bot name
        #[arg(long)]
        bot: Option<String>,
        /// Message body
        #[arg(long)]
        content: String,
    },
    /// Submit a review verdict without HTTP server dependency
    ReviewVerdict {
        /// Review dispatch ID
        #[arg(long = "dispatch")]
        dispatch_id: String,
        /// Verdict: pass, improve, rework, reject, approved
        #[arg(long = "verdict")]
        verdict: String,
        /// Optional verdict notes
        #[arg(long)]
        notes: Option<String>,
        /// Optional detailed feedback
        #[arg(long)]
        feedback: Option<String>,
        /// Reviewer provider id for counter-model reviews
        #[arg(long)]
        provider: Option<String>,
        /// Reviewed commit SHA
        #[arg(long)]
        commit: Option<String>,
    },
    /// Submit a review decision without HTTP server dependency
    ReviewDecision {
        /// Kanban card ID
        #[arg(long = "card")]
        card_id: String,
        /// Decision: approve, rework, escalate, accept, dispute, dismiss, requeue, resume
        #[arg(long = "decision")]
        decision: String,
        /// Optional decision comment
        #[arg(long)]
        comment: Option<String>,
        /// Optional dispatch scope for agent reply decisions
        #[arg(long = "dispatch")]
        dispatch_id: Option<String>,
    },
    /// Read API documentation without HTTP server dependency
    Docs {
        /// Optional docs category
        category: Option<String>,
        /// Emit flat endpoint list
        #[arg(long)]
        flat: bool,
    },
    /// Auto-queue operations without HTTP server dependency
    AutoQueue {
        #[command(subcommand)]
        action: AutoQueueAction,
    },
    /// Force-kill a session without HTTP server dependency
    ForceKill {
        /// Session key
        #[arg(long = "session-key")]
        session_key: String,
        /// Requeue card after force-kill
        #[arg(long)]
        retry: bool,
    },
    /// Sync GitHub issues without HTTP server dependency
    GithubSync {
        /// Repository in owner/repo form. Omit to sync all registered sync-enabled repos.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Discord utility commands without HTTP server dependency
    Discord {
        #[command(subcommand)]
        action: DiscordAction,
    },
    /// Kanban utility commands without HTTP server dependency
    Card {
        #[command(subcommand)]
        action: CardAction,
    },
    /// Cherry-pick a worktree branch into main and optionally close its issue
    CherryMerge {
        /// Source branch to merge into main
        branch: String,
        /// Close the linked GitHub issue when it can be inferred uniquely
        #[arg(long)]
        close_issue: bool,
    },
    /// tmux + Claude CLI integration wrapper (Unix only)
    #[cfg(unix)]
    TmuxWrapper {
        /// Path to the output capture file
        #[arg(long)]
        output_file: String,
        /// Path to the input FIFO
        #[arg(long)]
        input_fifo: String,
        /// Path to the prompt file
        #[arg(long)]
        prompt_file: String,
        /// Working directory (defaults to ".")
        #[arg(long, default_value = ".")]
        cwd: String,
        /// Input mode: fifo (default) or pipe
        #[arg(long, value_enum, default_value_t = InputModeArg::Fifo)]
        input_mode: InputModeArg,
        /// Claude command and arguments (after --)
        #[arg(last = true)]
        claude_cmd: Vec<String>,
    },
    /// tmux + Codex CLI integration wrapper (Unix only)
    #[cfg(unix)]
    CodexTmuxWrapper {
        /// Path to the output capture file
        #[arg(long)]
        output_file: String,
        /// Path to the input FIFO
        #[arg(long)]
        input_fifo: String,
        /// Path to the prompt file
        #[arg(long)]
        prompt_file: String,
        /// Path to codex binary
        #[arg(long)]
        codex_bin: String,
        /// Optional codex model override
        #[arg(long)]
        codex_model: Option<String>,
        /// Optional reasoning effort (low, normal, high, xhigh)
        #[arg(long)]
        reasoning_effort: Option<String>,
        /// Optional resume session id for the first turn
        #[arg(long)]
        resume_session_id: Option<String>,
        /// Working directory (defaults to ".")
        #[arg(long, default_value = ".")]
        cwd: String,
        /// Input mode: fifo (default) or pipe
        #[arg(long, value_enum, default_value_t = InputModeArg::Fifo)]
        input_mode: InputModeArg,
        /// Auto-compact token limit (absolute token count)
        #[arg(long)]
        compact_token_limit: Option<u64>,
    },
    /// tmux + Qwen CLI integration wrapper (Unix only)
    #[cfg(unix)]
    QwenTmuxWrapper {
        /// Path to the output capture file
        #[arg(long)]
        output_file: String,
        /// Path to the input FIFO
        #[arg(long)]
        input_fifo: String,
        /// Path to the prompt file
        #[arg(long)]
        prompt_file: String,
        /// Path to qwen binary
        #[arg(long)]
        qwen_bin: String,
        /// Optional qwen model override
        #[arg(long)]
        qwen_model: Option<String>,
        /// Qwen built-in core tool allowlist entry
        #[arg(long = "qwen-core-tool")]
        qwen_core_tools: Vec<String>,
        /// Optional resume session id for the first turn
        #[arg(long)]
        resume_session_id: Option<String>,
        /// Working directory (defaults to ".")
        #[arg(long, default_value = ".")]
        cwd: String,
        /// Input mode: fifo (default) or pipe
        #[arg(long, value_enum, default_value_t = InputModeArg::Fifo)]
        input_mode: InputModeArg,
    },
    /// Kill all AgentDesk-* tmux sessions and clean temp files
    ResetTmux,
    /// Check if MCP tool(s) are registered in .claude/settings.json
    Ismcptool {
        /// Tool names to check
        #[arg(required = true)]
        tools: Vec<String>,
    },
    /// Add MCP tool permission(s) to .claude/settings.json
    Addmcptool {
        /// Tool names to add
        #[arg(required = true)]
        tools: Vec<String>,
    },
    /// Show server health, active sessions, and auto-queue status
    Status,
    /// List kanban cards
    Cards {
        /// Filter by status (e.g. ready, in_progress, done)
        #[arg(long)]
        status: Option<String>,
    },
    /// Dispatch operations
    Dispatch(DispatchArgs),
    /// Resume a stuck kanban card from its current state
    Resume {
        /// Card ID or GitHub issue number
        card_id: String,
        /// Force resume (bypass gates for pending_decision/blocked)
        #[arg(long)]
        force: bool,
        /// Reason for audit log
        #[arg(long)]
        reason: Option<String>,
    },
    /// Complete pending dispatch and advance to review
    Advance {
        /// GitHub issue number
        issue_number: String,
    },
    /// Show auto-queue status with thread links
    Queue,
    /// Build + deploy dev + promote to release
    Deploy,
    /// List agents and their status
    Agents,
    /// Runtime config get/set
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Call any API endpoint (curl replacement)
    Api {
        /// HTTP method (GET, POST, PATCH, PUT, DELETE)
        method: String,
        /// API path (e.g. /api/health)
        path: String,
        /// Optional JSON body
        body: Option<String>,
    },
    /// List session termination events
    Terminations {
        /// Filter by kanban card ID
        #[arg(long)]
        card_id: Option<String>,
        /// Filter by dispatch ID
        #[arg(long)]
        dispatch_id: Option<String>,
        /// Filter by session key
        #[arg(long)]
        session: Option<String>,
        /// Max number of events to show
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    /// Environment diagnostics
    Doctor {
        /// Apply safe local repairs before running diagnostics
        #[arg(long)]
        fix: bool,
        /// Emit machine-readable JSON output for agent parsing
        #[arg(long)]
        json: bool,
    },
    /// Migration helpers
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
}

#[derive(Subcommand)]
pub(crate) enum DispatchAction {
    /// List active dispatches
    List,
    /// Retry a dispatch for a card
    Retry {
        /// Kanban card ID
        card_id: String,
    },
    /// Redispatch a card
    Redispatch {
        /// Kanban card ID
        card_id: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum AutoQueueAction {
    /// Activate an auto-queue run
    Activate {
        /// Auto-queue run ID
        #[arg(long = "run")]
        run_id: Option<String>,
        /// Target agent ID
        #[arg(long = "agent")]
        agent_id: Option<String>,
        /// Repository in owner/repo form
        #[arg(long)]
        repo: Option<String>,
        /// Restrict activation to already-live entries
        #[arg(long)]
        active_only: bool,
    },
    /// Add a card to an auto-queue run
    Add {
        /// Card ID or issue number
        card_id: String,
        /// Auto-queue run ID
        #[arg(long = "run")]
        run_id: Option<String>,
        /// Explicit priority rank
        #[arg(long)]
        priority: Option<i64>,
        /// Batch phase
        #[arg(long)]
        phase: Option<i64>,
        /// Agent override
        #[arg(long = "agent")]
        agent_id: Option<String>,
    },
    /// Update auto-queue runtime config
    Config {
        /// Auto-queue run ID
        #[arg(long = "run")]
        run_id: Option<String>,
        /// Repository in owner/repo form
        #[arg(long)]
        repo: Option<String>,
        /// Target agent ID
        #[arg(long = "agent")]
        agent_id: Option<String>,
        /// Max concurrently active threads
        #[arg(long = "max-concurrent")]
        max_concurrent_threads: i64,
    },
}

#[derive(Subcommand)]
pub(crate) enum DiscordAction {
    /// Read channel messages
    Read {
        /// Discord channel ID
        channel_id: String,
        /// Max messages to return
        #[arg(long)]
        limit: Option<u32>,
        /// Read messages before this message ID
        #[arg(long)]
        before: Option<String>,
        /// Read messages after this message ID
        #[arg(long)]
        after: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum CardAction {
    /// Create a card from a GitHub issue
    Create {
        /// GitHub issue number to import
        #[arg(long = "from-issue")]
        issue_number: i64,
        /// Repository in owner/repo form
        #[arg(long)]
        repo: Option<String>,
        /// Target status: backlog or ready
        #[arg(long)]
        status: Option<String>,
        /// Agent assignment for ready cards
        #[arg(long = "agent")]
        agent_id: Option<String>,
    },
    /// Inspect a card and connected lifecycle state
    Status {
        /// Card ID or GitHub issue number
        card_ref: String,
        /// Repository in owner/repo form
        #[arg(long)]
        repo: Option<String>,
    },
}

#[derive(Args)]
pub(crate) struct DispatchArgs {
    #[command(subcommand)]
    pub(crate) action: Option<DispatchAction>,
    /// Issue groups such as `423,405` or `407`
    #[arg(value_name = "ISSUE_GROUP")]
    pub(crate) issue_groups: Vec<String>,
    /// Repository full name. Defaults to git remote inference when possible.
    #[arg(long)]
    pub(crate) repo: Option<String>,
    /// Target agent id for auto-assignment and activation filtering.
    #[arg(long = "agent")]
    pub(crate) agent_id: Option<String>,
    /// Legacy compatibility flag forwarded to the API.
    #[arg(long)]
    pub(crate) unified: bool,
    /// Max number of concurrently active thread groups.
    #[arg(long)]
    pub(crate) concurrent: Option<i64>,
    /// Generate the run but do not activate it.
    #[arg(long)]
    pub(crate) no_activate: bool,
}

#[derive(Subcommand)]
pub(crate) enum MigrateAction {
    /// Import OpenClaw durable state into AgentDesk
    Openclaw(super::migrate::OpenClawMigrateArgs),
}

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Get current runtime config
    Get,
    /// Set runtime config (JSON string)
    Set {
        /// JSON value to set
        json: String,
    },
    /// Audit config source-of-truth drift across yaml/DB/legacy files
    Audit {
        /// Preview migrations without writing files or syncing the DB
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Clone, ValueEnum)]
pub(crate) enum ReportProvider {
    Claude,
    Codex,
    Gemini,
    Qwen,
}

#[derive(Clone, ValueEnum)]
#[cfg(unix)]
pub(crate) enum InputModeArg {
    Fifo,
    Pipe,
}

pub(crate) enum ParseOutcome {
    RunServer,
    Command(Commands),
}

pub(crate) fn parse() -> ParseOutcome {
    match Cli::try_parse() {
        Ok(cli) => match cli.command {
            Some(command) => ParseOutcome::Command(command),
            None => ParseOutcome::RunServer,
        },
        Err(error) => {
            if error.kind() == clap::error::ErrorKind::DisplayHelp
                || error.kind() == clap::error::ErrorKind::DisplayVersion
            {
                error.print().ok();
                std::process::exit(0);
            }
            let has_args = std::env::args().count() > 1;
            if has_args {
                error.print().ok();
                std::process::exit(1);
            }
            ParseOutcome::RunServer
        }
    }
}
