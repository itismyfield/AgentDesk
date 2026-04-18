use anyhow::Result;

use super::args::{
    AutoQueueAction, CardAction, Commands, ConfigAction, DispatchAction, MigrateAction,
    ReportProvider,
};

fn exit_for_cli(result: std::result::Result<(), String>) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("Error: {error}");
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

pub(crate) fn execute(command: Commands) -> Result<()> {
    match command {
        Commands::Dcserver { token } => {
            let token = token.or_else(|| std::env::var("AGENTDESK_TOKEN").ok());
            super::handle_dcserver(token);
            Ok(())
        }
        Commands::Init => {
            super::handle_init(false);
            Ok(())
        }
        Commands::Reconfigure => {
            super::handle_init(true);
            Ok(())
        }
        Commands::RestartDcserver {
            report_channel_id,
            report_provider,
            report_message_id,
        } => {
            match build_restart_report_context(
                report_channel_id,
                report_provider,
                report_message_id,
            ) {
                Ok(context) => super::handle_restart_dcserver(context),
                Err(error) => eprintln!("Error: {error}"),
            }
            Ok(())
        }
        Commands::DiscordSendfile { path, channel, key } => {
            super::handle_discord_sendfile(&path, channel, &key);
            Ok(())
        }
        Commands::DiscordSendmessage {
            channel,
            message,
            key,
        } => {
            super::handle_discord_sendmessage(&message, channel, key.as_deref());
            Ok(())
        }
        Commands::DiscordSenddm { user, message, key } => {
            super::handle_discord_senddm(&message, user, key.as_deref());
            Ok(())
        }
        Commands::Send {
            target,
            source,
            bot,
            content,
        } => exit_for_cli(super::direct::run_async(super::direct::cmd_send(
            &target,
            source.as_deref(),
            bot.as_deref(),
            &content,
        ))),
        Commands::ReviewVerdict {
            dispatch_id,
            verdict,
            notes,
            feedback,
            provider,
            commit,
        } => exit_for_cli(super::direct::run_async(super::direct::cmd_review_verdict(
            &dispatch_id,
            &verdict,
            notes.as_deref(),
            feedback.as_deref(),
            provider.as_deref(),
            commit.as_deref(),
        ))),
        Commands::ReviewDecision {
            card_id,
            decision,
            comment,
            dispatch_id,
        } => exit_for_cli(super::direct::run_async(
            super::direct::cmd_review_decision(
                &card_id,
                &decision,
                comment.as_deref(),
                dispatch_id.as_deref(),
            ),
        )),
        Commands::Docs { category, flat } => exit_for_cli(super::direct::run_async(
            super::direct::cmd_docs(category.as_deref(), flat),
        )),
        Commands::AutoQueue { action } => exit_for_cli(match action {
            AutoQueueAction::Activate {
                run_id,
                agent_id,
                repo,
                active_only,
            } => super::direct::run_async(super::direct::cmd_auto_queue_activate(
                run_id.as_deref(),
                agent_id.as_deref(),
                repo.as_deref(),
                active_only,
            )),
            AutoQueueAction::Add {
                card_id,
                run_id,
                priority,
                phase,
                thread_group,
                agent_id,
            } => super::direct::run_async(super::direct::cmd_auto_queue_add(
                &card_id,
                run_id.as_deref(),
                priority,
                phase,
                thread_group,
                agent_id.as_deref(),
            )),
            AutoQueueAction::Config {
                run_id,
                repo,
                agent_id,
                max_concurrent_threads,
            } => super::direct::run_async(super::direct::cmd_auto_queue_config(
                run_id.as_deref(),
                repo.as_deref(),
                agent_id.as_deref(),
                max_concurrent_threads,
            )),
        }),
        Commands::ForceKill { session_key, retry } => exit_for_cli(super::direct::run_async(
            super::direct::cmd_force_kill(&session_key, retry),
        )),
        Commands::GithubSync { repo } => exit_for_cli(super::direct::run_async(
            super::direct::cmd_github_sync(repo.as_deref()),
        )),
        Commands::Discord { action } => exit_for_cli(match action {
            super::args::DiscordAction::Read {
                channel_id,
                limit,
                before,
                after,
            } => super::direct::run_async(super::direct::cmd_discord_read(
                &channel_id,
                limit,
                before.as_deref(),
                after.as_deref(),
            )),
        }),
        Commands::Card { action } => exit_for_cli(match action {
            CardAction::Create {
                issue_number,
                repo,
                status,
                agent_id,
            } => super::direct::run_async(super::direct::cmd_card_create_from_issue(
                issue_number,
                repo.as_deref(),
                status.as_deref(),
                agent_id.as_deref(),
            )),
            CardAction::Status { card_ref, repo } => {
                super::direct::run_async(super::direct::cmd_card_status(&card_ref, repo.as_deref()))
            }
        }),
        Commands::CherryMerge {
            branch,
            close_issue,
        } => exit_for_cli(super::direct::cmd_cherry_merge(&branch, close_issue)),
        #[cfg(unix)]
        Commands::TmuxWrapper {
            output_file,
            input_fifo,
            prompt_file,
            cwd,
            input_mode,
            claude_cmd,
        } => {
            let mode = match input_mode {
                super::args::InputModeArg::Pipe => crate::services::tmux_wrapper::InputMode::Pipe,
                super::args::InputModeArg::Fifo => crate::services::tmux_wrapper::InputMode::Fifo,
            };
            crate::services::tmux_wrapper::run(
                &output_file,
                &input_fifo,
                &prompt_file,
                &cwd,
                &claude_cmd,
                mode,
            );
            Ok(())
        }
        #[cfg(unix)]
        Commands::CodexTmuxWrapper {
            output_file,
            input_fifo,
            prompt_file,
            codex_bin,
            codex_model,
            reasoning_effort,
            resume_session_id,
            fast_mode_state,
            cwd,
            input_mode,
            compact_token_limit,
        } => {
            let mode = match input_mode {
                super::args::InputModeArg::Pipe => crate::services::tmux_wrapper::InputMode::Pipe,
                super::args::InputModeArg::Fifo => crate::services::tmux_wrapper::InputMode::Fifo,
            };
            let fast_mode_override = fast_mode_state.map(|state| match state {
                super::args::FastModeStateArg::Enabled => true,
                super::args::FastModeStateArg::Disabled => false,
            });
            crate::services::codex_tmux_wrapper::run(
                &output_file,
                &input_fifo,
                &prompt_file,
                &cwd,
                &codex_bin,
                codex_model.as_deref(),
                reasoning_effort.as_deref(),
                resume_session_id.as_deref(),
                fast_mode_override,
                mode,
                compact_token_limit,
            );
            Ok(())
        }
        #[cfg(unix)]
        Commands::QwenTmuxWrapper {
            output_file,
            input_fifo,
            prompt_file,
            qwen_bin,
            qwen_model,
            qwen_core_tools,
            resume_session_id,
            cwd,
            input_mode,
        } => {
            let mode = match input_mode {
                super::args::InputModeArg::Pipe => crate::services::tmux_wrapper::InputMode::Pipe,
                super::args::InputModeArg::Fifo => crate::services::tmux_wrapper::InputMode::Fifo,
            };
            crate::services::qwen_tmux_wrapper::run(
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
            Ok(())
        }
        Commands::ResetTmux => {
            super::utils::handle_reset_tmux();
            Ok(())
        }
        Commands::Ismcptool { tools } => {
            super::utils::handle_ismcptool(&tools);
            Ok(())
        }
        Commands::Addmcptool { tools } => {
            super::utils::handle_addmcptool(&tools);
            Ok(())
        }
        Commands::Status => exit_for_cli(super::client::cmd_status()),
        Commands::Cards { status } => exit_for_cli(super::client::cmd_cards(status.as_deref())),
        Commands::Dispatch(args) => exit_for_cli(match args.action {
            Some(DispatchAction::List) => super::client::cmd_dispatch_list(),
            Some(DispatchAction::Retry { card_id }) => {
                super::direct::run_async(super::direct::cmd_dispatch_retry(&card_id))
            }
            Some(DispatchAction::Redispatch { card_id }) => {
                super::direct::run_async(super::direct::cmd_dispatch_redispatch(&card_id))
            }
            None => super::client::cmd_dispatch(
                &args.issue_groups,
                args.repo.as_deref(),
                args.agent_id.as_deref(),
                args.unified,
                args.concurrent,
                !args.no_activate,
            ),
        }),
        Commands::Resume {
            card_id,
            force,
            reason,
        } => exit_for_cli(super::client::cmd_resume(
            &card_id,
            force,
            reason.as_deref(),
        )),
        Commands::Agents => exit_for_cli(super::client::cmd_agents()),
        Commands::Advance { issue_number } => {
            exit_for_cli(super::client::cmd_advance(&issue_number))
        }
        Commands::Queue => exit_for_cli(super::client::cmd_queue()),
        Commands::Deploy => exit_for_cli(super::client::cmd_deploy()),
        Commands::Config { action } => exit_for_cli(match action {
            ConfigAction::Get => super::client::cmd_config_get(),
            ConfigAction::Set { json } => super::client::cmd_config_set(&json),
            ConfigAction::Audit { dry_run } => super::client::cmd_config_audit(dry_run),
        }),
        Commands::Api { method, path, body } => {
            exit_for_cli(super::client::cmd_api(&method, &path, body.as_deref()))
        }
        Commands::Terminations {
            card_id,
            dispatch_id,
            session,
            limit,
        } => exit_for_cli(super::client::cmd_terminations(
            card_id.as_deref(),
            dispatch_id.as_deref(),
            session.as_deref(),
            limit,
        )),
        Commands::Doctor { fix, json } => {
            if json {
                exit_for_json_cli(super::doctor::cmd_doctor(fix, json))
            } else {
                exit_for_cli(super::doctor::cmd_doctor(fix, json))
            }
        }
        Commands::Migrate { action } => exit_for_cli(match action {
            MigrateAction::Openclaw(args) => super::migrate::cmd_migrate_openclaw(args),
            MigrateAction::PostgresCutover(args) => {
                super::direct::run_async(super::migrate::cmd_migrate_postgres_cutover(args))
            }
        }),
    }
}

fn build_restart_report_context(
    report_channel_id: Option<u64>,
    report_provider: Option<ReportProvider>,
    report_message_id: Option<u64>,
) -> std::result::Result<
    Option<crate::services::discord::restart_report::RestartReportContext>,
    String,
> {
    use crate::services::discord::restart_report::{
        RestartReportContext, restart_report_context_from_env,
    };
    use crate::services::provider::ProviderKind;

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
