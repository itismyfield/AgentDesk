use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "agentdesk", version = env!("CARGO_PKG_VERSION"), about = "AI agent orchestration platform")]
struct Cli {
    /// Emit machine-readable JSON instead of the human-readable text output.
    /// Accepted globally — before or after the subcommand. Commands that only
    /// ever emit JSON treat this as a no-op (never a double-encode).
    #[arg(long, global = true)]
    json: bool,
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
    /// Deprecated: send a file to a Discord channel
    #[command(
        after_help = "Deprecated compatibility command. The routed `send` command does not currently accept file attachments, so keep using this command until an attachment-capable replacement is available."
    )]
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
    /// Deprecated: send a message to a Discord channel
    #[command(
        after_help = "Deprecated compatibility command. With `--key`, this command selects that one configured bot token; without `--key`, it tries configured bot tokens sequentially until one succeeds. For a role-mapped channel with a configured bot runtime, migrate to `agentdesk send --target channel:<id> --content <TEXT>`. Routed `send` instead requires role-map authorization and selects one `--bot` runtime (default: announce), so it does not preserve this command's token selection or sequential fallback. Keep using this compatibility command when those routing requirements do not match the current delivery."
    )]
    DiscordSendmessage {
        /// Discord channel ID
        #[arg(long)]
        channel: u64,
        /// Message text
        #[arg(long)]
        message: String,
        /// Authentication key hash (optional; without it, configured bot tokens are tried sequentially until one succeeds)
        #[arg(long)]
        key: Option<String>,
    },
    /// Deprecated: send a direct message to a Discord user
    #[command(
        after_help = "Deprecated compatibility command. With `--key`, this command selects that one configured bot token; without `--key`, it tries configured bot tokens sequentially until one succeeds. Routed `send` instead requires a role-map-authorized channel or agent target and selects one configured `--bot` runtime (default: announce); it does not preserve this token fallback, target a Discord user, or create a DM. Keep using this command until a supported replacement is available."
    )]
    DiscordSenddm {
        /// Discord user ID
        #[arg(long)]
        user: u64,
        /// Message text
        #[arg(long)]
        message: String,
        /// Authentication key hash (optional; without it, configured bot tokens are tried sequentially until one succeeds)
        #[arg(long)]
        key: Option<String>,
    },
    /// Send a routed Discord message without HTTP server dependency
    Send {
        #[arg(
            long,
            help = crate::services::discord::outbound::send_target::SEND_TARGET_CONTRACT
        )]
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
        /// Required: `true` → 회신 필수, `false` → 회신 불필요.
        #[arg(long = "expect-reply", action = clap::ArgAction::Set, required = true)]
        expect_reply: bool,
        /// #3556 — reserve a headless turn on the target mailbox instead of
        /// posting an announce message. Authoritative wake-up: exits non-zero
        /// on 409 (mailbox busy). Mutually exclusive with the default
        /// announce-post behavior.
        #[arg(long = "start-turn")]
        start_turn: bool,
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
        /// Optional developer instructions passed through Codex config
        #[arg(long)]
        developer_instructions: Option<String>,
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
    /// #2655: install (or refresh) the Memento `context()` SessionStart hook
    /// and a UserPromptSubmit token-pressure reminder into a Claude Code
    /// settings file. Idempotent — running twice produces the same file.
    InstallMementoSessionHook {
        /// Path to the settings.json to mutate. Defaults to
        /// `~/.claude/settings.json` when omitted.
        #[arg(long)]
        settings_path: Option<String>,
        /// Skip writing and instead print the rendered settings to stdout.
        #[arg(long)]
        dry_run: bool,
        /// Remove the AgentDesk-managed memento hook entries instead of
        /// installing them. Useful when an operator wants to revert.
        #[arg(long, conflicts_with = "dry_run")]
        uninstall: bool,
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
    /// Unified queue + dispatch + phase-gate inspection (issue #2651).
    ///
    /// First-class replacement for the ad-hoc `curl /api/queue/status |
    /// python -c '...'` polling pattern. Aggregates the queue run state,
    /// pending dispatches, and phase-gate snapshot into one structured
    /// response. Pass `--json` for machine-readable output.
    Query {
        #[command(subcommand)]
        action: Option<QueryAction>,
        /// Filter result rows. Repeatable. Format: `key:value`
        /// (e.g. `--filter status:pending`, `--filter dispatch_type:review`).
        #[arg(long = "filter", global = true)]
        filters: Vec<String>,
        /// Restrict to a specific agent_id when applicable.
        #[arg(long, global = true)]
        agent: Option<String>,
        /// Maximum rows to render per section (0 = unlimited).
        #[arg(long, global = true, default_value_t = 0)]
        limit: usize,
    },
    /// Phase-gate violation snapshot (issue #2657).
    ///
    /// Read-only inspector that flags auto-queue entries dispatched at a
    /// higher `batch_phase` than the run's live phase pointer. Mirrors the
    /// `/adk-phase` Discord slash command.
    Phase {
        #[command(subcommand)]
        action: Option<PhaseAction>,
        /// Show full per-entry detail.
        #[arg(long, global = true)]
        detailed: bool,
    },
    /// Build + deploy dev + promote to release
    Deploy,
    /// List agents and their status
    Agents,
    /// Show turn/session diagnostics for an agent ID or Discord channel ID
    Diag {
        /// Agent ID or Discord channel ID
        identifier: String,
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
    },
    /// Migration helpers
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
    /// Provider CLI safe migration management
    ProviderCli(super::provider_cli::ProviderCliArgs),
    /// Inspect deterministic session-binding values (epic #2285 E1).
    Show {
        #[command(subcommand)]
        action: ShowAction,
    },
    /// Show consolidated health snapshot of the current node (server status,
    /// dcserver pid, last deploy time, queue lag, Discord/disk/outbox).
    Health,
    /// Compare release/main/dev state across every registered worker node
    /// (`mac-mini`, `mac-book`, …). Renders a side-by-side table with
    /// dcserver pid, last deploy, queue lag, and a `diff` column.
    MachineCompare,
    /// Time-windowed activity report: commits / closed issues / merged PRs /
    /// deploys / incidents in a single table. Uses gh + git + AgentDesk API.
    Activity {
        /// Window start. Accepts RFC3339 (`2026-05-19T23:10:00+09:00`),
        /// duration suffix (`24h`, `90m`, `7d`), or `since:<sha>` to anchor
        /// on a commit (commit time becomes the start).
        #[arg(long)]
        since: String,
        /// Optional window end (RFC3339). Defaults to `now` when omitted.
        #[arg(long)]
        until: Option<String>,
        /// Repository in owner/repo form. Defaults to the current
        /// repo's `origin` remote when omitted.
        #[arg(long)]
        repo: Option<String>,
        /// Skip the AgentDesk-side deploy / incident lookup (useful when
        /// the local API is offline). Pure git + gh report.
        #[arg(long = "no-agentdesk")]
        no_agentdesk: bool,
    },
}

/// Subcommands for `adk query` (issue #2651).
///
/// Each variant aggregates one logical slice of runtime state. `All` (the
/// default when no subcommand is given) fetches every slice in parallel for
/// a single-shot snapshot — that is the curl-replacement happy path.
#[derive(Subcommand)]
pub(crate) enum QueryAction {
    /// Auto-queue run + entries snapshot (calls `/api/queue/status`).
    Queue,
    /// Pending dispatches across all agents (calls `/api/dispatches/pending`).
    Dispatches,
    /// Phase-gate catalog + active gate state (calls
    /// `/api/queue/phase-gates/catalog`).
    PhaseGate,
    /// All sections in one shot (default).
    All,
}

/// Subcommands for `adk phase` (issue #2657).
#[derive(Subcommand)]
pub(crate) enum PhaseAction {
    /// Show current violations (default).
    Status,
}

#[derive(Subcommand)]
pub(crate) enum ShowAction {
    /// Print the expected tmux session name for a Discord channel.
    SessionName {
        /// Discord channel id (or stable channel identifier) — same value
        /// AgentDesk uses when composing the tmux session name.
        #[arg(long = "channel")]
        channel: String,
        /// Provider to encode in the session name. Required unless the channel
        /// name ends in a registered provider suffix (`-cc`, `-cdx`, `-gm`,
        /// `-oc`, `-qw`). The CLI deliberately does not read live channel
        /// bindings — operator output must be reproducible from arguments
        /// alone.
        #[arg(long)]
        provider: Option<String>,
    },
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
    /// Override the launchd Label in the generated plist
    #[arg(long)]
    pub(crate) label: Option<String>,
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
    /// Create a guild category. Idempotent (list-then-create): returns the
    /// existing category when one with the same name is already present.
    /// Not safe under concurrent invocations against the same guild.
    CategoryCreate {
        /// Category display name
        #[arg(long)]
        name: String,
        /// Override the configured guild id
        #[arg(long)]
        guild_id: Option<String>,
    },
    /// Create a text channel under an optional category. Idempotent by
    /// (name, parent category) via list-then-create. Not safe under
    /// concurrent invocations against the same guild.
    ChannelCreate {
        /// Channel display name
        #[arg(long)]
        name: String,
        /// Parent category channel id
        #[arg(long)]
        category_id: Option<String>,
        /// Optional channel topic
        #[arg(long)]
        topic: Option<String>,
        /// Override the configured guild id
        #[arg(long)]
        guild_id: Option<String>,
    },
    /// Create a thread under a text/news channel or a post under a forum/media
    /// channel. Cross-process safe and idempotent by (parent, normalized name)
    /// across active and archived public threads.
    ThreadCreate {
        /// Parent text, news, forum, or media channel id
        #[arg(long)]
        parent_channel_id: String,
        /// Thread name
        #[arg(long)]
        name: String,
        /// Starter message required for forum/media parents (alias: --starter-message)
        #[arg(long, alias = "starter-message")]
        message: Option<String>,
        /// Forum/media tag id to apply (repeat for multiple tags)
        #[arg(long = "tag-id")]
        tag_ids: Vec<u64>,
        /// Auto-archive duration in minutes (60, 1440, 4320, 10080)
        #[arg(long, default_value_t = 1440)]
        auto_archive_minutes: u16,
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
}

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Get current runtime config
    Get,
    /// Set runtime config (JSON string)
    Set {
        /// JSON value to set
        #[arg(id = "config_json", value_name = "JSON")]
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
    Command { command: Commands, json: bool },
}

fn rewrite_legacy_args(mut args: Vec<String>) -> Vec<String> {
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
            Some(command) => ParseOutcome::Command {
                command,
                json: cli.json,
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser, error::ErrorKind};

    #[test]
    fn top_level_command_name_snapshot_preserves_public_cli_surface() {
        let mut command = Cli::command();
        command.build();
        let actual = command
            .get_subcommands()
            .map(|command| command.get_name())
            .collect::<Vec<_>>();
        let expected = vec![
            "dcserver",
            "init",
            "reconfigure",
            "emit-launchd-plist",
            "restart-dcserver",
            "discord-sendfile",
            "discord-sendmessage",
            "discord-senddm",
            "send",
            "send-to-agent",
            "review-verdict",
            "review-decision",
            "review-recover-target",
            "docs",
            "auto-queue",
            "force-kill",
            "github-sync",
            "monitoring",
            "intake-outbox",
            "discord",
            "card",
            "cherry-merge",
            #[cfg(unix)]
            "tmux-wrapper",
            #[cfg(unix)]
            "codex-tmux-wrapper",
            #[cfg(unix)]
            "qwen-tmux-wrapper",
            "claude-hook-relay",
            "codex-hook-relay",
            "reset-tmux",
            "ismcptool",
            "addmcptool",
            "install-memento-session-hook",
            "status",
            "cards",
            "dispatch",
            "resume",
            "advance",
            "queue",
            "query",
            "phase",
            "deploy",
            "agents",
            "diag",
            "config",
            "api",
            "terminations",
            "doctor",
            "migrate",
            "provider-cli",
            "show",
            "health",
            "machine-compare",
            "activity",
            "help",
        ];

        assert_eq!(actual, expected);
    }

    #[test]
    fn clap_help_and_version_remain_successful_authoritative_outputs() {
        for flag in ["-h", "--help"] {
            let Err(error) = Cli::try_parse_from(["agentdesk", flag]) else {
                panic!("{flag} must render help instead of running a command");
            };
            assert_eq!(error.kind(), ErrorKind::DisplayHelp);
            assert_eq!(error.exit_code(), 0);
            let rendered = error.to_string();
            assert!(rendered.contains("AI agent orchestration platform"));
            assert!(rendered.contains("Usage: agentdesk"));
            assert!(rendered.contains("Commands:"));
            assert!(rendered.contains("discord-sendmessage"));
            assert!(rendered.contains("provider-cli"));
        }

        let Err(error) = Cli::try_parse_from(["agentdesk", "help"]) else {
            panic!("help subcommand must render help instead of running a command");
        };
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert_eq!(error.exit_code(), 0);

        for flag in ["-V", "--version"] {
            let Err(error) = Cli::try_parse_from(["agentdesk", flag]) else {
                panic!("{flag} must render version instead of running a command");
            };
            assert_eq!(error.kind(), ErrorKind::DisplayVersion);
            assert_eq!(error.exit_code(), 0);
            assert_eq!(
                error.to_string().trim(),
                format!("agentdesk {}", env!("CARGO_PKG_VERSION"))
            );
        }
    }

    #[test]
    fn legacy_flat_discord_send_commands_still_parse_unchanged() {
        let file = Cli::try_parse_from([
            "agentdesk",
            "discord-sendfile",
            "/tmp/report.txt",
            "--channel",
            "42",
            "--key",
            "key-hash",
        ])
        .expect("legacy file send should still parse");
        assert!(matches!(
            file.command,
            Some(Commands::DiscordSendfile {
                path,
                channel: 42,
                key,
            }) if path == "/tmp/report.txt" && key == "key-hash"
        ));

        let channel = Cli::try_parse_from([
            "agentdesk",
            "discord-sendmessage",
            "--channel",
            "42",
            "--message",
            "hello channel",
        ])
        .expect("legacy channel send should still parse");
        assert!(matches!(
            channel.command,
            Some(Commands::DiscordSendmessage {
                channel: 42,
                message,
                key: None,
            }) if message == "hello channel"
        ));

        let dm = Cli::try_parse_from([
            "agentdesk",
            "discord-senddm",
            "--user",
            "43",
            "--message",
            "hello user",
        ])
        .expect("legacy DM send should still parse");
        assert!(matches!(
            dm.command,
            Some(Commands::DiscordSenddm {
                user: 43,
                message,
                key: None,
            }) if message == "hello user"
        ));
    }

    #[test]
    fn legacy_flat_discord_send_help_matches_routed_send_contract() {
        fn help_for(command: &str) -> String {
            let Err(error) = Cli::try_parse_from(["agentdesk", command, "--help"]) else {
                panic!("{command} --help must render help instead of running the command");
            };
            assert_eq!(error.kind(), ErrorKind::DisplayHelp, "{command}");
            assert_eq!(error.exit_code(), 0, "{command}");
            error.to_string()
        }

        fn assert_legacy_token_contract(help: &str, command: &str) -> String {
            let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
            for required in [
                "With `--key`",
                "one configured bot token",
                "without `--key`",
                "configured bot tokens",
                "sequentially until one succeeds",
            ] {
                assert!(
                    help.contains(required),
                    "{command} help must disclose {required}: {help}"
                );
            }
            assert!(
                !help.contains("AGENTDESK_TOKEN"),
                "{command} must not claim an environment-token fallback it does not implement: {help}"
            );
            help
        }

        let command = Cli::command();
        let send_target_help = command
            .find_subcommand("send")
            .expect("send subcommand")
            .get_arguments()
            .find(|argument| argument.get_id().as_str() == "target")
            .and_then(|argument| argument.get_help())
            .map(ToString::to_string);
        assert_eq!(
            send_target_help.as_deref(),
            Some(crate::services::discord::outbound::send_target::SEND_TARGET_CONTRACT),
            "Clap target help must be sourced from the resolver-owned contract"
        );

        let send = help_for("send");
        for supported_route in ["channel:<id>", "channel:<name>", "agent:<roleId>"] {
            assert!(
                send.contains(supported_route),
                "routed send help must advertise {supported_route}: {send}"
            );
        }
        assert!(
            !send.contains("user:<id>"),
            "unsupported user route: {send}"
        );
        assert!(
            !send.contains("role:<name>"),
            "unsupported role route: {send}"
        );

        let channel = help_for("discord-sendmessage");
        assert!(channel.contains("Deprecated"), "{channel}");
        let channel_contract = assert_legacy_token_contract(&channel, "discord-sendmessage");
        assert!(
            channel_contract.contains("selects one `--bot` runtime")
                && channel_contract.contains("does not preserve")
                && channel_contract.contains("sequential fallback"),
            "routed send must remain explicitly non-equivalent to legacy token fallback: {channel_contract}"
        );
        for migration_token in ["agentdesk", "channel:<id>", "--content"] {
            assert!(
                channel.contains(migration_token),
                "channel migration must include {migration_token}: {channel}"
            );
        }
        for required_caveat in ["role-map", "--bot", "--key", "compatibility"] {
            assert!(
                channel.contains(required_caveat),
                "channel migration must disclose {required_caveat}: {channel}"
            );
        }

        let dm = help_for("discord-senddm");
        assert!(dm.contains("Deprecated"), "{dm}");
        let dm_contract = assert_legacy_token_contract(&dm, "discord-senddm");
        assert!(
            dm_contract.contains("selects one configured `--bot` runtime")
                && dm_contract.contains("does not preserve this token fallback"),
            "routed send must not be presented as a DM token-fallback replacement: {dm_contract}"
        );
        for unsupported_or_gated in ["role-map", "--bot", "--key", "DM", "compatibility"] {
            assert!(
                dm.contains(unsupported_or_gated),
                "DM compatibility help must disclose {unsupported_or_gated}: {dm}"
            );
        }

        let file = help_for("discord-sendfile");
        assert!(file.contains("Deprecated"), "{file}");
        assert!(file.contains("attachments"), "{file}");
    }

    #[test]
    fn legacy_launchd_and_provider_value_aliases_still_parse() {
        let legacy = rewrite_legacy_args(vec![
            "agentdesk".to_string(),
            "--emit-launchd-plist".to_string(),
            "--flavor".to_string(),
            "release".to_string(),
        ]);
        let canonical = vec![
            "agentdesk".to_string(),
            "emit-launchd-plist".to_string(),
            "--flavor".to_string(),
            "release".to_string(),
        ];
        assert_eq!(legacy, canonical);
        assert!(matches!(
            Cli::try_parse_from(legacy)
                .expect("legacy launchd spelling should parse")
                .command,
            Some(Commands::EmitLaunchdPlist(_))
        ));

        let provider_alias = Cli::try_parse_from([
            "agentdesk",
            "restart-dcserver",
            "--report-channel-id",
            "42",
            "--report-provider",
            "open-code",
        ])
        .expect("open-code provider alias should parse");
        assert!(matches!(
            provider_alias.command,
            Some(Commands::RestartDcserver {
                report_provider: Some(ReportProvider::OpenCode),
                ..
            })
        ));
    }

    #[test]
    fn send_to_agent_requires_expect_reply() {
        let result = Cli::try_parse_from([
            "agentdesk",
            "send-to-agent",
            "--from",
            "a",
            "--to",
            "b",
            "--message",
            "hi",
        ]);
        let Err(error) = result else {
            panic!("send-to-agent without --expect-reply should fail");
        };

        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn send_to_agent_parses_expect_reply_true() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "send-to-agent",
            "--from",
            "a",
            "--to",
            "b",
            "--message",
            "hi",
            "--expect-reply",
            "true",
        ])
        .expect("send-to-agent with --expect-reply true should parse");

        match cli.command.expect("subcommand") {
            Commands::SendToAgent { expect_reply, .. } => assert!(expect_reply),
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn send_to_agent_parses_expect_reply_false() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "send-to-agent",
            "--from",
            "a",
            "--to",
            "b",
            "--message",
            "hi",
            "--expect-reply",
            "false",
        ])
        .expect("send-to-agent with --expect-reply false should parse");

        match cli.command.expect("subcommand") {
            Commands::SendToAgent { expect_reply, .. } => assert!(!expect_reply),
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn config_set_parses_json_value() {
        let cli = Cli::try_parse_from(["agentdesk", "config", "set", r#"{"a":1}"#])
            .expect("config set <JSON> parses");

        assert!(matches!(
            cli.command,
            Some(Commands::Config {
                action: ConfigAction::Set { json },
            }) if json == r#"{"a":1}"#
        ));
        assert!(!cli.json);
    }

    #[test]
    fn config_set_help_preserves_json_value_name() {
        let mut command = Cli::command();
        let config = command
            .find_subcommand_mut("config")
            .expect("config subcommand exists");
        let set = config
            .find_subcommand_mut("set")
            .expect("config set subcommand exists");
        let mut rendered = Vec::new();
        set.write_long_help(&mut rendered)
            .expect("config set help renders");
        let rendered = String::from_utf8(rendered).expect("help is UTF-8");

        assert!(rendered.contains("<JSON>"));
        assert!(!rendered.contains("<CONFIG_JSON>"));
    }

    #[test]
    fn config_set_parses_json_value_with_global_json() {
        let cli = Cli::try_parse_from(["agentdesk", "config", "set", r#"{"a":1}"#, "--json"])
            .expect("config set <JSON> --json parses");

        assert!(matches!(
            cli.command,
            Some(Commands::Config {
                action: ConfigAction::Set { json },
            }) if json == r#"{"a":1}"#
        ));
        assert!(cli.json);
    }

    #[test]
    fn provider_cli_status_coexists_with_global_json() {
        // provider-cli status carries its own nested `json`; the global flag
        // must coexist without a clap arg-id conflict (clap builds this arg
        // subtree only when parsing descends into it, so a shallow parse of an
        // unrelated command would not surface a conflict).
        let cli = Cli::try_parse_from(["agentdesk", "provider-cli", "status", "--json"])
            .expect("provider-cli status --json parses");
        assert!(cli.json);
    }

    #[test]
    fn global_json_defaults_false() {
        let cli = Cli::try_parse_from(["agentdesk", "status"]).expect("status parses");
        assert!(!cli.json);
        assert!(matches!(cli.command, Some(Commands::Status)));
    }

    #[test]
    fn global_json_accepted_after_subcommand() {
        let cli =
            Cli::try_parse_from(["agentdesk", "status", "--json"]).expect("status --json parses");
        assert!(cli.json);
    }

    #[test]
    fn global_json_accepted_before_subcommand() {
        let cli =
            Cli::try_parse_from(["agentdesk", "--json", "status"]).expect("--json status parses");
        assert!(cli.json);
    }

    #[test]
    fn global_json_reaches_text_only_commands() {
        for verb in ["cards", "queue", "terminations"] {
            let cli = Cli::try_parse_from(["agentdesk", verb, "--json"])
                .unwrap_or_else(|err| panic!("{verb} --json should parse: {err}"));
            assert!(cli.json, "{verb} --json did not set the global flag");
        }
        let advance = Cli::try_parse_from(["agentdesk", "advance", "42", "--json"])
            .expect("advance <n> --json parses");
        assert!(advance.json);
    }

    #[test]
    fn global_json_unifies_previously_local_json_commands() {
        // These commands used to declare their own `--json`; the flag is now
        // the single global one. Both positions must still parse.
        let health =
            Cli::try_parse_from(["agentdesk", "health", "--json"]).expect("health --json parses");
        assert!(health.json);
        let diag = Cli::try_parse_from(["agentdesk", "--json", "diag", "chan-1"])
            .expect("--json diag parses");
        assert!(diag.json);
        // Nested subcommand still accepts the global flag after the leaf verb.
        let query = Cli::try_parse_from(["agentdesk", "query", "queue", "--json"])
            .expect("query queue --json parses");
        assert!(query.json);
    }

    #[test]
    fn discord_thread_create_parses_optional_forum_starter_message() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "discord",
            "thread-create",
            "--parent-channel-id",
            "123",
            "--name",
            "release-notes",
            "--message",
            "Initial forum post",
        ])
        .expect("thread-create --message parses");

        match cli.command.expect("subcommand") {
            Commands::Discord {
                action:
                    DiscordAction::ThreadCreate {
                        parent_channel_id,
                        name,
                        message,
                        tag_ids,
                        auto_archive_minutes,
                    },
            } => {
                assert_eq!(parent_channel_id, "123");
                assert_eq!(name, "release-notes");
                assert_eq!(message.as_deref(), Some("Initial forum post"));
                assert!(tag_ids.is_empty());
                assert_eq!(auto_archive_minutes, 1440);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn discord_thread_create_accepts_starter_message_alias_and_text_omission() {
        let alias = Cli::try_parse_from([
            "agentdesk",
            "discord",
            "thread-create",
            "--parent-channel-id",
            "123",
            "--name",
            "forum-post",
            "--starter-message",
            "Starter",
        ])
        .expect("thread-create --starter-message parses");
        let omitted = Cli::try_parse_from([
            "agentdesk",
            "discord",
            "thread-create",
            "--parent-channel-id",
            "123",
            "--name",
            "text-thread",
        ])
        .expect("text thread without starter message parses");

        match alias.command.expect("alias subcommand") {
            Commands::Discord {
                action: DiscordAction::ThreadCreate { message, .. },
            } => assert_eq!(message.as_deref(), Some("Starter")),
            _ => panic!("unexpected alias command"),
        }
        match omitted.command.expect("omitted subcommand") {
            Commands::Discord {
                action: DiscordAction::ThreadCreate { message, .. },
            } => assert!(message.is_none()),
            _ => panic!("unexpected omitted command"),
        }
    }

    #[test]
    fn discord_thread_create_parses_repeatable_tag_ids_and_rejects_invalid_ids() {
        let cli = Cli::try_parse_from([
            "agentdesk",
            "discord",
            "thread-create",
            "--parent-channel-id",
            "123",
            "--name",
            "tagged-post",
            "--message",
            "Starter",
            "--tag-id",
            "41",
            "--tag-id",
            "42",
        ])
        .expect("repeatable --tag-id parses");

        match cli.command.expect("subcommand") {
            Commands::Discord {
                action: DiscordAction::ThreadCreate { tag_ids, .. },
            } => assert_eq!(tag_ids, vec![41, 42]),
            _ => panic!("unexpected command"),
        }

        assert!(
            Cli::try_parse_from([
                "agentdesk",
                "discord",
                "thread-create",
                "--parent-channel-id",
                "123",
                "--name",
                "tagged-post",
                "--tag-id",
                "not-a-snowflake",
            ])
            .is_err()
        );
    }
}
