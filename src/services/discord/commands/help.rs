use super::super::{Context, Data, Error};
use super::command_policy::{TEXT_COMMANDS, TextCommandSurface};
use crate::services::provider::ProviderKind;

/// Discord rejects single messages over 2000 characters; keep headroom.
const HELP_MESSAGE_CHAR_BUDGET: usize = 1900;
/// Discord caps slash descriptions at 100 characters.
const HELP_DESCRIPTION_CHAR_LIMIT: usize = 100;
/// Session controls kept as text because agents cannot run slash commands.
const AGENT_TEXT_COMMANDS: [&str; 3] = ["!clear", "!restart", "!stop"];

/// One help line. Descriptions that restate the usage as `/name [args] — text`
/// keep that usage; others get the bare command path.
fn slash_help_line(path: &str, description: Option<&str>) -> String {
    let description = description.unwrap_or("").trim();
    let (usage, text) = match description.trim_start_matches('`').split_once(" — ") {
        Some((usage, text)) if usage.starts_with(path) => {
            (usage.trim_end_matches('`').to_string(), text.trim())
        }
        _ => (path.to_string(), description),
    };
    let text = if text.chars().count() > HELP_DESCRIPTION_CHAR_LIMIT {
        let mut short: String = text.chars().take(HELP_DESCRIPTION_CHAR_LIMIT - 1).collect();
        short.push('…');
        short
    } else {
        text.to_string()
    };
    if text.is_empty() {
        format!("`{usage}`")
    } else {
        format!("`{usage}` — {text}")
    }
}

/// `/help` messages generated from the registered slash commands, each under
/// Discord's message limit. The risk-tier block is sent separately.
fn build_slash_help_messages(
    provider: &ProviderKind,
    commands: &[poise::Command<Data, Error>],
) -> Vec<String> {
    let provider_name = provider.display_name();
    let mut lines = vec![
        "**AgentDesk Discord Bot**".to_string(),
        format!("Chat with {provider_name}. Each channel gets an independent session."),
        format!("Any other message is sent to {provider_name}; send a file/photo to upload it."),
        String::new(),
        "**Slash Commands**".to_string(),
    ];
    for command in commands {
        let path = format!("/{}", command.name);
        lines.push(slash_help_line(&path, command.description.as_deref()));
        for sub in &command.subcommands {
            let sub_path = format!("{path} {}", sub.name);
            lines.push(slash_help_line(&sub_path, sub.description.as_deref()));
        }
    }
    lines.push(String::new());
    lines.push("**Agent Text Commands** (plain messages, for agents)".to_string());
    for name in AGENT_TEXT_COMMANDS {
        let implemented = TEXT_COMMANDS.iter().find(|(registered, id, _)| {
            *registered == name && id.text_surface() == TextCommandSurface::Implemented
        });
        if let Some((_, _, line)) = implemented {
            lines.push((*line).to_string());
        }
    }

    let mut messages = Vec::new();
    let mut current = String::new();
    for line in lines {
        if !current.is_empty() && current.len() + 1 + line.len() > HELP_MESSAGE_CHAR_BUDGET {
            messages.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(&line);
    }
    messages.push(current);
    messages
}

#[cfg(test)]
#[path = "help_tests.rs"]
mod tests;

/// /help — Show every registered slash command and the command risk tiers
#[poise::command(slash_command, rename = "help")]
pub(in crate::services::discord) async fn cmd_help(ctx: Context<'_>) -> Result<(), Error> {
    let messages =
        build_slash_help_messages(&ctx.data().provider, &ctx.framework().options().commands);
    for message in messages {
        ctx.say(message).await?;
    }
    // Sent separately so each message stays under Discord's 2000-char limit.
    let risk_block = super::risk_tier_summary_for_help(super::high_risk_enabled_via_env());
    ctx.say(risk_block).await?;
    Ok(())
}
