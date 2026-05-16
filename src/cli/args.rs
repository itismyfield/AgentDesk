use std::path::PathBuf;

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
    /// Emit a launchd plist using the canonical Rust renderer
    EmitLaunchdPlist(EmitLaunchdPlistArgs),
    /// Restart Discord bot server(s)
    RestartDcserver {
        /// Discord channel ID for restart completion report
        #[arg(long)]
        report_channel_id: Option<u64>,
        /// Provider for restart report (claude, codex, gemini, opencode, or qwen)
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
    /// Send a trigger-capable agent handoff through the announce bot
    SendToAgent {
        /// Source agent id recorded in the message envelope
        #[arg(long = "from")]
        from_agent_id: String,
        /// Target agent id whose Discord channel binding is used
        #[arg(long = "to")]
        to_agent_id: String,
        /// Message body
        #[arg(long)]
        message: String,
        /// Target channel binding kind
        #[arg(long = "channel-kind", value_enum, default_value = "cc")]
        channel_kind: AgentHandoffChannelKindArg,
        /// Send the body without the automatic handoff prefix
        #[arg(long = "no-prefix")]
        no_prefix: bool,
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
    /// Recover a review dispatch target commit/worktree through the API
    ReviewRecoverTarget {
        /// Review dispatch ID
        #[arg(long = "dispatch")]
        dispatch_id: Option<String>,
        /// Kanban card ID, used to resolve the latest review dispatch when --dispatch is omitted
        #[arg(long = "card")]
        card_id: Option<String>,
        /// Reviewed commit SHA to pin
        #[arg(long = "commit")]
        target_commit: Option<String>,
        /// Worktree path whose HEAD must match the reviewed commit
        #[arg(long = "worktree")]
        worktree_path: Option<String>,
        /// Operator reason stored in the audit event
        #[arg(long)]
        reason: Option<String>,
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
    /// Monitor status commands for Discord channels
    Monitoring {
        #[command(subcommand)]
        action: MonitoringAction,
    },
    /// intake_outbox operator tools (Phase 5 of intake-node-routing)
    IntakeOutbox {
        #[command(subcommand)]
        action: IntakeOutboxAction,
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
        /// Override Codex native fast mode for every turn in this wrapper session
        #[arg(long, value_enum)]
        fast_mode_state: Option<FastModeStateArg>,
        /// Override Codex goals feature flag for every turn in this wrapper session
        #[arg(long, value_enum)]
        goals_state: Option<FeatureStateArg>,
        /// Working directory (defaults to ".")
        #[arg(long, default_value = ".")]
        cwd: String,
        /// Additional directory writable alongside the primary workspace
        #[arg(long = "add-dir")]
        add_dirs: Vec<String>,
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
    /// Relay Claude Code hook stdin JSON to the AgentDesk TUI hook receiver
    ClaudeHookRelay {
        /// Hook receiver base endpoint, e.g. http://127.0.0.1:49152
        #[arg(long)]
        endpoint: String,
        /// Provider id, e.g. claude
        #[arg(long)]
        provider: String,
        /// Claude hook event name, e.g. Stop or PreToolUse
        #[arg(long)]
        event: String,
        /// TUI session id assigned by AgentDesk
        #[arg(long = "session-id")]
        session_id: String,
    },
    /// Relay Codex hook stdin JSON to the AgentDesk TUI hook receiver
    CodexHookRelay {
        /// Hook receiver base endpoint, e.g. http://127.0.0.1:49152
        #[arg(long)]
        endpoint: String,
        /// Provider id, e.g. codex
        #[arg(long)]
        provider: String,
        /// Codex hook event name, e.g. Stop or PreToolUse
        #[arg(long)]
        event: String,
        /// TUI session id assigned by AgentDesk
        #[arg(long = "session-id")]
        session_id: String,
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
        /// Force resume (bypass guards for manual-intervention states)
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
    /// Show turn/session diagnostics for an agent ID or Discord channel ID
    Diag {
        /// Agent ID or Discord channel ID
        identifier: String,
        /// Emit machine-readable JSON output
        #[arg(long)]
        json: bool,
    },
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
        /// Allow doctor --fix to restart the dcserver service
        #[arg(long)]
        allow_restart: bool,
        /// Allow doctor --fix to run SQLite schema repair and remove stale SQLite cache files
        #[arg(long)]
        repair_sqlite_cache: bool,
        /// Allow sending configured auth tokens to a non-loopback AGENTDESK_API_URL
        #[arg(long)]
        allow_remote: bool,
        /// Restrict checks to a profile
        #[arg(long, value_enum)]
        profile: Option<DoctorProfileArg>,
        /// Emit machine-readable JSON output for agent parsing
        #[arg(long)]
        json: bool,
    },
    /// Migration helpers
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
    /// Provider CLI safe migration management
    ProviderCli(super::provider_cli::ProviderCliArgs),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum LaunchdPlistFlavorArg {
    Release,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum DoctorProfileArg {
    Quick,
    Deep,
    Security,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum AgentHandoffChannelKindArg {
    Cc,
    Cdx,
}

#[derive(Args, Clone, Debug)]
pub(crate) struct EmitLaunchdPlistArgs {
    /// Runtime flavor to render
    #[arg(long, value_enum)]
    pub(crate) flavor: LaunchdPlistFlavorArg,
    /// Override the home directory used in generated paths
    #[arg(long)]
    pub(crate) home: Option<PathBuf>,
    /// Override the runtime root directory used in generated paths
    #[arg(long = "root-dir")]
    pub(crate) root_dir: Option<PathBuf>,
    /// Override the agentdesk binary path used in ProgramArguments
    #[arg(long = "agentdesk-bin")]
    pub(crate) agentdesk_bin: Option<PathBuf>,
    /// Write the plist to this path instead of stdout
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
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
pub(crate) enum IntakeOutboxAction {
    /// Show recent intake_outbox rows (newest first).
    Status {
        /// Filter to a single Discord channel id.
        #[arg(long = "channel")]
        channel_id: Option<String>,
        /// Maximum rows to display.
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Force-fail a stuck row and re-enqueue a fresh attempt.
    /// Implements transition 12 from the design doc.
    ForceFail {
        /// id of the stuck row.
        #[arg(long)]
        id: i64,
        /// Operator's reason text recorded in `last_error`.
        #[arg(long, default_value = "operator force-fail via CLI")]
        reason: String,
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
        /// Thread group
        #[arg(long = "thread-group")]
        thread_group: Option<i64>,
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
pub(crate) enum MonitoringAction {
    /// Register or refresh a monitor status entry
    Start {
        /// Discord channel ID
        #[arg(long)]
        channel: u64,
        /// Monitor key, stable across refreshes
        #[arg(long)]
        key: String,
        /// Human-readable monitor description
        #[arg(long)]
        description: String,
    },
    /// Remove a monitor status entry
    Stop {
        /// Discord channel ID
        #[arg(long)]
        channel: u64,
        /// Monitor key to remove
        #[arg(long)]
        key: String,
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
    /// Cut over SQLite history into PostgreSQL and verify live state is drained
    PostgresCutover(super::migrate::PostgresCutoverArgs),
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
    /// Sync runtime MCP servers into provider config files
    SyncMcp,
}

#[derive(Clone, ValueEnum)]
pub(crate) enum ReportProvider {
    Claude,
    Codex,
    Gemini,
    #[value(name = "opencode", alias = "open-code")]
    OpenCode,
    Qwen,
}

#[derive(Clone, ValueEnum)]
#[cfg(unix)]
pub(crate) enum InputModeArg {
    Fifo,
    Pipe,
}

#[derive(Clone, Copy, ValueEnum)]
#[cfg(unix)]
pub(crate) enum FastModeStateArg {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, ValueEnum)]
#[cfg(unix)]
pub(crate) enum FeatureStateArg {
    Enabled,
    Disabled,
}

pub(crate) enum ParseOutcome {
    RunServer,
    Command(Commands),
}

fn rewrite_legacy_args(mut args: Vec<String>) -> Vec<String> {
    if args.get(1).map(String::as_str) == Some("--cutover-pg") {
        let mut rewritten = Vec::with_capacity(args.len() + 1);
        rewritten.push(args.remove(0));
        rewritten.push("migrate".to_string());
        rewritten.push("postgres-cutover".to_string());
        rewritten.extend(args.into_iter().skip(1));
        return rewritten;
    }
    if args.get(1).map(String::as_str) == Some("--emit-launchd-plist") {
        let mut rewritten = Vec::with_capacity(args.len() + 1);
        rewritten.push(args.remove(0));
        rewritten.push("emit-launchd-plist".to_string());
        rewritten.extend(args.into_iter().skip(1));
        return rewritten;
    }
    args
}

pub(crate) fn parse() -> ParseOutcome {
    match Cli::try_parse_from(rewrite_legacy_args(std::env::args().collect())) {
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    #[test]
    fn auto_queue_add_parses_thread_group_flag() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "auto-queue",
            "add",
            "--phase",
            "1",
            "--thread-group",
            "2",
            "610",
        ])
        .expect("cli args should parse");

        match cli.command {
            Some(Commands::AutoQueue {
                action:
                    AutoQueueAction::Add {
                        card_id,
                        phase,
                        thread_group,
                        ..
                    },
            }) => {
                assert_eq!(card_id, "610");
                assert_eq!(phase, Some(1));
                assert_eq!(thread_group, Some(2));
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }

    #[test]
    fn send_to_agent_parses_defaults() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "send-to-agent",
            "--from",
            "project-agentdesk",
            "--to",
            "adk-dashboard",
            "--message",
            "hello",
        ])
        .expect("send-to-agent args should parse");

        match cli.command {
            Some(Commands::SendToAgent {
                from_agent_id,
                to_agent_id,
                message,
                channel_kind,
                no_prefix,
            }) => {
                assert_eq!(from_agent_id, "project-agentdesk");
                assert_eq!(to_agent_id, "adk-dashboard");
                assert_eq!(message, "hello");
                assert_eq!(channel_kind, AgentHandoffChannelKindArg::Cc);
                assert!(!no_prefix);
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }

    #[test]
    fn send_to_agent_parses_cdx_without_prefix() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "send-to-agent",
            "--from",
            "project-agentdesk",
            "--to",
            "adk-dashboard",
            "--message",
            "hello",
            "--channel-kind",
            "cdx",
            "--no-prefix",
        ])
        .expect("send-to-agent args should parse");

        match cli.command {
            Some(Commands::SendToAgent {
                channel_kind,
                no_prefix,
                ..
            }) => {
                assert_eq!(channel_kind, AgentHandoffChannelKindArg::Cdx);
                assert!(no_prefix);
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }

    #[test]
    fn migrate_postgres_cutover_parses_archive_dir() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "migrate",
            "postgres-cutover",
            "--dry-run",
            "--archive-dir",
            "/tmp/agentdesk-cutover",
        ])
        .expect("cli args should parse");

        match cli.command {
            Some(Commands::Migrate {
                action: MigrateAction::PostgresCutover(args),
            }) => {
                assert!(args.dry_run);
                assert_eq!(args.archive_dir.as_deref(), Some("/tmp/agentdesk-cutover"));
                assert!(!args.skip_pg_import);
                assert!(!args.allow_runtime_active);
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }

    #[test]
    fn migrate_postgres_cutover_parses_allow_runtime_active() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "migrate",
            "postgres-cutover",
            "--skip-pg-import",
            "--archive-dir",
            "/tmp/agentdesk-cutover",
            "--allow-runtime-active",
        ])
        .expect("cli args should parse");

        match cli.command {
            Some(Commands::Migrate {
                action: MigrateAction::PostgresCutover(args),
            }) => {
                assert!(args.skip_pg_import);
                assert!(args.allow_runtime_active);
                assert_eq!(args.archive_dir.as_deref(), Some("/tmp/agentdesk-cutover"));
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }

    #[test]
    fn legacy_cutover_pg_flag_rewrites_to_migrate_command() {
        let cli = Cli::try_parse_from(rewrite_legacy_args(vec![
            "agentdesk".to_string(),
            "--cutover-pg".to_string(),
            "--dry-run".to_string(),
            "--archive-dir".to_string(),
            "/tmp/agentdesk-cutover".to_string(),
        ]))
        .expect("legacy cutover args should parse");

        match cli.command {
            Some(Commands::Migrate {
                action: MigrateAction::PostgresCutover(args),
            }) => {
                assert!(args.dry_run);
                assert_eq!(args.archive_dir.as_deref(), Some("/tmp/agentdesk-cutover"));
                assert!(!args.skip_pg_import);
                assert!(!args.allow_unsent_messages);
                assert!(!args.allow_runtime_active);
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }

    #[test]
    fn legacy_emit_launchd_plist_flag_rewrites_to_command() {
        let cli = Cli::try_parse_from(rewrite_legacy_args(vec![
            "agentdesk".to_string(),
            "--emit-launchd-plist".to_string(),
            "--flavor".to_string(),
            "release".to_string(),
        ]))
        .expect("legacy emit-launchd-plist args should parse");

        match cli.command {
            Some(Commands::EmitLaunchdPlist(args)) => {
                assert_eq!(args.flavor, LaunchdPlistFlavorArg::Release);
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }

    #[test]
    fn migrate_postgres_cutover_parses_allow_unsent_messages_flag() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "migrate",
            "postgres-cutover",
            "--dry-run",
            "--allow-unsent-messages",
        ])
        .expect("cli args should parse");

        match cli.command {
            Some(Commands::Migrate {
                action: MigrateAction::PostgresCutover(args),
            }) => {
                assert!(args.dry_run);
                assert!(args.allow_unsent_messages);
                assert!(!args.allow_runtime_active);
            }
            other => panic!(
                "unexpected parse result: {:?}",
                other.map(|_| "other command")
            ),
        }
    }
}
