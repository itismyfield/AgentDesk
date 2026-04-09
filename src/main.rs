#![recursion_limit = "256"]
mod cli;
mod config;
pub(crate) mod credential;
mod db;
mod dispatch;
mod engine;
mod error;
mod github;
pub(crate) mod kanban;
pub(crate) mod pipeline;
pub(crate) mod receipt;
pub(crate) mod reconcile;
pub(crate) mod runtime;
pub(crate) mod runtime_layout;
mod server;
mod services;
pub(crate) mod supervisor;
mod ui;
mod utils;

#[cfg(test)]
mod integration_tests;

// Re-export for crate-level access (used by services::discord::mod.rs)
pub(crate) use cli::agentdesk_runtime_root;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

// ── Clap CLI definition ──────────────────────────────────────

#[derive(Parser)]
#[command(name = "agentdesk", version = env!("CARGO_PKG_VERSION"), about = "AI agent orchestration platform")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start Discord bot server(s)
    Dcserver {
        /// Bot token (defaults to bot_settings.json or AGENTDESK_TOKEN env)
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
        /// Authentication key hash (optional; falls back to AGENTDESK_TOKEN or bot_settings.json)
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
        /// Authentication key hash (optional; falls back to AGENTDESK_TOKEN or bot_settings.json)
        #[arg(long)]
        key: Option<String>,
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
    Dispatch {
        #[command(subcommand)]
        action: DispatchAction,
    },
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
enum DispatchAction {
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
enum MigrateAction {
    /// Import OpenClaw durable state into AgentDesk
    Openclaw(cli::migrate::OpenClawMigrateArgs),
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Get current runtime config
    Get,
    /// Set runtime config (JSON string)
    Set {
        /// JSON value to set
        json: String,
    },
}

#[derive(Clone, ValueEnum)]
enum ReportProvider {
    Claude,
    Codex,
    Gemini,
    Qwen,
}

#[derive(Clone, ValueEnum)]
#[cfg(unix)]
enum InputModeArg {
    Fifo,
    Pipe,
}

fn exit_for_cli(result: std::result::Result<(), String>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    }
}

fn exit_for_json_cli(result: std::result::Result<(), String>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(_) => std::process::exit(1),
    }
}

fn main() -> Result<()> {
    let parsed = Cli::try_parse();

    match parsed {
        Ok(cli) => match cli.command {
            // ── Legacy commands (migrated from manual parsing) ──
            Some(Commands::Dcserver { token }) => {
                let token = token.or_else(|| std::env::var("AGENTDESK_TOKEN").ok());
                cli::handle_dcserver(token);
                return Ok(());
            }
            Some(Commands::Init) => {
                cli::handle_init(false);
                return Ok(());
            }
            Some(Commands::Reconfigure) => {
                cli::handle_init(true);
                return Ok(());
            }
            Some(Commands::RestartDcserver {
                report_channel_id,
                report_provider,
                report_message_id,
            }) => {
                let report_context = build_restart_report_context(
                    report_channel_id,
                    report_provider,
                    report_message_id,
                );
                match report_context {
                    Ok(ctx) => cli::handle_restart_dcserver(ctx),
                    Err(err) => eprintln!("Error: {err}"),
                }
                return Ok(());
            }
            Some(Commands::DiscordSendfile { path, channel, key }) => {
                cli::handle_discord_sendfile(&path, channel, &key);
                return Ok(());
            }
            Some(Commands::DiscordSendmessage {
                channel,
                message,
                key,
            }) => {
                cli::handle_discord_sendmessage(&message, channel, key.as_deref());
                return Ok(());
            }
            Some(Commands::DiscordSenddm { user, message, key }) => {
                cli::handle_discord_senddm(&message, user, key.as_deref());
                return Ok(());
            }
            #[cfg(unix)]
            Some(Commands::TmuxWrapper {
                output_file,
                input_fifo,
                prompt_file,
                cwd,
                input_mode,
                claude_cmd,
            }) => {
                let mode = match input_mode {
                    InputModeArg::Pipe => services::tmux_wrapper::InputMode::Pipe,
                    InputModeArg::Fifo => services::tmux_wrapper::InputMode::Fifo,
                };
                services::tmux_wrapper::run(
                    &output_file,
                    &input_fifo,
                    &prompt_file,
                    &cwd,
                    &claude_cmd,
                    mode,
                );
                return Ok(());
            }
            #[cfg(unix)]
            Some(Commands::CodexTmuxWrapper {
                output_file,
                input_fifo,
                prompt_file,
                codex_bin,
                codex_model,
                reasoning_effort,
                resume_session_id,
                cwd,
                input_mode,
                compact_token_limit,
            }) => {
                let mode = match input_mode {
                    InputModeArg::Pipe => services::tmux_wrapper::InputMode::Pipe,
                    InputModeArg::Fifo => services::tmux_wrapper::InputMode::Fifo,
                };
                services::codex_tmux_wrapper::run(
                    &output_file,
                    &input_fifo,
                    &prompt_file,
                    &cwd,
                    &codex_bin,
                    codex_model.as_deref(),
                    reasoning_effort.as_deref(),
                    resume_session_id.as_deref(),
                    mode,
                    compact_token_limit,
                );
                return Ok(());
            }
            #[cfg(unix)]
            Some(Commands::QwenTmuxWrapper {
                output_file,
                input_fifo,
                prompt_file,
                qwen_bin,
                qwen_model,
                qwen_core_tools,
                resume_session_id,
                cwd,
                input_mode,
            }) => {
                let mode = match input_mode {
                    InputModeArg::Pipe => services::tmux_wrapper::InputMode::Pipe,
                    InputModeArg::Fifo => services::tmux_wrapper::InputMode::Fifo,
                };
                services::qwen_tmux_wrapper::run(
                    &output_file,
                    &input_fifo,
                    &prompt_file,
                    &cwd,
                    &qwen_bin,
                    qwen_model.as_deref(),
                    &qwen_core_tools,
                    resume_session_id.as_deref(),
                    mode,
                );
                return Ok(());
            }
            Some(Commands::ResetTmux) => {
                cli::utils::handle_reset_tmux();
                return Ok(());
            }
            Some(Commands::Ismcptool { tools }) => {
                cli::utils::handle_ismcptool(&tools);
                return Ok(());
            }
            Some(Commands::Addmcptool { tools }) => {
                cli::utils::handle_addmcptool(&tools);
                return Ok(());
            }
            // ── Client commands ──
            Some(Commands::Status) => {
                return exit_for_cli(cli::client::cmd_status());
            }
            Some(Commands::Cards { status }) => {
                return exit_for_cli(cli::client::cmd_cards(status.as_deref()));
            }
            Some(Commands::Dispatch { action }) => {
                return exit_for_cli(match action {
                    DispatchAction::List => cli::client::cmd_dispatch_list(),
                    DispatchAction::Retry { card_id } => cli::client::cmd_dispatch_retry(&card_id),
                    DispatchAction::Redispatch { card_id } => {
                        cli::client::cmd_dispatch_redispatch(&card_id)
                    }
                });
            }
            Some(Commands::Resume {
                card_id,
                force,
                reason,
            }) => {
                return exit_for_cli(cli::client::cmd_resume(&card_id, force, reason.as_deref()));
            }
            Some(Commands::Agents) => {
                return exit_for_cli(cli::client::cmd_agents());
            }
            Some(Commands::Advance { issue_number }) => {
                return exit_for_cli(cli::client::cmd_advance(&issue_number));
            }
            Some(Commands::Queue) => {
                return exit_for_cli(cli::client::cmd_queue());
            }
            Some(Commands::Deploy) => {
                return exit_for_cli(cli::client::cmd_deploy());
            }
            Some(Commands::Config { action }) => {
                return exit_for_cli(match action {
                    ConfigAction::Get => cli::client::cmd_config_get(),
                    ConfigAction::Set { json } => cli::client::cmd_config_set(&json),
                });
            }
            Some(Commands::Api { method, path, body }) => {
                return exit_for_cli(cli::client::cmd_api(&method, &path, body.as_deref()));
            }
            Some(Commands::Terminations {
                card_id,
                dispatch_id,
                session,
                limit,
            }) => {
                return exit_for_cli(cli::client::cmd_terminations(
                    card_id.as_deref(),
                    dispatch_id.as_deref(),
                    session.as_deref(),
                    limit,
                ));
            }
            Some(Commands::Doctor { fix, json }) => {
                return if json {
                    exit_for_json_cli(cli::doctor::cmd_doctor(fix, json))
                } else {
                    exit_for_cli(cli::doctor::cmd_doctor(fix, json))
                };
            }
            Some(Commands::Migrate { action }) => {
                return exit_for_cli(match action {
                    MigrateAction::Openclaw(args) => cli::migrate::cmd_migrate_openclaw(args),
                });
            }
            None => {
                // No subcommand — fall through to server start
            }
        },
        Err(e) => {
            // --help and --version exit with 0; actual errors exit with 1
            if e.kind() == clap::error::ErrorKind::DisplayHelp
                || e.kind() == clap::error::ErrorKind::DisplayVersion
            {
                e.print().ok();
                std::process::exit(0);
            }
            let has_args = std::env::args().count() > 1;
            if has_args {
                e.print().ok();
                std::process::exit(1);
            }
            // No args — fall through to server start
        }
    }

    // ── Default: start full AgentDesk server ─────────────────
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let directive = "agentdesk=info"
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse tracing directive: {e}"))?;
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_default_env().add_directive(directive))
            .init();

        if let Some(root) = config::runtime_root() {
            crate::runtime_layout::ensure_runtime_layout(&root)
                .map_err(|error| anyhow::anyhow!("Failed to prepare runtime layout: {error}"))?;
        }

        let config = config::load().context("Failed to load config")?;
        let db = db::init(&config).context("Failed to init DB")?;
        services::termination_audit::init_audit_db(db.clone());

        // Load data-driven pipeline definition (#106)
        let pipeline_path = config.policies.dir.join("default-pipeline.yaml");
        if pipeline_path.exists() {
            pipeline::load(&pipeline_path).context("Failed to load pipeline definition")?;
            tracing::info!("Pipeline loaded: {}", pipeline_path.display());
        }

        let engine = engine::PolicyEngine::new(&config, db.clone())
            .context("Failed to init policy engine")?;

        tracing::info!(
            "AgentDesk v{} starting on {}:{}",
            env!("CARGO_PKG_VERSION"),
            config.server.host,
            config.server.port
        );

        server::run(config.clone(), db.clone(), engine.clone(), None)
            .await
            .context("Server error")?;

        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Build RestartReportContext from clap-parsed arguments, falling back to env vars.
fn build_restart_report_context(
    report_channel_id: Option<u64>,
    report_provider: Option<ReportProvider>,
    report_message_id: Option<u64>,
) -> std::result::Result<Option<services::discord::restart_report::RestartReportContext>, String> {
    use services::discord::restart_report::{
        RestartReportContext, restart_report_context_from_env,
    };
    use services::provider::ProviderKind;

    match (report_provider, report_channel_id, report_message_id) {
        (None, None, None) => Ok(restart_report_context_from_env()),
        (None, None, Some(_)) => Err(
            "--report-message-id requires --report-channel-id and --report-provider".to_string(),
        ),
        (Some(_), None, _) => Err("--report-provider requires --report-channel-id".to_string()),
        (None, Some(_), _) => Err("--report-channel-id requires --report-provider".to_string()),
        (Some(provider_arg), Some(channel_id), current_msg_id) => {
            let provider = match provider_arg {
                ReportProvider::Claude => ProviderKind::Claude,
                ReportProvider::Codex => ProviderKind::Codex,
                ReportProvider::Gemini => ProviderKind::Gemini,
                ReportProvider::Qwen => ProviderKind::Qwen,
            };
            Ok(Some(RestartReportContext {
                provider,
                channel_id,
                current_msg_id,
            }))
        }
    }
}
