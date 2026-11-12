use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

pub(crate) use super::migrate::MigrateAction;

mod command_args;
pub(crate) use command_args::*;

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
    Dcserver(DcserverArgs),
    /// Write an isolated instance credential from stdin
    DevInstance(super::dev_instance::DevInstanceArgs),
    /// Run the initial setup wizard
    Init,
    /// Re-run the configuration wizard
    Reconfigure,
    /// Emit a launchd plist using the canonical Rust renderer
    EmitLaunchdPlist(EmitLaunchdPlistArgs),
    /// Restart Discord bot server(s)
    RestartDcserver(RestartDcserverArgs),
    /// Deprecated: send a file to a Discord channel
    #[command(
        after_help = "Deprecated compatibility command. The routed `send` command does not currently accept file attachments, so keep using this command until an attachment-capable replacement is available."
    )]
    DiscordSendfile(DiscordSendfileArgs),
    /// Deprecated: send a message to a Discord channel
    #[command(
        after_help = "Deprecated compatibility command. With `--key`, this command selects that one configured bot token; without `--key`, it tries configured bot tokens sequentially until one succeeds. For a role-mapped channel with a configured bot runtime, migrate to `agentdesk send --target channel:<id> --content <TEXT>`. Routed `send` instead requires role-map authorization and selects one `--bot` runtime (default: announce), so it does not preserve this command's token selection or sequential fallback. Keep using this compatibility command when those routing requirements do not match the current delivery."
    )]
    DiscordSendmessage(DiscordSendmessageArgs),
    /// Deprecated: send a direct message to a Discord user
    #[command(
        after_help = "Deprecated compatibility command. With `--key`, this command selects that one configured bot token; without `--key`, it tries configured bot tokens sequentially until one succeeds. Routed `send` instead requires a role-map-authorized channel or agent target and selects one configured `--bot` runtime (default: announce); it does not preserve this token fallback, target a Discord user, or create a DM. Keep using this command until a supported replacement is available."
    )]
    DiscordSenddm(DiscordSenddmArgs),
    /// Send a routed Discord message without HTTP server dependency
    Send(SendArgs),
    /// Send a trigger-capable agent handoff through the announce bot
    SendToAgent(SendToAgentArgs),
    /// Submit a review verdict without HTTP server dependency
    ReviewVerdict(ReviewVerdictArgs),
    /// Submit a review decision without HTTP server dependency
    ReviewDecision(ReviewDecisionArgs),
    /// Recover a review dispatch target commit/worktree through the API
    ReviewRecoverTarget(ReviewRecoverTargetArgs),
    /// Read API documentation without HTTP server dependency
    Docs(DocsArgs),
    /// Auto-queue operations without HTTP server dependency
    AutoQueue {
        #[command(subcommand)]
        action: AutoQueueAction,
    },
    /// Inspect or explicitly release one turn lease while preserving its session
    TurnLease(super::turn_lease::TurnLeaseArgs),
    /// Force-kill a session without HTTP server dependency
    ForceKill(ForceKillArgs),
    /// Sync GitHub issues without HTTP server dependency
    GithubSync(GithubSyncArgs),
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
    CherryMerge(CherryMergeArgs),
    /// Claude session wrapper (tmux FIFO on Unix, process pipe on all platforms)
    TmuxWrapper(TmuxWrapperArgs),
    /// Codex session wrapper (tmux FIFO on Unix, process pipe on all platforms)
    CodexTmuxWrapper(CodexTmuxWrapperArgs),
    /// Qwen session wrapper (tmux FIFO on Unix, process pipe on all platforms)
    QwenTmuxWrapper(QwenTmuxWrapperArgs),
    /// Relay Claude Code hook stdin JSON to the AgentDesk TUI hook receiver
    ClaudeHookRelay(ClaudeHookRelayArgs),
    /// Relay Codex hook stdin JSON to the AgentDesk TUI hook receiver
    CodexHookRelay(CodexHookRelayArgs),
    /// Kill all AgentDesk-* tmux sessions and clean temp files
    ResetTmux,
    /// Check if MCP tool(s) are registered in .claude/settings.json
    Ismcptool(IsmcptoolArgs),
    /// Add MCP tool permission(s) to .claude/settings.json
    Addmcptool(AddmcptoolArgs),
    /// Show server health, active sessions, and auto-queue status
    Status,
    /// List kanban cards
    Cards(CardsArgs),
    /// Dispatch operations
    Dispatch(DispatchArgs),
    /// Resume a stuck kanban card from its current state
    Resume(ResumeArgs),
    /// Complete pending dispatch and advance to review
    Advance(AdvanceArgs),
    /// Show auto-queue status with thread links
    #[command(
        after_help = "Deprecated compatibility command. For the queue-only inspector, use `agentdesk query queue`; use `agentdesk query` for the combined queue, dispatch, and phase-gate snapshot. This command preserves its legacy text and JSON output shapes."
    )]
    Queue,
    /// Unified queue + dispatch + phase-gate inspection (issue #2651).
    ///
    /// First-class replacement for the ad-hoc `curl /api/queue/status |
    /// python -c '...'` polling pattern. Aggregates the queue run state,
    /// pending dispatches, and phase-gate snapshot into one structured
    /// response. Pass `--json` for machine-readable output.
    Query(QueryArgs),
    /// Phase-gate violation snapshot (issue #2657).
    ///
    /// Read-only inspector that flags auto-queue entries dispatched at a
    /// higher `batch_phase` than the run's live phase pointer. Same data as
    /// `GET /api/queue/phase-gates/violations`.
    Phase(PhaseArgs),
    /// Build + deploy dev + promote to release
    Deploy,
    /// Print this binary's build switches as JSON (read by deploy scripts)
    BuildInfo,
    /// List agents and their status
    Agents,
    /// Show turn/session diagnostics for an agent ID or Discord channel ID
    Diag(DiagArgs),
    /// Runtime config get/set
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Call any API endpoint (curl replacement)
    Api(ApiArgs),
    /// List session termination events
    Terminations(TerminationsArgs),
    /// Environment diagnostics
    Doctor(DoctorArgs),
    /// Migration helpers
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
    /// Apply pending PostgreSQL migrations before release activation
    #[command(hide = true)]
    ReleaseMigratePostgres,
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
    Activity(ActivityArgs),
    /// Herdr-hosted executions on this node: read-only status, and retire of an ended one
    Herdr(crate::cli::herdr::HerdrArgs),
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
    /// List dispatched rows and their recorded audit facts without changing them.
    DispatchedAudit,
    /// Settle a dispatched handoff as unknown without creating a retry child that could redeliver an already delivered turn.
    DispatchedSettle {
        id: i64,
        #[arg(long, default_value = "operator dispatched-settle via CLI")]
        reason: String,
    },
    /// Settle a spawned handoff as unknown without creating a retry child that could redeliver an already delivered turn.
    SpawnedSettle {
        id: i64,
        #[arg(long, default_value = "operator spawned-settle via CLI")]
        reason: String,
    },
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

#[derive(Clone, Default, ValueEnum)]
pub(crate) enum InputModeArg {
    #[cfg(unix)]
    #[default]
    Fifo,
    #[cfg_attr(not(unix), default)]
    Pipe,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum FastModeStateArg {
    Enabled,
    Disabled,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum FeatureStateArg {
    Enabled,
    Disabled,
}

#[allow(clippy::large_enum_variant)]
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
                super::dev_instance::reject_argument_error();
                error.print().ok();
                std::process::exit(1);
            }
            ParseOutcome::RunServer
        }
    }
}

#[cfg(test)]
#[path = "removed_channel_home_tests.rs"]
mod removed_channel_home_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser, error::ErrorKind};

    #[test]
    fn cli_parse_fits_in_the_windows_default_main_thread_stack() {
        // Windows gives the main thread 1 MiB, and `run_from_args` parses there before any command runs.
        let parsed = std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| Cli::try_parse_from(["agentdesk", "config", "audit", "--dry-run"]).is_ok())
            .expect("spawn the 1 MiB parser thread")
            .join();
        assert!(matches!(parsed, Ok(true)));
    }

    #[test]
    fn turn_lease_cli_parses_read_and_exact_release() {
        let read = Cli::try_parse_from([
            "agentdesk",
            "turn-lease",
            "inspect",
            "--provider",
            "codex",
            "--channel-id",
            "5754",
        ])
        .unwrap();
        assert!(matches!(read.command, Some(Commands::TurnLease(_))));
        let release = Cli::try_parse_from([
            "agentdesk",
            "--json",
            "turn-lease",
            "release",
            "--expected",
            "{}",
            "--reason",
            "operator verified turn ended",
        ])
        .unwrap();
        assert!(release.json);
        assert!(matches!(release.command, Some(Commands::TurnLease(_))));
    }

    #[test]
    fn turn_lease_cli_rejects_unpinned_release_and_zero_channel() {
        assert!(
            Cli::try_parse_from(["agentdesk", "turn-lease", "release", "--reason", "verified"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["agentdesk", "turn-lease", "release", "--expected", "{}"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "agentdesk",
                "turn-lease",
                "inspect",
                "--provider",
                "codex",
                "--channel-id",
                "0"
            ])
            .is_err()
        );
    }

    #[test]
    fn provider_wrappers_accept_process_backend_pipe_arguments() {
        for (wrapper, provider_args) in [
            ("tmux-wrapper", vec!["--", "claude"]),
            ("codex-tmux-wrapper", vec!["--codex-bin", "codex"]),
            ("qwen-tmux-wrapper", vec!["--qwen-bin", "qwen"]),
        ] {
            let mut args = vec![
                "agentdesk",
                wrapper,
                "--output-file",
                "output.jsonl",
                "--input-fifo",
                "unused-fifo",
                "--prompt-file",
                "prompt.txt",
                "--cwd",
                ".",
                "--input-mode",
                "pipe",
            ];
            args.extend(provider_args);
            Cli::try_parse_from(args).expect("ProcessBackend command is available on every OS");
        }
    }

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
            "turn-lease",
            "force-kill",
            "github-sync",
            "monitoring",
            "intake-outbox",
            "discord",
            "card",
            "cherry-merge",
            "tmux-wrapper",
            "codex-tmux-wrapper",
            "qwen-tmux-wrapper",
            "claude-hook-relay",
            "codex-hook-relay",
            "reset-tmux",
            "ismcptool",
            "addmcptool",
            "status",
            "cards",
            "dispatch",
            "resume",
            "advance",
            "queue",
            "query",
            "phase",
            "deploy",
            "build-info",
            "agents",
            "diag",
            "config",
            "api",
            "terminations",
            "doctor",
            "migrate",
            "release-migrate-postgres",
            "provider-cli",
            "show",
            "health",
            "machine-compare",
            "activity",
            "herdr",
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
            Some(Commands::DiscordSendfile(DiscordSendfileArgs {
                path,
                channel: 42,
                key,
            })) if path == "/tmp/report.txt" && key == "key-hash"
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
            Some(Commands::DiscordSendmessage(DiscordSendmessageArgs {
                channel: 42,
                message,
                key: None,
            })) if message == "hello channel"
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
            Some(Commands::DiscordSenddm(DiscordSenddmArgs {
                user: 43,
                message,
                key: None,
            })) if message == "hello user"
        ));
    }

    #[test]
    fn legacy_queue_help_directs_users_to_query_without_changing_compatibility_contract() {
        let Err(error) = Cli::try_parse_from(["agentdesk", "queue", "--help"]) else {
            panic!("queue --help must render help instead of running the command");
        };
        assert_eq!(error.kind(), ErrorKind::DisplayHelp);
        assert_eq!(error.exit_code(), 0);

        let help = error
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(help.contains("Deprecated compatibility command."));
        assert!(help.contains("`agentdesk query queue`"));
        assert!(help.contains("`agentdesk query`"));
        assert!(help.contains("legacy text and JSON output shapes"));
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
            Some(Commands::RestartDcserver(RestartDcserverArgs {
                report_provider: Some(ReportProvider::OpenCode),
                ..
            }))
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
            Commands::SendToAgent(SendToAgentArgs { expect_reply, .. }) => assert!(expect_reply),
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
            Commands::SendToAgent(SendToAgentArgs { expect_reply, .. }) => assert!(!expect_reply),
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
