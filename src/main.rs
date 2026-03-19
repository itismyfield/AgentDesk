mod cli;
mod config;
mod db;
mod dispatch;
mod engine;
mod error;
mod github;
mod server;
mod services;
mod ui;
mod utils;

// Re-export for crate-level access (used by services::discord::mod.rs)
pub(crate) use cli::remotecc_runtime_root;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Check for dcserver mode
    for arg in &args[1..] {
        match arg.as_str() {
            "--dcserver" | "dcserver" => {
                let token = args
                    .iter()
                    .skip_while(|a| a.as_str() != "--dcserver" && a.as_str() != "dcserver")
                    .nth(1)
                    .filter(|a| !a.starts_with('-'))
                    .cloned()
                    .or_else(|| std::env::var("REMOTECC_TOKEN").ok());
                cli::handle_dcserver(token);
                return Ok(());
            }
            "--restart-dcserver" => {
                let start_index = args.iter().position(|a| a == "--restart-dcserver").unwrap() + 1;
                match cli::parse_restart_dcserver_report_context(&args, start_index) {
                    Ok(report_context) => cli::handle_restart_dcserver(report_context),
                    Err(err) => eprintln!("Error: {err}"),
                }
                return Ok(());
            }
            "--discord-sendfile" => {
                let mut file_path: Option<String> = None;
                let mut channel_id: Option<u64> = None;
                let mut key: Option<String> = None;
                let mut j = args.iter().position(|a| a == "--discord-sendfile").unwrap() + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--channel" => { channel_id = args.get(j + 1).and_then(|v| v.parse().ok()); j += 2; }
                        "--key" => { key = args.get(j + 1).cloned(); j += 2; }
                        _ if file_path.is_none() && !args[j].starts_with("--") => { file_path = Some(args[j].clone()); j += 1; }
                        _ => { j += 1; }
                    }
                }
                match (file_path, channel_id, key) {
                    (Some(fp), Some(cid), Some(k)) => cli::handle_discord_sendfile(&fp, cid, &k),
                    _ => eprintln!("Error: --discord-sendfile requires <PATH>, --channel <ID>, and --key <HASH>"),
                }
                return Ok(());
            }
            "--discord-sendmessage" => {
                let mut message: Option<String> = None;
                let mut channel_id: Option<u64> = None;
                let mut key: Option<String> = None;
                let mut j = args.iter().position(|a| a == "--discord-sendmessage").unwrap() + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--channel" => { channel_id = args.get(j + 1).and_then(|v| v.parse().ok()); j += 2; }
                        "--message" => { message = args.get(j + 1).cloned(); j += 2; }
                        "--key" => { key = args.get(j + 1).cloned(); j += 2; }
                        _ => { j += 1; }
                    }
                }
                match (message, channel_id) {
                    (Some(msg), Some(cid)) => cli::handle_discord_sendmessage(&msg, cid, key.as_deref()),
                    _ => eprintln!("Error: --discord-sendmessage requires --channel <ID> and --message <TEXT>"),
                }
                return Ok(());
            }
            "--discord-senddm" => {
                let mut message: Option<String> = None;
                let mut user_id: Option<u64> = None;
                let mut key: Option<String> = None;
                let mut j = args.iter().position(|a| a == "--discord-senddm").unwrap() + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--user" => { user_id = args.get(j + 1).and_then(|v| v.parse().ok()); j += 2; }
                        "--message" => { message = args.get(j + 1).cloned(); j += 2; }
                        "--key" => { key = args.get(j + 1).cloned(); j += 2; }
                        _ => { j += 1; }
                    }
                }
                match (message, user_id) {
                    (Some(msg), Some(uid)) => cli::handle_discord_senddm(&msg, uid, key.as_deref()),
                    _ => eprintln!("Error: --discord-senddm requires --user <ID> and --message <TEXT>"),
                }
                return Ok(());
            }
            "--tmux-wrapper" => {
                let i = args.iter().position(|a| a == "--tmux-wrapper").unwrap();
                let mut output_file: Option<String> = None;
                let mut input_fifo: Option<String> = None;
                let mut prompt_file: Option<String> = None;
                let mut cwd: Option<String> = None;
                let mut input_mode = services::tmux_wrapper::InputMode::Fifo;
                let mut claude_cmd: Vec<String> = Vec::new();
                let mut j = i + 1;
                let mut after_separator = false;
                while j < args.len() {
                    if after_separator { claude_cmd.push(args[j].clone()); j += 1; continue; }
                    match args[j].as_str() {
                        "--" => { after_separator = true; j += 1; }
                        "--output-file" => { output_file = args.get(j + 1).cloned(); j += 2; }
                        "--input-fifo" => { input_fifo = args.get(j + 1).cloned(); j += 2; }
                        "--prompt-file" => { prompt_file = args.get(j + 1).cloned(); j += 2; }
                        "--cwd" => { cwd = args.get(j + 1).cloned(); j += 2; }
                        "--input-mode" => {
                            if let Some(mode) = args.get(j + 1) {
                                input_mode = match mode.as_str() {
                                    "pipe" => services::tmux_wrapper::InputMode::Pipe,
                                    _ => services::tmux_wrapper::InputMode::Fifo,
                                };
                            }
                            j += 2;
                        }
                        _ => { j += 1; }
                    }
                }
                match (output_file, input_fifo, prompt_file) {
                    (Some(of), Some(inf), Some(pf)) => {
                        let wd = cwd.unwrap_or_else(|| ".".to_string());
                        services::tmux_wrapper::run(&of, &inf, &pf, &wd, &claude_cmd, input_mode);
                    }
                    _ => eprintln!("Error: --tmux-wrapper requires --output-file, --input-fifo, and --prompt-file"),
                }
                return Ok(());
            }
            "--codex-tmux-wrapper" => {
                let i = args.iter().position(|a| a == "--codex-tmux-wrapper").unwrap();
                let mut output_file: Option<String> = None;
                let mut input_fifo: Option<String> = None;
                let mut prompt_file: Option<String> = None;
                let mut cwd: Option<String> = None;
                let mut codex_bin: Option<String> = None;
                let mut input_mode = services::tmux_wrapper::InputMode::Fifo;
                let mut j = i + 1;
                while j < args.len() {
                    match args[j].as_str() {
                        "--output-file" => { output_file = args.get(j + 1).cloned(); j += 2; }
                        "--input-fifo" => { input_fifo = args.get(j + 1).cloned(); j += 2; }
                        "--prompt-file" => { prompt_file = args.get(j + 1).cloned(); j += 2; }
                        "--cwd" => { cwd = args.get(j + 1).cloned(); j += 2; }
                        "--codex-bin" => { codex_bin = args.get(j + 1).cloned(); j += 2; }
                        "--input-mode" => {
                            if let Some(mode) = args.get(j + 1) {
                                input_mode = match mode.as_str() {
                                    "pipe" => services::tmux_wrapper::InputMode::Pipe,
                                    _ => services::tmux_wrapper::InputMode::Fifo,
                                };
                            }
                            j += 2;
                        }
                        _ => { j += 1; }
                    }
                }
                match (output_file, input_fifo, prompt_file, codex_bin) {
                    (Some(of), Some(inf), Some(pf), Some(bin)) => {
                        let wd = cwd.unwrap_or_else(|| ".".to_string());
                        services::codex_tmux_wrapper::run(&of, &inf, &pf, &wd, &bin, input_mode);
                    }
                    _ => eprintln!("Error: --codex-tmux-wrapper requires --output-file, --input-fifo, --prompt-file, and --codex-bin"),
                }
                return Ok(());
            }
            _ => {}
        }
    }

    // Default: start full AgentDesk server
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("agentdesk=info".parse()?))
        .init();

    let config = config::load()?;
    let db = db::init(&config)?;
    let engine = engine::PolicyEngine::new(&config, db.clone())?;

    tracing::info!(
        "AgentDesk v{} starting on {}:{}",
        env!("CARGO_PKG_VERSION"),
        config.server.host,
        config.server.port
    );

    // Start subsystems concurrently
    tokio::try_join!(
        server::run(config.clone(), db.clone(), engine.clone()),
    )?;

    Ok(())
}
