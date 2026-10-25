use super::build_slash_help_messages;
use crate::services::discord::commands::command_policy::SLASH_COMMAND_RISKS;
use crate::services::discord::runtime_bootstrap::run_bot_build_slash_commands;
use crate::services::provider::ProviderKind;
use std::collections::BTreeSet;

/// Command names at the start of help lines (`` `/name ...` ``).
fn help_command_names(messages: &[String]) -> BTreeSet<String> {
    messages
        .iter()
        .flat_map(|message| message.lines())
        .filter_map(|line| line.strip_prefix("`/"))
        .map(|rest| {
            let end = rest.find([' ', '`']).unwrap_or(rest.len());
            format!("/{}", &rest[..end])
        })
        .collect()
}

/// The registered slash commands, the `/help` list and the risk table name the
/// same commands, and every help message fits a Discord message.
#[test]
fn slash_registration_help_and_risk_table_name_the_same_commands() {
    let commands = run_bot_build_slash_commands();
    let registered: Vec<String> = commands
        .iter()
        .map(|command| format!("/{}", command.name))
        .collect();
    let registered_set: BTreeSet<String> = registered.iter().cloned().collect();
    assert_eq!(
        registered_set.len(),
        registered.len(),
        "duplicate registration"
    );

    let risk_names: Vec<&str> = SLASH_COMMAND_RISKS.iter().map(|(name, _)| *name).collect();
    let risk_set: BTreeSet<String> = risk_names.iter().map(|name| name.to_string()).collect();
    assert_eq!(risk_set.len(), risk_names.len(), "duplicate risk entry");
    assert_eq!(risk_set, registered_set, "risk table vs registration");

    for provider in [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Gemini,
    ] {
        let messages = build_slash_help_messages(&provider, &commands);
        assert_eq!(
            help_command_names(&messages),
            registered_set,
            "/help vs registration ({provider:?})"
        );
        assert!(
            messages
                .iter()
                .all(|message| message.chars().count() <= 2000)
        );
    }

    for enabled in [false, true] {
        let tiers = super::super::risk_tier_summary_for_help(enabled);
        assert!(tiers.chars().count() <= 2000);
        let tier_names: BTreeSet<String> = tiers
            .split_whitespace()
            .filter(|token| token.starts_with('/'))
            .map(str::to_string)
            .collect();
        assert_eq!(
            tier_names, registered_set,
            "risk tier block vs registration"
        );
    }
}

#[test]
fn help_advertises_explicit_queue_cancellation_not_reaction_removal() {
    let messages =
        build_slash_help_messages(&ProviderKind::Claude, &run_bot_build_slash_commands());
    let body = messages.join("\n");
    assert!(body.contains("`/cancel-queued <message_id>`"));
    assert!(body.contains("`/queue` shows IDs"));
    assert!(!body.contains("reaction remove"));
}

/// Retired slash commands stay out of the registration that the boot-time
/// guild overwrite publishes, so Discord drops them.
#[test]
fn retired_slash_commands_are_not_registered() {
    let registered: BTreeSet<String> = run_bot_build_slash_commands()
        .iter()
        .map(|command| command.name.to_string())
        .collect();
    for retired in [
        "start",
        "receipt",
        "meeting",
        "vc_join",
        "vc_leave",
        "deadlock-recover",
        "machine-flip",
        "stuck-pr-rebase",
    ] {
        assert!(!registered.contains(retired), "/{retired} is registered");
    }
}

/// `/help` lists only the text session controls agents can send today, not
/// the other `!` commands.
#[test]
fn help_lists_only_working_agent_text_commands() {
    let body = build_slash_help_messages(&ProviderKind::Claude, &run_bot_build_slash_commands())
        .join("\n");
    let listed: BTreeSet<&str> = body
        .lines()
        .filter_map(|line| line.strip_prefix("`!"))
        .map(|rest| &rest[..rest.find([' ', '`']).unwrap_or(rest.len())])
        .collect();
    assert_eq!(listed, BTreeSet::from(["clear", "stop"]));
}
