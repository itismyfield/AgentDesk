use anyhow::Result;

use super::args::{
    AgentHandoffChannelKindArg, AutoQueueAction, CardAction, Commands, ConfigAction,
    DispatchAction, DoctorProfileArg, IntakeOutboxAction, MigrateAction, MonitoringAction,
    PhaseAction, QueryAction, ReportProvider, ShowAction,
};

fn agent_handoff_channel_kind(
    value: AgentHandoffChannelKindArg,
) -> crate::services::discord::agent_handoff::AgentHandoffChannelKind {
    match value {
        AgentHandoffChannelKindArg::Cc => {
            crate::services::discord::agent_handoff::AgentHandoffChannelKind::Cc
        }
        AgentHandoffChannelKindArg::Cdx => {
            crate::services::discord::agent_handoff::AgentHandoffChannelKind::Cdx
        }
    }
}

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
        Err(error) => {
            // stdout stays pure so piped JSON consumers see clean output; the
            // failure is reported as one line of error JSON on stderr instead.
            eprintln!("{}", json_cli_error_line(&error));
            std::process::exit(1);
        }
    }
}

/// Build the single-line `{"error":"…"}` string emitted to stderr when a
/// JSON-mode CLI command fails. Pure (the caller then does `process::exit`,
/// which is not directly testable) and serde-serialized so quotes/newlines in
/// the message — e.g. the multi-line connection hint — are escaped into one
/// physical line.
fn json_cli_error_line(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// Whether `command` has a JSON output path — either it always emits JSON
/// (the JSON-only commands and every `direct::run_command` action, which print
/// via `print_json`) or it accepts a `--json` toggle (the dual-mode commands).
/// Text-only / operational commands return `false`, and [`execute`] rejects a
/// global `--json` for them (#4372 r2) instead of silently emitting text with
/// no machine-readable signal.
///
/// The match is intentionally exhaustive with **no wildcard**: adding a new
/// `Commands` variant will fail to compile here until it is classified, so the
/// flag's acceptance can never silently drift out of sync with its support.
fn command_supports_json(command: &Commands) -> bool {
    match command {
        // JSON emitters (always print structured JSON) + dual-mode commands
        // that honour a `--json` toggle.
        Commands::Send { .. }
        | Commands::SendToAgent { .. }
        | Commands::ReviewVerdict { .. }
        | Commands::ReviewDecision { .. }
        | Commands::ReviewRecoverTarget { .. }
        | Commands::Docs { .. }
        | Commands::AutoQueue { .. }
        | Commands::ForceKill { .. }
        | Commands::GithubSync { .. }
        | Commands::Monitoring { .. }
        | Commands::Discord { .. }
        | Commands::Card { .. }
        | Commands::CherryMerge { .. }
        | Commands::Status
        | Commands::Cards { .. }
        | Commands::Dispatch(..)
        | Commands::Resume { .. }
        | Commands::Advance { .. }
        | Commands::Queue
        | Commands::Query { .. }
        | Commands::Phase { .. }
        | Commands::Agents
        | Commands::Diag { .. }
        | Commands::Config { .. }
        | Commands::Api { .. }
        | Commands::Terminations { .. }
        | Commands::Doctor { .. }
        | Commands::ProviderCli(..)
        | Commands::Health
        | Commands::MachineCompare
        | Commands::Activity { .. } => true,

        // Text-only / operational commands — no JSON output path.
        Commands::Dcserver { .. }
        | Commands::Init
        | Commands::Reconfigure
        | Commands::EmitLaunchdPlist(..)
        | Commands::RestartDcserver { .. }
        | Commands::DiscordSendfile { .. }
        | Commands::DiscordSendmessage { .. }
        | Commands::DiscordSenddm { .. }
        | Commands::IntakeOutbox { .. }
        | Commands::ClaudeHookRelay { .. }
        | Commands::CodexHookRelay { .. }
        | Commands::ResetTmux
        | Commands::Ismcptool { .. }
        | Commands::Addmcptool { .. }
        | Commands::InstallMementoSessionHook { .. }
        | Commands::Deploy
        | Commands::Migrate { .. }
        | Commands::Show { .. } => false,

        #[cfg(unix)]
        Commands::TmuxWrapper { .. }
        | Commands::CodexTmuxWrapper { .. }
        | Commands::QwenTmuxWrapper { .. } => false,
    }
}

pub(crate) fn execute(command: Commands, json: bool) -> Result<()> {
    // Global `--json` is accepted by clap on every command, so reject it up
    // front for commands that have no JSON output path — otherwise a script
    // passing `--json` would get plain text and exit 0 with no signal (#4372
    // r2). Reported as `{"error":…}` on stderr + nonzero exit, matching the
    // JSON-mode failure contract.
    if json && !command_supports_json(&command) {
        return exit_for_json_cli(Err(
            "--json is not supported by this command: it has no JSON output path. \
             Re-run without --json."
                .to_string(),
        ));
    }
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
        Commands::EmitLaunchdPlist(args) => {
            exit_for_cli(super::init::handle_emit_launchd_plist(&args))
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
        Commands::SendToAgent {
            from_agent_id,
            to_agent_id,
            message,
            channel_kind,
            no_prefix,
            expect_reply,
            start_turn,
        } => exit_for_cli(super::direct::run_async(super::direct::cmd_send_to_agent(
            &from_agent_id,
            &to_agent_id,
            &message,
            agent_handoff_channel_kind(channel_kind),
            !no_prefix,
            expect_reply,
            start_turn,
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
        Commands::ReviewRecoverTarget {
            dispatch_id,
            card_id,
            target_commit,
            worktree_path,
            reason,
        } => exit_for_cli(super::client::cmd_review_recover_target(
            dispatch_id.as_deref(),
            card_id.as_deref(),
            target_commit.as_deref(),
            worktree_path.as_deref(),
            reason.as_deref(),
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
        Commands::Diag { identifier } => exit_for_cli(super::client::cmd_diag(&identifier, json)),
        Commands::GithubSync { repo } => exit_for_cli(super::direct::run_async(
            super::direct::cmd_github_sync(repo.as_deref()),
        )),
        Commands::Monitoring { action } => exit_for_cli(match action {
            MonitoringAction::Start {
                channel,
                key,
                description,
            } => super::monitoring::start(channel, &key, &description),
            MonitoringAction::Stop { channel, key } => super::monitoring::stop(channel, &key),
        }),
        Commands::IntakeOutbox { action } => exit_for_cli(match action {
            IntakeOutboxAction::Status { channel_id, limit } => super::direct::run_async(
                super::direct::cmd_intake_outbox_status(channel_id.as_deref(), limit),
            ),
            IntakeOutboxAction::ForceFail { id, reason } => {
                super::direct::run_async(super::direct::cmd_intake_outbox_force_fail(id, &reason))
            }
        }),
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
            super::args::DiscordAction::CategoryCreate { name, guild_id } => {
                super::direct::run_async(super::direct::cmd_discord_category_create(
                    &name,
                    guild_id.as_deref(),
                ))
            }
            super::args::DiscordAction::ChannelCreate {
                name,
                category_id,
                topic,
                guild_id,
            } => super::direct::run_async(super::direct::cmd_discord_channel_create(
                &name,
                category_id.as_deref(),
                topic.as_deref(),
                guild_id.as_deref(),
            )),
            super::args::DiscordAction::ThreadCreate {
                parent_channel_id,
                name,
                message,
                tag_ids,
                auto_archive_minutes,
            } => super::direct::run_async(super::direct::cmd_discord_thread_create(
                &parent_channel_id,
                &name,
                message.as_deref(),
                &tag_ids,
                auto_archive_minutes,
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
            developer_instructions,
            resume_session_id,
            fast_mode_state,
            goals_state,
            cwd,
            add_dirs,
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
            let goals_override = goals_state.map(|state| match state {
                super::args::FeatureStateArg::Enabled => true,
                super::args::FeatureStateArg::Disabled => false,
            });
            crate::services::codex_tmux_wrapper::run(
                &output_file,
                &input_fifo,
                &prompt_file,
                &cwd,
                &codex_bin,
                codex_model.as_deref(),
                reasoning_effort.as_deref(),
                developer_instructions.as_deref(),
                resume_session_id.as_deref(),
                fast_mode_override,
                goals_override,
                mode,
                compact_token_limit,
                &add_dirs,
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
        Commands::ClaudeHookRelay {
            endpoint,
            provider,
            event,
            session_id,
        } => exit_for_cli(crate::services::claude_tui::hook_relay::run_cli(
            &endpoint,
            &provider,
            &event,
            &session_id,
        )),
        Commands::CodexHookRelay {
            endpoint,
            provider,
            event,
            session_id,
        } => exit_for_cli(crate::services::claude_tui::hook_relay::run_cli(
            &endpoint,
            &provider,
            &event,
            &session_id,
        )),
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
        Commands::InstallMementoSessionHook {
            settings_path,
            dry_run,
            uninstall,
        } => super::utils::handle_install_memento_session_hook(
            settings_path.as_deref(),
            dry_run,
            uninstall,
        )
        .map_err(anyhow::Error::msg),
        Commands::Status => {
            let invoke = super::client::cmd_status(json);
            if json {
                exit_for_json_cli(invoke)
            } else {
                exit_for_cli(invoke)
            }
        }
        Commands::Cards { status } => {
            let invoke = super::client::cmd_cards(status.as_deref(), json);
            if json {
                exit_for_json_cli(invoke)
            } else {
                exit_for_cli(invoke)
            }
        }
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
            let invoke = super::client::cmd_advance(&issue_number, json);
            if json {
                exit_for_json_cli(invoke)
            } else {
                exit_for_cli(invoke)
            }
        }
        Commands::Queue => {
            let invoke = super::client::cmd_queue(json);
            if json {
                exit_for_json_cli(invoke)
            } else {
                exit_for_cli(invoke)
            }
        }
        Commands::Query {
            action,
            filters,
            agent,
            limit,
        } => {
            let section = match action {
                Some(QueryAction::Queue) => super::query::QuerySection::Queue,
                Some(QueryAction::Dispatches) => super::query::QuerySection::Dispatches,
                Some(QueryAction::PhaseGate) => super::query::QuerySection::PhaseGate,
                Some(QueryAction::All) | None => super::query::QuerySection::All,
            };
            let opts_result = super::query::QueryOptions::from_raw(json, filters, agent, limit);
            let invoke = match opts_result {
                Ok(opts) => super::query::cmd_query(section, opts),
                Err(err) => Err(err),
            };
            if json {
                exit_for_json_cli(invoke)
            } else {
                exit_for_cli(invoke)
            }
        }
        Commands::Phase { action, detailed } => {
            // Default + explicit `status` are identical for now; PhaseAction
            // is left as a Subcommand so future verbs (`watch`, `clear`) can
            // attach without breaking call sites.
            let _ = action.unwrap_or(PhaseAction::Status);
            let invoke = super::client::cmd_phase_status(json, detailed);
            if json {
                exit_for_json_cli(invoke)
            } else {
                exit_for_cli(invoke)
            }
        }
        Commands::Deploy => exit_for_cli(super::client::cmd_deploy()),
        Commands::Config { action } => exit_for_cli(match action {
            ConfigAction::Get => super::client::cmd_config_get(),
            ConfigAction::Set { json } => super::client::cmd_config_set(&json),
            ConfigAction::Audit { dry_run } => super::client::cmd_config_audit(dry_run),
            ConfigAction::SyncMcp => super::client::cmd_config_sync_mcp(),
        }),
        Commands::Api { method, path, body } => {
            exit_for_cli(super::client::cmd_api(&method, &path, body.as_deref()))
        }
        Commands::Terminations {
            card_id,
            dispatch_id,
            session,
            limit,
        } => {
            let invoke = super::client::cmd_terminations(
                card_id.as_deref(),
                dispatch_id.as_deref(),
                session.as_deref(),
                limit,
                json,
            );
            if json {
                exit_for_json_cli(invoke)
            } else {
                exit_for_cli(invoke)
            }
        }
        Commands::Doctor {
            fix,
            allow_restart,
            repair_sqlite_cache,
            allow_remote,
            profile,
        } => {
            let profile = match profile {
                Some(DoctorProfileArg::Quick) => {
                    Some(super::doctor::contract::DoctorProfile::Quick)
                }
                Some(DoctorProfileArg::Deep) => Some(super::doctor::contract::DoctorProfile::Deep),
                Some(DoctorProfileArg::Security) => {
                    Some(super::doctor::contract::DoctorProfile::Security)
                }
                None => None,
            };
            let options = super::doctor::DoctorOptions {
                fix,
                json,
                allow_restart,
                repair_sqlite_cache,
                allow_remote,
                profile,
                run_context: super::doctor::contract::RunContext::ManualCli,
                artifact_path: None,
            };
            if json {
                exit_for_json_cli(super::doctor::cmd_doctor(options))
            } else {
                exit_for_cli(super::doctor::cmd_doctor(options))
            }
        }
        Commands::Migrate { action } => exit_for_cli(match action {
            MigrateAction::Openclaw(args) => super::migrate::cmd_migrate_openclaw(args),
        }),
        Commands::ProviderCli(args) => exit_for_cli(super::provider_cli::cmd_provider_cli(args)),
        Commands::Show { action } => exit_for_cli(handle_show(action)),
        Commands::Health => exit_for_cli(super::client::cmd_health(json)),
        Commands::MachineCompare => exit_for_cli(super::client::cmd_machine_compare(json)),
        Commands::Activity {
            since,
            until,
            repo,
            no_agentdesk,
        } => exit_for_cli(super::client::cmd_activity(
            &since,
            until.as_deref(),
            repo.as_deref(),
            json,
            no_agentdesk,
        )),
    }
}

fn handle_show(action: ShowAction) -> std::result::Result<(), String> {
    match action {
        ShowAction::SessionName { channel, provider } => {
            cmd_show_session_name(&channel, provider.as_deref())
        }
    }
}

/// `agentdesk show session-name --channel <id> [--provider <kind>]`.
///
/// Prints the deterministic tmux session name AgentDesk will use for the given
/// channel. Operator-facing: pre-create matching sessions with
/// `tmux new -s "$(agentdesk show session-name --channel <id> --provider <kind>)"`.
///
/// Provider resolution is deliberately *offline-reproducible*:
///   1. explicit `--provider` flag — always wins;
///   2. channel-suffix heuristic when the channel ends in a registered
///      provider suffix (`-cc`/`-cdx`/`-gm`/`-oc`/`-qw`);
///   3. otherwise, error out and require the operator to pass `--provider`.
///
/// We do *not* consult the live agent_bindings table here. That would make
/// the output depend on database state that operators can't see from a
/// terminal — the whole point of the contract is determinism. Discovery /
/// supervisor code (E2/E3) that *does* have the binding directory should call
/// [`crate::services::cluster::session_matcher::expected_session_name_for`]
/// directly.
fn cmd_show_session_name(channel: &str, provider: Option<&str>) -> std::result::Result<(), String> {
    use crate::services::cluster::session_matcher::expected_session_name_for;
    use crate::services::provider::ProviderKind;

    let resolved = match provider {
        Some(raw) => ProviderKind::from_str(raw).ok_or_else(|| {
            format!(
                "unknown provider '{raw}'. supported: {}",
                crate::services::provider::supported_provider_ids().join(", ")
            )
        })?,
        None => ProviderKind::from_channel_suffix(channel).ok_or_else(|| {
            format!(
                "could not infer provider from channel '{channel}' \
                 (no registered suffix). pass --provider <{}>",
                crate::services::provider::supported_provider_ids().join("|")
            )
        })?,
    };

    let session = expected_session_name_for(None, &resolved, channel);
    println!("{session}");
    Ok(())
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
                ReportProvider::OpenCode => ProviderKind::OpenCode,
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

#[cfg(test)]
mod exit_json_tests {
    use super::*;

    #[test]
    fn json_cli_error_line_is_single_line_json() {
        assert_eq!(json_cli_error_line("boom"), r#"{"error":"boom"}"#);
    }

    #[test]
    fn json_cli_error_line_escapes_newlines_and_quotes() {
        let raw = "Request failed: refused\n  힌트: \"x\"";
        let line = json_cli_error_line(raw);
        // Embedded newline is escaped — the payload stays one physical line.
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("\\n"));
        assert!(line.contains("\\\""));
        // Round-trips back to the original message.
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["error"], raw);
    }

    #[test]
    fn command_supports_json_accepts_json_commands() {
        // The 5 dual-mode commands.
        assert!(command_supports_json(&Commands::Status));
        assert!(command_supports_json(&Commands::Queue));
        assert!(command_supports_json(&Commands::Advance {
            issue_number: "42".to_string(),
        }));
        // A representative always-JSON command.
        assert!(command_supports_json(&Commands::Agents));
    }

    #[test]
    fn command_supports_json_rejects_text_only_commands() {
        // The reviewer's repro: `show session-name … --json` printed text.
        assert!(!command_supports_json(&Commands::Show {
            action: ShowAction::SessionName {
                channel: "review-cdx".to_string(),
                provider: None,
            },
        }));
        assert!(!command_supports_json(&Commands::Deploy));
        assert!(!command_supports_json(&Commands::ResetTmux));
    }
}
